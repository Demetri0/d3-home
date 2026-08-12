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

/// How long `send` pumps the transport before returning.
///
/// The session does not surface acknowledgements as an `Action` — an `Ack`
/// silently clears the resend queue internally — so there is no explicit
/// "your command was applied" signal for the client to wait for. This fixed
/// grace period gives a loopback round trip far more time than it needs
/// while keeping calls fast. A command that only fails much later (all five
/// resend attempts exhausted, several seconds out) is not reported by this
/// call; it surfaces the next time something drives the session and hits
/// `Action::Lost` — the next `send`, `collect_state`, or `watch`.
const SEND_SETTLE: Duration = Duration::from_millis(300);

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

    /// Send a command to the device and wait a fixed settle period for the
    /// round trip. See [`SEND_SETTLE`] for why this is a grace period
    /// rather than a wait for an explicit acknowledgement.
    pub fn send(&mut self, cmd: Command) -> Result<(), Error> {
        let now = self.now();
        let actions = self.session.request(cmd, now);
        self.perform(&actions)?;

        let deadline = Instant::now() + SEND_SETTLE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let actions = self.pump(remaining)?;
            for action in &actions {
                if let Action::Lost(reason) = action {
                    return Err(lost_to_error(*reason));
                }
            }
        }
    }

    /// Collect whatever state events arrive within `window` into a
    /// [`DeviceState`]. The protocol has no "query state" command; the
    /// device reports state on its own, so this just listens.
    pub fn collect_state(&mut self, window: Duration) -> Result<DeviceState, Error> {
        let mut state = DeviceState::default();
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
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
