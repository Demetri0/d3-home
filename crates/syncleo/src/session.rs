//! The session state machine that sequences frames into a conversation.
//!
//! This module owns no sockets and reads no system clock: every call takes the
//! current time as an explicit [`Millis`] argument, incoming bytes arrive as
//! [`Input::Packet`], and everything the caller must do (send bytes, surface an
//! event, notice the connection came up or died) leaves as an [`Action`]. That
//! keeps the whole state machine deterministic and testable without I/O.

use crate::codec::command::{Command, Event};
use crate::codec::crypt::{decrypt_frame, encrypt_frame};
use crate::codec::frame::{Frame, FrameType};
use crate::codec::handshake::handshake_frame;
use crate::codec::keys::{SessionKeys, derive, public_wire};

const RESEND_INTERVAL_MS: u64 = 1_000;
const MAX_ATTEMPTS: u32 = 5;
const PING_INTERVAL_MS: u64 = 3_000;
const SILENCE_TIMEOUT_MS: u64 = 15_000;

/// A point in monotonic time, in milliseconds, supplied by the caller.
///
/// The session never reads a clock itself; every timing decision is made by
/// comparing `Millis` values it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Millis(pub u64);

/// Something happening to the session: a packet arrived, or time passed.
#[derive(Debug, Clone)]
pub enum Input {
    Packet(Vec<u8>),
    Tick,
}

/// Why the session declared the connection lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostReason {
    /// No incoming packet at all for `SILENCE_TIMEOUT_MS`.
    Silence,
    /// The device answered our handshake with a `Nak`: it rejected our token.
    HandshakeRejected,
}

/// Something the session wants the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Bytes to put on the wire.
    Send(Vec<u8>),
    /// A decoded event to surface to whoever is watching the device.
    Emit(Event),
    /// The handshake completed; the session is now connected.
    Connected,
    /// The session is done; no more actions will follow until a new one is built.
    Lost(LostReason),
}

/// An outgoing `Cmd` frame we are waiting on an `Ack` for.
struct Pending {
    seq: u8,
    bytes: Vec<u8>,
    sent_at: Millis,
    attempts: u32,
}

/// The session state machine. See the module docs for the design.
pub struct Session {
    keys: SessionKeys,
    next_seq: u8,
    connected: bool,
    /// Set once the session reaches a terminal state (a `Lost` was emitted).
    /// Further input is ignored rather than re-triggering terminal actions.
    dead: bool,
    pending: Option<Pending>,
    last_incoming: Millis,
    last_ping: Millis,
}

impl Session {
    /// Start a session: derive the shared keys and immediately queue the
    /// handshake, returned as the sole `Action::Send`.
    pub fn new(
        our_private: [u8; 32],
        device_public_wire: [u8; 32],
        token: [u8; 16],
        now: Millis,
    ) -> (Session, Vec<Action>) {
        let keys = derive(&our_private, &device_public_wire);
        let our_public_wire = public_wire(&our_private);

        let mut session = Session {
            keys,
            next_seq: 0,
            connected: false,
            dead: false,
            pending: None,
            last_incoming: now,
            last_ping: now,
        };

        let seq = session.take_seq();
        let bytes = handshake_frame(&session.keys, seq, &our_public_wire, &token).to_bytes();
        session.pending = Some(Pending { seq, bytes: bytes.clone(), sent_at: now, attempts: 1 });

        (session, vec![Action::Send(bytes)])
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    fn take_seq(&mut self) -> u8 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    /// Encrypt and queue an outgoing `Cmd` frame, replacing whatever was
    /// pending before. Returns the bytes to send.
    fn queue_command(&mut self, body: &[u8], now: Millis) -> Vec<u8> {
        let seq = self.take_seq();
        let bytes = encrypt_frame(&self.keys, seq, FrameType::Cmd, body).to_bytes();
        self.pending = Some(Pending { seq, bytes: bytes.clone(), sent_at: now, attempts: 1 });
        bytes
    }

    /// Send a command to the device, tracked for resend until acknowledged.
    pub fn request(&mut self, cmd: Command, now: Millis) -> Vec<Action> {
        if self.dead {
            return Vec::new();
        }
        let bytes = self.queue_command(&cmd.encode(), now);
        vec![Action::Send(bytes)]
    }

    /// Advance the session with either an incoming packet or the passage of time.
    pub fn step(&mut self, input: Input, now: Millis) -> Vec<Action> {
        if self.dead {
            return Vec::new();
        }
        match input {
            Input::Packet(bytes) => self.on_packet(&bytes, now),
            Input::Tick => self.on_tick(now),
        }
    }

    fn on_packet(&mut self, bytes: &[u8], now: Millis) -> Vec<Action> {
        let frame = match Frame::parse(bytes) {
            Ok(frame) => frame,
            // Garbage on the wire: no panic, no state change, nothing to do.
            Err(_) => return Vec::new(),
        };
        let body = match decrypt_frame(&self.keys, &frame) {
            Ok(body) => body,
            Err(_) => return Vec::new(),
        };

        // Any frame that decrypts is proof the device is alive, regardless of
        // its type or content.
        self.last_incoming = now;
        let mut actions = Vec::new();

        match frame.head.ty {
            FrameType::Nak => {
                if !self.connected {
                    // The device rejected our token: this is terminal, there
                    // is no retry that fixes a bad token.
                    self.dead = true;
                    self.pending = None;
                    actions.push(Action::Lost(LostReason::HandshakeRejected));
                }
            }
            FrameType::Ack => {
                if let Some(pending) = &self.pending
                    && pending.seq == frame.head.seq
                {
                    self.pending = None;
                }
            }
            FrameType::Cmd => {
                // Every incoming Cmd gets acknowledged, whether or not we
                // understand its body.
                let ack = encrypt_frame(&self.keys, frame.head.seq, FrameType::Ack, &[]);
                actions.push(Action::Send(ack.to_bytes()));

                if let Ok(event) = Event::decode(&body) {
                    if !self.connected && matches!(event, Event::HandshakeResponse { .. }) {
                        self.connected = true;
                        self.pending = None;
                        self.last_ping = now;
                        actions.push(Action::Connected);
                    }
                    actions.push(Action::Emit(event));
                }
            }
            FrameType::Aux => {}
        }

        actions
    }

    fn on_tick(&mut self, now: Millis) -> Vec<Action> {
        if now.0.saturating_sub(self.last_incoming.0) >= SILENCE_TIMEOUT_MS {
            self.dead = true;
            self.connected = false;
            self.pending = None;
            return vec![Action::Lost(LostReason::Silence)];
        }

        let mut actions = Vec::new();

        if let Some(pending) = &mut self.pending {
            // A pending frame occupies the outgoing slot until it is acked or
            // its retries run out; while it does, no ping is scheduled. This
            // is deliberate: a maxed-out command should not free the slot for
            // a ping to sneak into, since that ping would itself need
            // tracking and retries just like anything else we send.
            if now.0.saturating_sub(pending.sent_at.0) >= RESEND_INTERVAL_MS
                && pending.attempts < MAX_ATTEMPTS
            {
                actions.push(Action::Send(pending.bytes.clone()));
                pending.sent_at = now;
                pending.attempts += 1;
            }
        } else if self.connected && now.0.saturating_sub(self.last_ping.0) >= PING_INTERVAL_MS {
            let bytes = self.queue_command(&Command::Ping.encode(), now);
            self.last_ping = now;
            actions.push(Action::Send(bytes));
        }

        actions
    }
}
