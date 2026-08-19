//! The device side of the protocol, running in-process.
//!
//! This is not test scaffolding: without a physical kettle on the desk,
//! this is the only way to develop against the protocol at all. It listens
//! on a real UDP socket, verifies the handshake token, and thereafter
//! behaves like a (deliberately minimal) kettle: it acknowledges commands,
//! applies them to its own state, and reports that state back.
//!
//! Session keys are derived exactly as [`crate::session::Session`] derives
//! them, then used with `inkey`/`outkey` swapped: the client encrypts with
//! `outkey` and decrypts with `inkey`, so the device — sitting on the other
//! end of the same shared secret — must do the reverse. See
//! `crates/syncleo/tests/session.rs`'s `device_keys()` for the same pattern
//! on the pure-session side.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aes::Aes128;
use cbc::Decryptor;
use cipher::{BlockModeDecrypt, KeyIvInit};

use crate::codec::command::{Event, PowerMode, ty};
use crate::codec::crypt::{decrypt_frame, encrypt_frame};
use crate::codec::frame::{Frame, FrameType};
use crate::codec::keys::{SessionKeys, derive, public_wire};

/// How often the simulator wakes up to check for a shutdown request while
/// otherwise idle. Bounds how long `KettleHandle::shutdown` can take.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Fixed, obviously-a-placeholder private key. The simulator only ever
/// talks over loopback for local development and tests, so there is no
/// reason for it to vary between runs, and it is not a real device secret.
const DEVICE_PRIVATE: [u8; 32] = [0x42; 32];

/// Firmware identity reported in the handshake response. Arbitrary: nothing
/// in the protocol depends on these specific numbers, only that they parse.
const PROTOCOL_VERSION: u16 = 2;
const FW_MAJOR: u8 = 1;
const FW_MINOR: u8 = 0;

/// The kettle's state, as tracked by the simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulatedState {
    pub mode: PowerMode,
    pub target: u8,
    pub current: u8,
    /// The raw byte the device sends for code 9 (`Event::Volume`). Set to
    /// a value other than 0 or 1 on purpose: the old `WaterPresent(bool)`
    /// decode collapsed anything through `== 1`, so a simulator that only
    /// ever sent 0 or 1 could never have exercised -- let alone caught --
    /// that bug. Tests reading this back exercise the same non-boolean
    /// case a real device showed.
    pub volume: u8,
}

/// Everything the background thread and its `KettleHandle` both touch,
/// bundled into one `Arc` so `run`/`handle_established`/`handle_handshake`
/// take a single shared reference instead of a growing list of
/// individually-threaded flags.
struct Shared {
    /// Sequence numbers for frames the device sends of its own accord.
    /// Starts clear of the post-handshake burst, which numbers itself 1..6.
    next_seq: AtomicU8,
    state: Mutex<SimulatedState>,
    /// From now on, silently drop every `Cmd` frame from an established
    /// peer instead of acknowledging it. See `KettleHandle::ignore_commands`.
    ignore_commands: AtomicBool,
    /// How many established `Cmd` frames have been decrypted so far,
    /// counting from 0. Compared against `ack_limit` to implement
    /// `ignore_commands_after`.
    commands_seen: AtomicUsize,
    /// `commands_seen` values `>= ack_limit` are silently dropped instead
    /// of acknowledged, exactly like `ignore_commands` but only once this
    /// many commands have already gone through normally. Defaults to
    /// `usize::MAX` (never triggers). See `KettleHandle::ignore_commands_after`.
    ack_limit: AtomicUsize,
    /// From now on, answer every `Cmd` frame from an established peer with
    /// a `Nak`. See `KettleHandle::reject_commands`.
    reject_commands: AtomicBool,
    /// How many `Ack` frames from the peer have been decrypted and
    /// accepted as genuine. See `KettleHandle::valid_acks`.
    valid_acks: AtomicUsize,
    /// Milliseconds to hold the post-handshake state burst back by, `0`
    /// meaning "send it immediately" (the default). See
    /// `KettleHandle::delay_state_burst`.
    burst_delay_ms: AtomicU64,
    /// While `Some` and not yet elapsed, every incoming frame -- including
    /// a fresh handshake attempt -- is silently dropped, exactly as if the
    /// device had no power at all. See `KettleHandle::vanish_for`.
    silent_until: Mutex<Option<Instant>>,
    /// A heat in progress, left here by `KettleHandle::boil_to` for the run
    /// loop to perform. See [`Climb`].
    climb: Mutex<Option<Climb>>,
    stop: AtomicBool,
}

