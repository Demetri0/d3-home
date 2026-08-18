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
    pub water_present: Option<bool>,
    pub error: Option<bool>,
    pub child_lock: Option<bool>,
}

impl DeviceState {
    fn apply(&mut self, event: Event) {
        match event {
            Event::CurrentTemperature(t) => self.current_temperature = Some(t),
            Event::TargetTemperature(t) => self.target_temperature = Some(t),
            Event::Mode(m) => self.mode = Some(m),
            Event::WaterPresent(b) => self.water_present = Some(b),
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
                    Action::Lost(reason) => return Err(lost_to_error(*reason)),
                    _ => {}
                }
            }
        }
    }

    /// Collect whatever state events arrive within `window` into a
    /// [`DeviceState`]. The protocol has no "query state" command; the
    /// device reports state on its own, so this just listens.
    ///
    /// A `DeviceState` with every field still `None` is reported as
    /// [`Error::NoState`] rather than a hollow success: the post-handshake
    /// state burst is this project's own assumption about what a Syncleo
    /// device does, not a documented part of the protocol. If a real
    /// device doesn't send one, or it lands after `window` closes, this is
    /// the caller's only way to tell "genuinely learned nothing" apart
    /// from "the device really has no water, isn't erroring, and so on" --
    /// both would otherwise print identically as six `unknown` lines with
    /// exit code 0, and a script would have no way to distinguish them. Any
    /// field actually set still counts as a real (if partial) success.
    pub fn collect_state(&mut self, window: Duration) -> Result<DeviceState, Error> {
        let mut state = DeviceState::default();
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if state == DeviceState::default() {
                    return Err(Error::NoState);
                }
                return Ok(state);
            }
            let actions = self.pump(remaining)?;
            for action in actions {
                match action {
                    Action::Emit(event) => state.apply(event),
                    Action::Lost(reason) => return Err(lost_to_error(reason)),
                    _ => {}
                }
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
