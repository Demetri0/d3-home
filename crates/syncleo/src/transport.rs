//! The [`Transport`] abstraction and its real UDP implementation.
//!
//! Everything below this module (the session, the codec) is free of
//! sockets; this is where bytes actually cross a wire boundary.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

/// A byte pipe to a single device: opaque frames in, opaque frames out.
///
/// `recv` returning `Ok(None)` means the timeout elapsed with nothing to
/// report — that is not an error, just "no packet arrived in time."
pub trait Transport {
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<Vec<u8>>>;
}

/// A UDP socket connected to a single peer.
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Bind an ephemeral local port and connect it to `addr`. `connect` on a
    /// UDP socket does not perform a handshake; it just fixes the peer that
    /// `send`/`recv` talk to.
    pub fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        let bind_addr: SocketAddr =
            if addr.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
        let socket = UdpSocket::bind(bind_addr)?;
        socket.connect(addr)?;
        Ok(Self { socket })
    }
}

impl Transport for UdpTransport {
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.socket.send(bytes)?;
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<Vec<u8>>> {
        if timeout.is_zero() {
            // A zero read timeout is rejected by the socket API as
            // ambiguous with "block forever"; treat it as "no time left".
            return Ok(None);
        }
        self.socket.set_read_timeout(Some(timeout))?;
        let mut buf = [0u8; 2048];
        match self.socket.recv(&mut buf) {
            Ok(n) => Ok(Some(buf[..n].to_vec())),
            Err(e) if is_no_data(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Treat both "nothing arrived before the deadline" and the ICMP-triggered
/// `ECONNREFUSED`/`ECONNRESET` that a *connected* UDP socket raises on
/// Linux/BSD when nothing is listening at the peer address as "no packet
/// this round" rather than a fatal transport error. From the caller's point
/// of view both mean the same thing: keep waiting, or give up once its own
/// deadline elapses.
///
/// Note: swallowing the ICMP error here means a persistently unreachable
/// peer on some platforms could make `recv` return `Ok(None)` immediately
/// on every call instead of actually waiting out `timeout`, turning the
/// caller's poll loop into a busy loop until its own deadline elapses.
fn is_no_data(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), WouldBlock | TimedOut | ConnectionRefused | ConnectionReset)
}
