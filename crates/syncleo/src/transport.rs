//! The [`Transport`] abstraction and its real UDP implementation.
//!
//! Everything below this module (the session, the codec) is free of
//! sockets; this is where bytes actually cross a wire boundary.

use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::time::Duration;

use crate::error::Error;

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

/// Build the address to connect to, resolving `interface` -- a stable
/// interface *name* -- to the OS's current index for it.
///
/// Names, not indices, are what gets carried through discovery and stored
/// in the CLI's config: a kernel interface index is reassigned whenever
/// hardware is replugged or a driver reloads, so anything meant to survive
/// a reboot has to keep the name and resolve it fresh, right before it's
/// needed, rather than caching the index itself.
///
/// A link-local IPv6 address (`fe80::/10`) cannot be connected to at all
/// without a scope id -- the kernel returns `EINVAL`, unable to tell which
/// link the address lives on -- so `interface` being `None` for one is
/// rejected here with a clear error instead of being passed through to a
/// socket call that would fail anyway, less legibly.
pub fn socket_addr(address: IpAddr, port: u16, interface: Option<&str>) -> Result<SocketAddr, Error> {
    let v6 = match address {
        IpAddr::V4(v4) => return Ok(SocketAddr::V4(SocketAddrV4::new(v4, port))),
        IpAddr::V6(v6) => v6,
    };

    if !v6.is_unicast_link_local() {
        return Ok(SocketAddr::V6(SocketAddrV6::new(v6, port, 0, 0)));
    }

    let name = interface.ok_or(Error::LinkLocalAddressWithoutScope)?;
    let index = interface_index(name)?;
    Ok(SocketAddr::V6(SocketAddrV6::new(v6, port, 0, index)))
}

/// The one FFI call needed to turn a stable interface name into the OS's
/// current index for it (`libc::if_nametoindex`, the standard POSIX way to
/// do this -- see `if_nametoindex(3)`).
fn interface_index(name: &str) -> Result<u32, Error> {
    let c_name = std::ffi::CString::new(name).map_err(|_| {
        Error::Io(std::io::Error::other(format!("interface name {name:?} contains a NUL byte")))
    })?;
    // Safety: `c_name` is a valid, NUL-terminated C string that outlives
    // the call, per `CString`'s own guarantee; `if_nametoindex` only reads
    // it and returns a plain integer.
    let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if index == 0 {
        // The kernel doesn't distinguish "no such interface" from other
        // failures here beyond errno; std::io::Error::last_os_error()
        // reports whatever it was (typically ENODEV / ENXIO).
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn a_global_ipv4_address_needs_no_interface() {
        let addr = socket_addr(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)), 8888, None).unwrap();
        assert_eq!(addr, SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 42), 8888)));
    }

    #[test]
    fn a_global_ipv6_address_needs_no_interface() {
        let global: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let addr = socket_addr(IpAddr::V6(global), 8888, None).unwrap();
        assert_eq!(addr, SocketAddr::V6(SocketAddrV6::new(global, 8888, 0, 0)));
    }

    #[test]
    fn a_link_local_ipv6_address_without_an_interface_is_rejected() {
        let link_local: Ipv6Addr = "fe80::1".parse().unwrap();
        let err = socket_addr(IpAddr::V6(link_local), 8888, None).unwrap_err();
        assert!(
            matches!(err, Error::LinkLocalAddressWithoutScope),
            "expected LinkLocalAddressWithoutScope, got {err:?}"
        );
    }

    #[test]
    fn a_link_local_ipv6_address_resolves_a_real_interface_to_its_index() {
        // "lo" (the loopback interface) exists on every Linux box this
        // runs on, sandboxed or not -- resolving its name to an index is a
        // read of the kernel's interface table, not a network operation.
        let link_local: Ipv6Addr = "fe80::1".parse().unwrap();
        let addr = socket_addr(IpAddr::V6(link_local), 8888, Some("lo")).unwrap();
        match addr {
            SocketAddr::V6(v6) => {
                assert_eq!(*v6.ip(), link_local);
                assert_ne!(v6.scope_id(), 0, "the loopback interface must resolve to a real index");
            }
            SocketAddr::V4(_) => panic!("expected a V6 address"),
        }
    }

    #[test]
    fn an_unknown_interface_name_is_reported_rather_than_silently_using_scope_zero() {
        let link_local: Ipv6Addr = "fe80::1".parse().unwrap();
        let err =
            socket_addr(IpAddr::V6(link_local), 8888, Some("d3home-no-such-iface")).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "expected an Io error, got {err:?}");
    }
}
