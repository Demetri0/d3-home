//! Finding a kettle on the local network over mDNS.
//!
//! The device advertises itself as a `_syncleo._udp.local.` service, with
//! its public key and protocol version in the TXT record. This module keeps
//! all the judgement — parsing, validation, filtering — in the pure
//! [`parse_service`], leaving [`MdnsDiscovery`] as a thin shell that only
//! does I/O: browse, collect events until the timeout, hand each one to
//! `parse_service`.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent};

use crate::error::Error;

/// The mDNS service type the kettle advertises itself under.
pub const SERVICE_TYPE: &str = "_syncleo._udp.local.";

/// The only curve and protocol version this client knows how to speak.
const SUPPORTED_CURVE: u8 = 29;
const SUPPORTED_PROTOCOL: u16 = 2;

/// Everything needed to open a session with a device found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub mac: String,
    pub address: IpAddr,
    pub port: u16,
    pub public_wire: [u8; 32],
    pub curve: u8,
    pub protocol: u16,
}

pub trait Discovery {
    /// Scan for every advertised device until `timeout` elapses.
    fn find_all(&self, timeout: Duration) -> Result<Vec<Found>, Error>;

    /// Scan for a single device by MAC among everything found within
    /// `timeout`.
    fn find(&self, mac: &str, timeout: Duration) -> Result<Option<Found>, Error>;
}

/// The real, networked [`Discovery`]: owns an `mdns-sd` daemon.
pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
}

impl MdnsDiscovery {
    pub fn new() -> Result<Self, Error> {
        let daemon = ServiceDaemon::new().map_err(mdns_error)?;
        Ok(Self { daemon })
    }
}

impl Discovery for MdnsDiscovery {
    /// Browse `SERVICE_TYPE`, collecting `ServiceResolved` events until
    /// `timeout` elapses. Each resolved record is run through
    /// `parse_service`; records that fail to parse (a malformed neighbour,
    /// an unsupported protocol version, an address made only of link-local
    /// junk) are skipped rather than aborting the whole scan.
    fn find_all(&self, timeout: Duration) -> Result<Vec<Found>, Error> {
        let receiver = self.daemon.browse(SERVICE_TYPE).map_err(mdns_error)?;
        let deadline = Instant::now() + timeout;
        let mut found = Vec::new();

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match receiver.recv_timeout(remaining) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let addresses: Vec<IpAddr> =
                        info.get_addresses().iter().map(ScopedIp::to_ip_addr).collect();
                    let txt: Vec<(String, String)> = info
                        .get_properties()
                        .iter()
                        .map(|prop| (prop.key().to_string(), prop.val_str().to_string()))
                        .collect();

                    if let Ok(device) =
                        parse_service(info.get_fullname(), &addresses, info.get_port(), &txt)
                    {
                        found.push(device);
                    }
                }
                // Anything other than a resolved record (search started,
                // bare "found", removed, ...) carries nothing to parse yet.
                Ok(_) => continue,
                // Either the timeout elapsed or the daemon's channel closed;
                // either way there is nothing more to wait for.
                Err(_) => break,
            }
        }

        // Best-effort: the daemon and its thread are about to be dropped
        // regardless, so a failure to unregister the browse here changes
        // nothing observable.
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        Ok(found)
    }

    fn find(&self, mac: &str, timeout: Duration) -> Result<Option<Found>, Error> {
        Ok(self.find_all(timeout)?.into_iter().find(|device| device.mac == mac))
    }
}

/// `mdns-sd`'s own error type is a library-internal detail; fold it into
/// our `Error::Io` rather than growing a discovery-specific error variant
/// for it.
fn mdns_error(err: mdns_sd::Error) -> Error {
    Error::Io(std::io::Error::other(err))
}

