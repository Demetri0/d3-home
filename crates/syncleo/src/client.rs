//! The blocking client: owns the real clock and the transport, and drives
//! the pure [`Session`] state machine by repeatedly receiving a packet (or
//! letting time pass), feeding it to the session, and performing whatever
//! [`Action`]s come back.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use crate::codec::command::{Command, Event, PowerMode};
use crate::error::Error;
use crate::session::{Action, Input, LostReason, Millis, Session};
use crate::transport::Transport;

/// How long a single `recv` call is allowed to block before the client comes
/// back up to check its overall deadline (or, for `watch`, to let the
/// session's own tick-driven timers — resend, ping, silence — run). Short
/// enough to stay responsive, long enough to not busy-loop.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The overall deadline `send` gives a command to be acknowledged.
///
/// The session resends an unacknowledged `Cmd` every `RESEND_INTERVAL_MS`
/// (1s) and gives up after `MAX_ATTEMPTS` (5) attempts — roughly four to
/// five seconds from the first send to `Action::Lost(Unacknowledged)`. This
/// deadline must comfortably outlast that so a genuinely lost command
/// surfaces as the session's own give-up (a specific `Error`) rather than as
/// a premature, less informative client timeout.
const SEND_DEADLINE: Duration = Duration::from_secs(8);

fn lost_to_error(reason: LostReason) -> Error {
    match reason {
        LostReason::HandshakeRejected => Error::HandshakeRejected,
        LostReason::Silence => Error::Silence,
        // The device may still be alive and sending; it has simply stopped
        // acknowledging our commands. From the caller's side that is
        // indistinguishable from "did not respond in time."
        LostReason::Unacknowledged => Error::Timeout,
    }
}

/// Everything the device has told us about its state, accumulated from
/// whatever events arrived during a [`Client::collect_state`] window.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeviceState {
    pub current_temperature: Option<u8>,
    pub target_temperature: Option<u8>,
    pub mode: Option<PowerMode>,
    pub volume: Option<u8>,
    pub error: Option<bool>,
    pub child_lock: Option<bool>,
}

impl DeviceState {
    /// Whether this reading is worth showing a person.
    ///
    /// Mode and current temperature are what a status is actually about;
    /// the flags are context. A state carrying only one of the two is
    /// technically a successful read and practically a shrug.
    pub fn is_informative(&self) -> bool {
        self.mode.is_some() && self.current_temperature.is_some()
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::CurrentTemperature(t) => self.current_temperature = Some(t),
            Event::TargetTemperature(t) => self.target_temperature = Some(t),
            Event::Mode(m) => self.mode = Some(m),
            Event::Volume(v) => self.volume = Some(v),
            Event::Error(b) => self.error = Some(b),
            Event::ChildLock(b) => self.child_lock = Some(b),
            _ => {}
        }
    }
}

/// A blocking connection to one device: real sockets, real clock, driving a
/// pure [`Session`].
pub struct Client {
    transport: Box<dyn Transport>,
    session: Session,
    start: Instant,
}