/// A heat in progress: the device climbing towards a target and switching
/// itself off on arrival.
///
/// The handle cannot send anything itself -- the socket and the peer it is
/// talking to live on the run loop's stack, so that one thread owns the wire
/// -- so a heat is left here as an intention and performed by the loop, the
/// same way `delay_state_burst` leaves a burst to be sent later.
struct Climb {
    target: u8,
    step: Duration,
    next_due: Instant,
    /// Whether the target and the switch to heating have been reported yet.
    announced: bool,
}

/// How long a simulated degree takes. Fast enough that a test does not wait
/// on a kettle, slow enough that the climb arrives as a stream of separate
/// reports rather than one indistinguishable burst.
const CLIMB_STEP: Duration = Duration::from_millis(20);

/// A running simulator: an address to point a
/// [`crate::transport::UdpTransport`] at, and the public key the client
/// needs to derive the shared session keys.
pub struct KettleHandle {
    pub addr: SocketAddr,
    pub public_wire: [u8; 32],
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl KettleHandle {
    /// Read the simulator's current state. Safe to call from another thread
    /// while the simulator is running: it is read from a shared lock.
    pub fn state(&self) -> SimulatedState {
        *self
            .shared
            .state
            .lock()
            .expect("simulator state lock poisoned")
    }

    /// Drive a heat: switch to heating, climb to `target` a degree at a
    /// time as the real device does, then switch off on arrival.
    ///
    /// Returns immediately. The heat is carried out by the simulator's own
    /// thread, the only one holding the socket, and every degree is reported
    /// the way the device reports it -- so a client sees the same stream it
    /// would see from a kettle on a worktop, including the unbidden
    /// `mode: off` that ends it.
    pub fn boil_to(&self, target: u8) {
        *self.shared.climb.lock().expect("climb lock poisoned") = Some(Climb {
            target,
            step: CLIMB_STEP,
            next_due: Instant::now(),
            announced: false,
        });
    }

    /// From now on, silently drop every `Cmd` frame from an established
    /// peer instead of acknowledging it: no `Ack`, no state change. The
    /// handshake itself is unaffected. Exists so tests can pin what happens
    /// when a command is genuinely never delivered, without needing a
    /// separate device that is merely unreachable.
    pub fn ignore_commands(&self) {
        self.shared.ignore_commands.store(true, Ordering::SeqCst);
    }

    /// Acknowledge (and apply) the next `count` `Cmd` frames from an
    /// established peer exactly as normal, then silently drop every one
    /// after that -- exactly like `ignore_commands`, but only takes effect
    /// once `count` commands have already gone through cleanly. The
    /// handshake itself is unaffected and never counts.
    ///
    /// Exists to test a multi-command sequence (like `start <temperature>`
    /// sending both `TargetTemperature` and `Mode(Custom)`) where an
    /// earlier command's ack must arrive but a later one's must not,
    /// without needing a real dropped packet or a timing race.
    pub fn ignore_commands_after(&self, count: usize) {
        self.shared.ack_limit.store(count, Ordering::SeqCst);
    }

    /// From now on, answer every `Cmd` frame from an established peer with
    /// a `Nak` instead of an `Ack`, and apply no state change. The
    /// handshake itself is unaffected. Exists so tests can exercise a real
    /// device clearly rejecting a command post-handshake (e.g. hardware
    /// probing its real temperature bounds finding one out of range)
    /// without hand-crafting frames.
    pub fn reject_commands(&self) {
        self.shared.reject_commands.store(true, Ordering::SeqCst);
    }

    /// From now on, hold the post-handshake state burst back by `delay`
    /// instead of sending it immediately after the handshake response.
    /// The handshake itself is unaffected -- only the burst that normally
    /// follows it right away. Exists so tests can pin what happens when a
    /// real device is slow to start its burst, without needing the real
    /// hardware. Must be called before the client performs its handshake
    /// (i.e. before `Client::connect`), the same as `ignore_commands` and
    /// `reject_commands` must be called before the command they affect.
    pub fn delay_state_burst(&self, delay: Duration) {
        self.shared
            .burst_delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// From now on, silently drop every incoming frame -- including a
    /// fresh handshake attempt from a new peer -- for `duration`, then
    /// resume answering normally on its own. No reply of any kind goes
    /// out while vanished: no ack, no nak, nothing, exactly what lifting a
    /// real kettle off its base looks like from the network's side. Unlike
    /// `shutdown`, the socket stays bound to the same address throughout,
    /// so a test can prove a client's reconnect logic finds the device
    /// again at the same cached endpoint without needing real discovery.
    pub fn vanish_for(&self, duration: Duration) {
        let until = Instant::now() + duration;
        *self
            .shared
            .silent_until
            .lock()
            .expect("silent_until lock poisoned") = Some(until);
    }

    /// How many `Ack` frames from the peer this simulator has decrypted and
    /// accepted as genuine (correct type byte, correct padding, decrypted
    /// sequence matches the header). Exists so a test can prove the
    /// client's outgoing acks were actually exercised end-to-end rather
    /// than merely generated and discarded: before this counter existed,
    /// `handle_established` returned early on any non-`Cmd` frame without
    /// even decrypting it.
    pub fn valid_acks(&self) -> usize {
        self.shared.valid_acks.load(Ordering::SeqCst)
    }

    /// Stop the simulator thread and wait for it to exit.
    pub fn shutdown(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for KettleHandle {
    /// A safety net for tests that panic before calling `shutdown`: signal
    /// the thread to stop so it does not outlive the test. Does not join —
    /// blocking in `drop` (possibly during an unwind) is worse than a
    /// thread that exits a moment later on its own.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

/// The device side of the protocol.
pub struct KettleSimulator;

impl KettleSimulator {
    /// Bind to an ephemeral loopback port and start serving in a background
    /// thread. The device only accepts a handshake whose token matches
    /// `token`.
    pub fn spawn(token: [u8; 16]) -> std::io::Result<KettleHandle> {
        Self::spawn_with(token, true)
    }

    /// Like [`spawn`], but the device never sends its post-handshake state
    /// burst. Exists to test what a real device that doesn't send one
    /// looks like from the client's side: the burst is this project's own
    /// assumption about how a Syncleo device behaves, not a documented
    /// part of the protocol (see [`crate::error::Error::NoState`]).
    pub fn spawn_silent(token: [u8; 16]) -> std::io::Result<KettleHandle> {
        Self::spawn_with(token, false)
    }

    fn spawn_with(token: [u8; 16], send_state_burst: bool) -> std::io::Result<KettleHandle> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_read_timeout(Some(POLL_INTERVAL))?;
        let addr = socket.local_addr()?;

        let shared = Arc::new(Shared {
            next_seq: AtomicU8::new(100),
            state: Mutex::new(SimulatedState {
                mode: PowerMode::Off,
                target: 100,
                current: 20,
                volume: 42,
            }),
            ignore_commands: AtomicBool::new(false),
            commands_seen: AtomicUsize::new(0),
            ack_limit: AtomicUsize::new(usize::MAX),
            reject_commands: AtomicBool::new(false),
            valid_acks: AtomicUsize::new(0),
            burst_delay_ms: AtomicU64::new(0),
            silent_until: Mutex::new(None),
            climb: Mutex::new(None),
            stop: AtomicBool::new(false),
        });

        let thread = {
            let shared = shared.clone();
            thread::spawn(move || run(socket, token, &shared, send_state_burst))
        };

        Ok(KettleHandle {
            addr,
            public_wire: public_wire(&DEVICE_PRIVATE),
            shared,
            thread: Some(thread),
        })
    }
}

/// The one client currently talking to us, from the device's point of view:
/// where to reply, and the keys to encrypt/decrypt with. The simulator only
/// ever serves one peer at a time, which is all these tests need.
struct Peer {
    addr: SocketAddr,
    keys: SessionKeys,
}

/// A state burst that has been held back by `delay_state_burst` and is
/// waiting for its moment, tracked entirely on the run loop's own stack --
/// nothing here needs to be shared with another thread.
struct PendingBurst {
    due: Instant,
    to: SocketAddr,
    keys: SessionKeys,
}

fn run(socket: UdpSocket, token: [u8; 16], shared: &Shared, send_state_burst: bool) {
    let mut peer: Option<Peer> = None;
    let mut pending_burst: Option<PendingBurst> = None;
    let mut buf = [0u8; 2048];

    while !shared.stop.load(Ordering::SeqCst) {
        // Checked once per iteration -- `recv_from`'s own read timeout
        // (`POLL_INTERVAL`) upper-bounds how late this can fire relative
        // to `due`, the same way the rest of this loop is timeout-driven
        // rather than needing a second thread.
        if let Some(burst) = &pending_burst
            && Instant::now() >= burst.due
            && !is_silent(shared)
        {
            report_state_burst(&socket, burst.to, &burst.keys, shared);
            pending_burst = None;
        }

        // Likewise checked once per iteration: a heat left by `boil_to` is
        // performed here, on the thread that owns the socket.
        if let Some(p) = &peer
            && !is_silent(shared)
        {
            advance_climb(&socket, p, shared);
        }

        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            // Timeout (checked at the top of the loop) or a benign ICMP
            // artifact of loopback UDP: either way, try again.
            Err(_) => continue,
        };

        if is_silent(shared) {
            // "Off its base": nothing goes out, not even a nak, and a
            // frame that arrives during this window is simply lost --
            // exactly as it would be if the device had no power to
            // receive it with.
            continue;
        }

        let Ok(frame) = Frame::parse(&buf[..n]) else {
            continue;
        };

        match &peer {
            Some(p) if p.addr == from => {
                if !shared.ignore_commands.load(Ordering::SeqCst) {
                    handle_established(&socket, p, &frame, shared);
                }
            }
            _ => {
                pending_burst = handle_handshake(
                    &socket,
                    from,
                    &frame,
                    token,
                    &mut peer,
                    shared,
                    send_state_burst,
                );
            }
        }
    }
}

/// Whether `vanish_for`'s window is still in effect. Left set after it
/// elapses rather than cleared -- checking `Instant::now()` against a
/// stale `Some(until)` in the past is exactly as cheap as checking a bool,
/// and there is no second caller for whom "still `Some`" would mean
/// anything different from "expired."
fn is_silent(shared: &Shared) -> bool {
    match *shared
        .silent_until
        .lock()
        .expect("silent_until lock poisoned")
    {
        Some(until) => Instant::now() < until,
        None => false,
    }
}

/// Try to interpret `frame` as the opening handshake and reply with either
/// a handshake response (token matched) or a `Nak` (it didn't). Anything
/// else arriving before a handshake is ignored.
///
/// Returns a [`PendingBurst`] when `delay_state_burst` has set a nonzero
/// delay: the caller (the run loop) is responsible for sending it once due.
/// With no delay configured, the burst goes out immediately, exactly as
/// before, and this returns `None`.
fn handle_handshake(
    socket: &UdpSocket,
    from: SocketAddr,
    frame: &Frame,
    token: [u8; 16],
    peer: &mut Option<Peer>,
    shared: &Shared,
    send_state_burst: bool,
) -> Option<PendingBurst> {
    let is_handshake_payload = frame.head.ty == FrameType::Cmd
        && frame.payload.len() == 1 + 32 + 16
        && frame.payload[0] == 0x00;
    if !is_handshake_payload {
        return None;
    }

    let client_public_wire: [u8; 32] = frame.payload[1..33].try_into().expect("checked length");
    let mut block: [u8; 16] = frame.payload[33..49].try_into().expect("checked length");

    // The shared secret is symmetric: deriving it from our private key and
    // the client's public key gives back the identical raw keys the client
    // used to encrypt the token. No swap for this one manual block — the
    // swap is a convention for the in/out roles used by encrypt_frame /
    // decrypt_frame, not part of key derivation itself.
    let raw = derive(&DEVICE_PRIVATE, &client_public_wire);
    Decryptor::<Aes128>::new(&raw.outkey.into(), &raw.inkey.into())
        .decrypt_blocks(core::slice::from_mut((&mut block).into()));

    let keys = SessionKeys {
        inkey: raw.outkey,
        outkey: raw.inkey,
    };

    if block != token {
        let nak = encrypt_frame(&keys, 0, FrameType::Nak, &[]).to_bytes();
        let _ = socket.send_to(&nak, from);
        return None;
    }

    let mode_byte = shared
        .state
        .lock()
        .expect("state lock poisoned")
        .mode
        .as_u8();
    let body = vec![
        ty::HANDSHAKE,
        PROTOCOL_VERSION as u8,
        (PROTOCOL_VERSION >> 8) as u8,
        FW_MAJOR,
        FW_MINOR,
        mode_byte,
    ];
    let response = encrypt_frame(&keys, 0, FrameType::Cmd, &body).to_bytes();
    let _ = socket.send_to(&response, from);

    let pending = if !send_state_burst {
        None
    } else {
        let delay_ms = shared.burst_delay_ms.load(Ordering::SeqCst);
        if delay_ms == 0 {
            report_state_burst(socket, from, &keys, shared);
            None
        } else {
            Some(PendingBurst {
                due: Instant::now() + Duration::from_millis(delay_ms),
                to: from,
                keys: keys.clone(),
            })
        }
    };

    *peer = Some(Peer { addr: from, keys });
    pending
}

/// Report full current state, unprompted, right after the handshake. This
/// is how a client ever learns anything at all: the protocol has no "query
/// state" command, only asynchronous reports from the device.
///
/// The last two reports (hardware, diagnostic) mirror what the real kettle
/// sends unprompted in the same burst: a fixed `1.1.4` hardware version (the
/// exact value confirmed against the real device's vendor app) and a
/// diagnostic blob shaped like the real one -- a 20-byte header followed by
/// four 4-byte ASCII tag / 4-byte little-endian value pairs, 52 bytes total
/// (see the design spec's code-145 row). Both are fixed, not part of
/// `SimulatedState`: nothing in this project reads or acts on either, so
/// there is nothing for a test to configure.
/// Move a heat along, if one is due. Called once per run-loop iteration, so
/// the poll interval bounds how late a degree can be; several degrees may
/// fall due within a single iteration, and all of them are reported.
fn advance_climb(socket: &UdpSocket, peer: &Peer, shared: &Shared) {
    let mut guard = shared.climb.lock().expect("climb lock poisoned");
    // Taken out for the duration: putting it back is what keeps the heat
    // going, and the arrival below simply does not.
    let Some(mut climb) = guard.take() else {
        return;
    };

    if !climb.announced {
        climb.announced = true;
        {
            let mut state = shared.state.lock().expect("state lock poisoned");
            state.target = climb.target;
            state.mode = PowerMode::Custom;
        }
        report(
            socket,
            peer,
            shared,
            vec![ty::TARGET_TEMPERATURE, climb.target, 0],
        );
        report(
            socket,
            peer,
            shared,
            vec![ty::MODE, PowerMode::Custom.as_u8()],
        );
    }

    while Instant::now() >= climb.next_due {
        climb.next_due += climb.step;
        let current = {
            let mut state = shared.state.lock().expect("state lock poisoned");
            if state.current < climb.target {
                state.current += 1;
            }
            state.current
        };
        report(
            socket,
            peer,
            shared,
            vec![ty::CURRENT_TEMPERATURE, current, 0],
        );

        if current >= climb.target {
            shared.state.lock().expect("state lock poisoned").mode = PowerMode::Off;
            report(socket, peer, shared, vec![ty::MODE, PowerMode::Off.as_u8()]);
            return;
        }
    }

    *guard = Some(climb);
}

/// Send one unbidden report to the peer, as the device does when its state
/// changes of its own accord.
fn report(socket: &UdpSocket, peer: &Peer, shared: &Shared, body: Vec<u8>) {
    let seq = next_seq(shared);
    let frame = encrypt_frame(&peer.keys, seq, FrameType::Cmd, &body).to_bytes();
    let _ = socket.send_to(&frame, peer.addr);
}

fn report_state_burst(socket: &UdpSocket, to: SocketAddr, keys: &SessionKeys, shared: &Shared) {
    let snapshot = *shared.state.lock().expect("state lock poisoned");

    let mut diagnostic = vec![ty::DIAGNOSTIC];
    diagnostic.extend_from_slice(&[0u8; 20]); // header: contents unknown, only the length is
    for (tag, value) in [
        (*b"udps", 1u32),
        (*b"IDLE", 2u32),
        (*b"Tmr\0", 3u32),
        (*b"rtT\0", 4u32),
    ] {
        diagnostic.extend_from_slice(&tag);
        diagnostic.extend_from_slice(&value.to_le_bytes());
    }

    let reports: [(u8, Vec<u8>); 6] = [
        (1, vec![ty::MODE, snapshot.mode.as_u8()]),
        (2, vec![ty::TARGET_TEMPERATURE, snapshot.target, 0]),
        (3, vec![ty::CURRENT_TEMPERATURE, snapshot.current, 0]),
        (4, vec![ty::VOLUME, snapshot.volume]),
        (5, vec![ty::HARDWARE, 1, 1, 4]),
        (6, diagnostic),
    ];
    for (seq, body) in reports {
        let frame = encrypt_frame(keys, seq, FrameType::Cmd, &body).to_bytes();
        let _ = socket.send_to(&frame, to);
    }
}

/// Handle a frame from an already-handshaken peer.
///
/// A `Cmd` is acknowledged (or, with `reject_commands` set, NAK'd) and, if
/// acknowledged and it decodes into a command we track, applied to the
/// state. An `Ack` is the peer acknowledging one of *our* outgoing frames
/// (the handshake response, or a state-burst report): decrypted and
/// counted as valid rather than dropped unread, so the end-to-end tests
/// actually exercise the client's ack framing instead of merely generating
/// and discarding it. Matching on `frame.head.ty` up front (rather than,
/// say, "try to decrypt it as an Ack and see if that succeeds") matters
/// here specifically: `Ack` and `Nak` carry identical ciphertext for a
/// given sequence, so a check that only asked "does this decrypt cleanly"
/// could not tell the two apart.
/// A sequence number for an unbidden report, wrapping clear of the burst's
/// own fixed numbering.
fn next_seq(shared: &Shared) -> u8 {
    let seq = shared.next_seq.fetch_add(1, Ordering::SeqCst);
    if seq < 100 { 100 } else { seq }
}

fn handle_established(socket: &UdpSocket, peer: &Peer, frame: &Frame, shared: &Shared) {
    match frame.head.ty {
        FrameType::Cmd => {
            let Ok(body) = decrypt_frame(&peer.keys, frame) else {
                return;
            };

            let seen = shared.commands_seen.fetch_add(1, Ordering::SeqCst);
            if seen >= shared.ack_limit.load(Ordering::SeqCst) {
                // Silently drop, exactly like `ignore_commands`, but only
                // once `ignore_commands_after`'s count has been reached:
                // no ack, no nak, no state change.
                return;
            }

            if shared.reject_commands.load(Ordering::SeqCst) {
                let nak = encrypt_frame(&peer.keys, frame.head.seq, FrameType::Nak, &[]).to_bytes();
                let _ = socket.send_to(&nak, peer.addr);
                return;
            }

            let ack = encrypt_frame(&peer.keys, frame.head.seq, FrameType::Ack, &[]).to_bytes();
            let _ = socket.send_to(&ack, peer.addr);

            if let Ok(event) = Event::decode(&body) {
                let echo = {
                    let mut s = shared.state.lock().expect("state lock poisoned");
                    match event {
                        Event::Mode(m) => {
                            s.mode = m;
                            // `On` is "boil", and the device treats it as
                            // setting the target to 100 as well -- observed
                            // on the real kettle, where a bare start after
                            // an earlier `start 45` reported a target of 100.
                            if m == PowerMode::On {
                                s.target = 100;
                            }
                            Some(vec![ty::MODE, m.as_u8()])
                        }
                        Event::TargetTemperature(t) => {
                            s.target = t;
                            Some(vec![ty::TARGET_TEMPERATURE, t, 0])
                        }
                        _ => None,
                    }
                };
                // The real device reports a change once it has taken effect
                // -- a boil ending shows up as an unbidden `mode: off` -- and
                // that report is the only way a caller can tell "the frame
                // arrived" from "the kettle agreed". Without it here, the
                // simulator would let a command look accepted that a real
                // kettle might have quietly ignored.
                if let Some(body) = echo {
                    let seq = next_seq(shared);
                    let frame = encrypt_frame(&peer.keys, seq, FrameType::Cmd, &body).to_bytes();
                    let _ = socket.send_to(&frame, peer.addr);

                    // A mode change can move the target with it; report the
                    // target too, so a reader is not left with a stale one.
                    if body[0] == ty::MODE {
                        let target = shared.state.lock().expect("state lock poisoned").target;
                        let seq = next_seq(shared);
                        let body = vec![ty::TARGET_TEMPERATURE, target, 0];
                        let frame =
                            encrypt_frame(&peer.keys, seq, FrameType::Cmd, &body).to_bytes();
                        let _ = socket.send_to(&frame, peer.addr);
                    }
                }
            }
        }
        FrameType::Ack => {
            if decrypt_frame(&peer.keys, frame).is_ok() {
                shared.valid_acks.fetch_add(1, Ordering::SeqCst);
            }
        }
        FrameType::Aux | FrameType::Nak => {}
    }
}