/// Parse one resolved mDNS service record into a [`Found`]. Pure and
/// network-free: everything `MdnsDiscovery` learns from the wire funnels
/// through here, so this is where malformed or unsupported records are
/// rejected.
pub fn parse_service(
    name: &str,
    addresses: &[IpAddr],
    port: u16,
    txt: &[(String, String)],
) -> Result<Found, Error> {
    // The instance name is "<mac>._syncleo._udp.local."; the MAC is
    // whatever precedes the first label separator.
    let mac = name.split('.').next().unwrap_or_default().to_string();

    let address = addresses
        .iter()
        .copied()
        .find(|addr| !is_link_local_junk(addr))
        .ok_or(Error::NoUsableAddress)?;

    let public_hex = txt_value(txt, "public")?;
    let curve_str = txt_value(txt, "curve")?;
    let protocol_str = txt_value(txt, "protocol")?;

    let curve: u8 = curve_str
        .parse()
        .map_err(|_| Error::BadServiceRecord(format!("curve {curve_str:?} is not a number")))?;
    let protocol: u16 = protocol_str.parse().map_err(|_| {
        Error::BadServiceRecord(format!("protocol {protocol_str:?} is not a number"))
    })?;

    if curve != SUPPORTED_CURVE || protocol != SUPPORTED_PROTOCOL {
        return Err(Error::UnsupportedProtocol { curve, protocol });
    }

    let public_wire = decode_public_key(public_hex)?;

    Ok(Found { mac, address, port, public_wire, curve, protocol })
}

fn txt_value<'a>(txt: &'a [(String, String)], key: &str) -> Result<&'a str, Error> {
    txt.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| Error::BadServiceRecord(format!("missing TXT key {key:?}")))
}

/// 169.254.0.0/16 is IPv4 link-local autoconfiguration: an interface that
/// never got a real address. Useless as a destination for talking to the
/// kettle.
fn is_link_local_junk(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(_) => false,
    }
}

fn hex_digit(b: u8) -> Result<u8, Error> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::BadServiceRecord("public key is not valid hex".into())),
    }
}

fn decode_public_key(hex: &str) -> Result<[u8; 32], Error> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(Error::BadServiceRecord(format!(
            "public key must be 64 hex characters (32 bytes), got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_digit(bytes[2 * i])?;
        let lo = hex_digit(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn txt(public: &str, curve: &str, protocol: &str) -> Vec<(String, String)> {
        vec![
            ("public".into(), public.into()),
            ("curve".into(), curve.into()),
            ("protocol".into(), protocol.into()),
        ]
    }

    const PUBLIC: &str = "21d4043d930c3d75140c158c3406257204670512254e6e145eae239f354bdb57";

    #[test]
    fn reads_a_well_formed_service_record() {
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap();

        assert_eq!(found.mac, "aabbccddeeff");
        assert_eq!(found.port, 8888);
        assert_eq!(found.address, IpAddr::from(Ipv4Addr::new(192, 168, 1, 42)));
        assert_eq!(found.public_wire.len(), 32);
    }

    #[test]
    fn skips_link_local_addresses() {
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(169, 254, 3, 4).into(), Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap();

        assert_eq!(
            found.address,
            IpAddr::from(Ipv4Addr::new(192, 168, 1, 42)),
            "169.254/16 is useless here"
        );
    }

    #[test]
    fn refuses_protocol_versions_it_was_not_written_for() {
        // Guessing at an unknown protocol version would be worse than saying so.
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "29", "3"),
        )
        .is_err());

        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "30", "2"),
        )
        .is_err());
    }

    #[test]
    fn refuses_a_record_with_no_usable_address() {
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(169, 254, 3, 4).into()],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .is_err());
    }

    #[test]
    fn refuses_a_malformed_public_key() {
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt("abcd", "29", "2"),
        )
        .is_err());
    }

    // The networked half is exercised for real against the physical kettle
    // in a later task; here it only needs to compile and behave sanely when
    // nothing answers, which does not depend on anything being present.
    #[test]
    fn find_all_returns_cleanly_when_nothing_answers_in_time() {
        let discovery = MdnsDiscovery::new().expect("mdns daemon should start");
        let found = discovery.find_all(Duration::from_millis(50));
        assert!(found.is_ok(), "a timeout with no replies is not an error: {found:?}");
    }
}