impl std::fmt::Debug for Client {
    /// The transport is a `Box<dyn Transport>` and has no meaningful debug
    /// representation of its own; this exists only so `Result<Client, _>`
    /// satisfies `expect_err` and friends in tests.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// Perform the handshake and block until the session reports it is
    /// connected, the device rejects the token, or `timeout` elapses.
    pub fn connect(
        transport: Box<dyn Transport>,
        our_private: [u8; 32],
        device_public_wire: [u8; 32],
        token: [u8; 16],
        timeout: Duration,
    ) -> Result<Client, Error> {
        let start = Instant::now();
        let (session, actions) = Session::new(our_private, device_public_wire, token, Millis(0));

        let mut client = Client { transport, session, start };
        client.perform(&actions)?;

        let deadline = start + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            let actions = client.pump(remaining)?;
            for action in &actions {
                match action {
                    Action::Connected => return Ok(client),
                    Action::Lost(reason) => return Err(lost_to_error(*reason)),
                    _ => {}
                }
            }
        }
    }

    /// Send a command to the device and block until it is acknowledged, the
    /// connection is declared lost, or [`SEND_DEADLINE`] elapses.
    pub fn send(&mut self, cmd: Command) -> Result<(), Error> {
        if let Some(err) = self.already_lost() {
            return Err(err);
        }
        let now = self.now();
        let actions = self.session.request(cmd, now);
        self.perform(&actions)?;

        let deadline = Instant::now() + SEND_DEADLINE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            let actions = self.pump(remaining)?;
            for action in &actions {
                match action {
                    Action::Acked(_) => return Ok(()),
                    Action::Nacked(_) => return Err(Error::DeviceNak),
                    Action::Lost(reason) => return Err(lost_to_error(*reason)),
                    _ => {}
                }
            }
        }
    }

    /// Collect whatever state events arrive into a [`DeviceState`], waiting
    /// adaptively rather than for a single fixed window. The protocol has
    /// no "query state" command; the device reports its full state,
    /// unprompted, as a burst of many small events right after the
    /// handshake, so this has two separate jobs: return promptly once that
    /// burst has clearly finished, but not give up too early on a device
    /// that is simply slow to start it.
    ///
    /// `quiet` and `overall` give the two bounds:
    /// - Once at least one event has arrived, this returns as soon as
    ///   `quiet` elapses with no further event -- the burst is many
    ///   messages sent close together, so a gap this long means it is
    ///   done, not merely paused.
    /// - Until the first event arrives, `quiet` does not apply at all;
    ///   this keeps waiting up to `overall`, measured from the start of
    ///   the call, to cover a device that starts its burst late.
    ///
    /// A `DeviceState` with every field still `None` is reported as
    /// [`Error::NoState`] rather than a hollow success: the post-handshake
    /// state burst is this project's own assumption about what a Syncleo
    /// device does, not a documented part of the protocol. If a real
    /// device doesn't send one, or it lands after `overall` closes, this
    /// is the caller's only way to tell "genuinely learned nothing" apart
    /// from "the device really has no water, isn't erroring, and so on" --
    /// both would otherwise print identically as six `unknown` lines with
    /// exit code 0, and a script would have no way to distinguish them. Any
    /// field actually set still counts as a real (if partial) success.
    pub fn collect_state(&mut self, quiet: Duration, overall: Duration) -> Result<DeviceState, Error> {
        if let Some(err) = self.already_lost() {
            return Err(err);
        }
        let mut state = DeviceState::default();
        let overall_deadline = Instant::now() + overall;
        // Set once the first event of any kind arrives; from then on it is
        // pushed forward on every further event, and closing in on it (as
        // opposed to `overall_deadline`) is what lets the common case
        // return early instead of waiting out the full window.
        let mut quiet_deadline: Option<Instant> = None;

        loop {
            let deadline = match quiet_deadline {
                Some(quiet_deadline) => quiet_deadline.min(overall_deadline),
                None => overall_deadline,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if state == DeviceState::default() {
                    return Err(Error::NoState);
                }
                // Going quiet is only permission to stop once the reading is
                // worth having. An idle kettle spreads its burst out with
                // gaps wider than the quiet window, so returning on the first
                // lull hands back a status that is mostly "unknown" -- true,
                // and useless. When the essentials are still missing, keep
                // waiting for the overall deadline instead.
                let out_of_time = Instant::now() >= overall_deadline;
                if state.is_informative() || out_of_time {
                    return Ok(state);
                }
                quiet_deadline = None;
                continue;
            }

            let actions = self.pump(remaining)?;
            let mut heard_something = false;
            for action in actions {
                match action {
                    Action::Emit(event) => {
                        state.apply(event);
                        heard_something = true;
                    }
                    Action::Lost(reason) => return Err(lost_to_error(reason)),
                    _ => {}
                }
            }
            // Any event counts, even one that carries no field this
            // client tracks (diagnostics, hardware info, unknown types):
            // the real device's burst interleaves those with the events
            // that do, and a quiet window that only reset on recognized
            // fields could close mid-burst.
            if heard_something {
                quiet_deadline = Some(Instant::now() + quiet);
            }
        }
    }

    /// Run until `on_event` asks to stop or the connection is lost. Events
    /// are handed to the callback rather than printed, leaving room for
    /// whatever the caller wants to do with them (notifications, logging,
    /// a UI).
    pub fn watch(
        &mut self,
        mut on_event: impl FnMut(Event) -> ControlFlow<()>,
    ) -> Result<(), Error> {
        if let Some(err) = self.already_lost() {
            return Err(err);
        }
        loop {
            let actions = self.pump(POLL_INTERVAL)?;
            for action in actions {
                match action {
                    Action::Emit(event) => {
                        if on_event(event).is_break() {
                            return Ok(());
                        }
                    }
                    Action::Lost(reason) => return Err(lost_to_error(reason)),
                    _ => {}
                }
            }
        }
    }

    /// Whether the session backing this client has already declared itself
    /// dead, translated into the `Error` a caller would eventually see from
    /// `pump` if it kept polling. `None` while the session is still alive.
    ///
    /// Finding 11: every caller in this codebase today drops its `Client`
    /// the moment it sees the *first* `Lost`, so this guard is unreachable
    /// from any of them -- but nothing enforces that a caller must. Once
    /// `dead`, `Session::step`/`request` return no actions at all (by
    /// design: see `Session`'s own doc comments), so without this,
    /// `send`/`collect_state`/`watch` on an already-dead session polled
    /// uselessly until *that call's own* deadline gave up -- 8 seconds for
    /// `send`, `overall` for `collect_state`, forever for `watch` (it has
    /// none of its own). This turns that into an immediate, well-typed
    /// error instead of a silent stall.
    fn already_lost(&self) -> Option<Error> {
        self.session.last_lost().map(lost_to_error)
    }

    fn now(&self) -> Millis {
        Millis(self.start.elapsed().as_millis() as u64)
    }

    fn perform(&mut self, actions: &[Action]) -> Result<(), Error> {
        for action in actions {
            if let Action::Send(bytes) = action {
                self.transport.send(bytes)?;
            }
        }
        Ok(())
    }

    /// One receive-or-tick cycle: wait up to `budget` (capped at
    /// [`POLL_INTERVAL`]) for a packet, feed the session either the packet
    /// or the passage of time, execute any resulting `Send` actions
    /// immediately, and return the full action list for the caller to
    /// interpret.
    fn pump(&mut self, budget: Duration) -> Result<Vec<Action>, Error> {
        let wait = budget.min(POLL_INTERVAL);
        let packet = self.transport.recv(wait)?;
        let now = self.now();
        let actions = match packet {
            Some(bytes) => self.session.step(Input::Packet(bytes), now),
            None => self.session.step(Input::Tick, now),
        };
        self.perform(&actions)?;
        Ok(actions)
    }
}

