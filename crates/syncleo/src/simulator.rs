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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    pub water: bool,
}

/// A running simulator: an address to point a
/// [`crate::transport::UdpTransport`] at, and the public key the client
/// needs to derive the shared session keys.
pub struct KettleHandle {
    pub addr: SocketAddr,
    pub public_wire: [u8; 32],
    state: Arc<Mutex<SimulatedState>>,
    ignore_commands: Arc<AtomicBool>,
    valid_acks: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl KettleHandle {
    /// Read the simulator's current state. Safe to call from another thread
    /// while the simulator is running: it is read from a shared lock.
    pub fn state(&self) -> SimulatedState {
        *self.state.lock().expect("simulator state lock poisoned")
    }

    /// From now on, silently drop every `Cmd` frame from an established
    /// peer instead of acknowledging it: no `Ack`, no state change. The
    /// handshake itself is unaffected. Exists so tests can pin what happens
    /// when a command is genuinely never delivered, without needing a
    /// separate device that is merely unreachable.
    pub fn ignore_commands(&self) {
        self.ignore_commands.store(true, Ordering::SeqCst);
    }

    /// How many `Ack` frames from the peer this simulator has decrypted and
    /// accepted as genuine (correct type byte, correct padding, decrypted
    /// sequence matches the header). Exists so a test can prove the
    /// client's outgoing acks were actually exercised end-to-end rather
    /// than merely generated and discarded: before this counter existed,
    /// `handle_established` returned early on any non-`Cmd` frame without
    /// even decrypting it.
    pub fn valid_acks(&self) -> usize {
        self.valid_acks.load(Ordering::SeqCst)
    }

    /// Stop the simulator thread and wait for it to exit.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
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
        self.stop.store(true, Ordering::SeqCst);
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

        let state = Arc::new(Mutex::new(SimulatedState {
            mode: PowerMode::Off,
            target: 100,
            current: 20,
            water: true,
        }));
        let ignore_commands = Arc::new(AtomicBool::new(false));
        let valid_acks = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let state = state.clone();
            let ignore_commands = ignore_commands.clone();
            let valid_acks = valid_acks.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                run(socket, token, state, ignore_commands, valid_acks, stop, send_state_burst)
            })
        };

        Ok(KettleHandle {
            addr,
            public_wire: public_wire(&DEVICE_PRIVATE),
            state,
            ignore_commands,
            valid_acks,
            stop,
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

fn run(
    socket: UdpSocket,
    token: [u8; 16],
    state: Arc<Mutex<SimulatedState>>,
    ignore_commands: Arc<AtomicBool>,
    valid_acks: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    send_state_burst: bool,
) {
    let mut peer: Option<Peer> = None;
    let mut buf = [0u8; 2048];

    while !stop.load(Ordering::SeqCst) {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            // Timeout (checked at the top of the loop) or a benign ICMP
            // artifact of loopback UDP: either way, try again.
            Err(_) => continue,
        };
        let Ok(frame) = Frame::parse(&buf[..n]) else { continue };

        match &peer {
            Some(p) if p.addr == from => {
                if !ignore_commands.load(Ordering::SeqCst) {
                    handle_established(&socket, p, &frame, &state, &valid_acks);
                }
            }
            _ => handle_handshake(&socket, from, &frame, token, &mut peer, &state, send_state_burst),
        }
    }
}

/// Try to interpret `frame` as the opening handshake and reply with either
/// a handshake response (token matched) or a `Nak` (it didn't). Anything
/// else arriving before a handshake is ignored.
fn handle_handshake(
    socket: &UdpSocket,
    from: SocketAddr,
    frame: &Frame,
    token: [u8; 16],
    peer: &mut Option<Peer>,
    state: &Arc<Mutex<SimulatedState>>,
    send_state_burst: bool,
) {
    let is_handshake_payload =
        frame.head.ty == FrameType::Cmd && frame.payload.len() == 1 + 32 + 16 && frame.payload[0] == 0x00;
    if !is_handshake_payload {
        return;
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

    let keys = SessionKeys { inkey: raw.outkey, outkey: raw.inkey };

    if block != token {
        let nak = encrypt_frame(&keys, 0, FrameType::Nak, &[]).to_bytes();
        let _ = socket.send_to(&nak, from);
        return;
    }

    let mode_byte = state.lock().expect("state lock poisoned").mode.as_u8();
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

    if send_state_burst {
        report_state_burst(socket, from, &keys, state);
    }

    *peer = Some(Peer { addr: from, keys });
}

/// Report full current state, unprompted, right after the handshake. This
/// is how a client ever learns anything at all: the protocol has no "query
/// state" command, only asynchronous reports from the device.
fn report_state_burst(socket: &UdpSocket, to: SocketAddr, keys: &SessionKeys, state: &Arc<Mutex<SimulatedState>>) {
    let snapshot = *state.lock().expect("state lock poisoned");
    let reports: [(u8, Vec<u8>); 4] = [
        (1, vec![ty::MODE, snapshot.mode.as_u8()]),
        (2, vec![ty::TARGET_TEMPERATURE, snapshot.target, 0]),
        (3, vec![ty::CURRENT_TEMPERATURE, snapshot.current, 0]),
        (4, vec![ty::WATER, snapshot.water as u8]),
    ];
    for (seq, body) in reports {
        let frame = encrypt_frame(keys, seq, FrameType::Cmd, &body).to_bytes();
        let _ = socket.send_to(&frame, to);
    }
}

/// Handle a frame from an already-handshaken peer.
///
/// A `Cmd` is acknowledged and, if it decodes into a command we track,
/// applied to the state -- unchanged from before. An `Ack` is the peer
/// acknowledging one of *our* outgoing frames (the handshake response, or a
/// state-burst report): decrypted and counted as valid rather than dropped
/// unread, so the end-to-end tests actually exercise the client's ack
/// framing instead of merely generating and discarding it. Matching on
/// `frame.head.ty` up front (rather than, say, "try to decrypt it as an
/// Ack and see if that succeeds") matters here specifically: `Ack` and
/// `Nak` carry identical ciphertext for a given sequence, so a check that
/// only asked "does this decrypt cleanly" could not tell the two apart.
fn handle_established(
    socket: &UdpSocket,
    peer: &Peer,
    frame: &Frame,
    state: &Arc<Mutex<SimulatedState>>,
    valid_acks: &AtomicUsize,
) {
    match frame.head.ty {
        FrameType::Cmd => {
            let Ok(body) = decrypt_frame(&peer.keys, frame) else { return };

            let ack = encrypt_frame(&peer.keys, frame.head.seq, FrameType::Ack, &[]).to_bytes();
            let _ = socket.send_to(&ack, peer.addr);

            if let Ok(event) = Event::decode(&body) {
                let mut s = state.lock().expect("state lock poisoned");
                match event {
                    Event::Mode(m) => s.mode = m,
                    Event::TargetTemperature(t) => s.target = t,
                    _ => {}
                }
            }
        }
        FrameType::Ack => {
            if decrypt_frame(&peer.keys, frame).is_ok() {
                valid_acks.fetch_add(1, Ordering::SeqCst);
            }
        }
        FrameType::Aux | FrameType::Nak => {}
    }
}