#[cfg(test)]
impl Client {
    /// Test-only: build a `Client` around an already-constructed `Session`,
    /// skipping the real handshake entirely. Lets a test drive the session
    /// to `Lost` purely on its own virtual clock (instant -- see
    /// `Session`'s module doc comment) and then exercise `Client`'s
    /// already-dead guard without waiting out any real deadline.
    fn from_parts(transport: Box<dyn Transport>, session: Session) -> Client {
        Client { transport, session, start: Instant::now() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport that never delivers anything and never fails to send:
    /// enough to drive a `Client` whose session is already dead, without
    /// any real socket or peer.
    struct NullTransport;

    impl Transport for NullTransport {
        fn send(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn recv(&mut self, _timeout: Duration) -> std::io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    #[test]
    fn send_on_an_already_dead_session_fails_immediately_instead_of_waiting_out_the_deadline() {
        // Finding 11: once a session has declared itself dead, `send`
        // used to have no way to tell that apart from "still waiting" and
        // kept polling for the full 8-second SEND_DEADLINE before
        // reporting a timeout. Reaching `Lost` here costs no real time at
        // all: it's driven entirely on the session's own virtual clock
        // (the 15s silence timeout, fed as a single `Tick` at exactly that
        // offset), not real wall-clock waiting.
        let (mut session, _initial) = Session::new([1; 32], [2; 32], [0xAA; 16], Millis(0));
        let lost = session.step(Input::Tick, Millis(15_000));
        assert!(
            lost.iter().any(|a| matches!(a, Action::Lost(LostReason::Silence))),
            "the session must have declared itself dead by now: {lost:?}"
        );
        assert_eq!(session.last_lost(), Some(LostReason::Silence));

        let mut client = Client::from_parts(Box::new(NullTransport), session);

        let start = Instant::now();
        let err = client.send(Command::Ping).expect_err("a dead session must never accept a new command");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "must fail immediately, not wait out SEND_DEADLINE: took {:?}",
            start.elapsed()
        );
        assert!(matches!(err, Error::Silence), "got {err:?}");
    }

    #[test]
    fn watch_on_an_already_dead_session_fails_immediately() {
        // Same guard, the caller with no deadline of its own at all: a
        // `watch()` call on an already-dead session used to poll forever.
        let (mut session, _initial) = Session::new([1; 32], [2; 32], [0xAA; 16], Millis(0));
        session.step(Input::Tick, Millis(15_000));

        let mut client = Client::from_parts(Box::new(NullTransport), session);

        let start = Instant::now();
        let err = client.watch(|_| ControlFlow::Continue(())).expect_err("must not watch a dead session");
        assert!(start.elapsed() < Duration::from_millis(500), "must fail immediately, took {:?}", start.elapsed());
        assert!(matches!(err, Error::Silence), "got {err:?}");
    }
}
