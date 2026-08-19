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

/// One address advertised for a device, together with whatever interface
/// scope came with it. `interface` is only ever meaningful for IPv6: an
/// mDNS responder tags every IPv6 address it advertises with the name of
/// the interface it saw it on, but never does this for IPv4.
///
/// Deliberately network-free: this is `mdns-sd`'s `ScopedIp` reduced to the
/// two facts `parse_service` needs, so that function stays pure and never
/// has to reach for the OS or the library's own types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedAddr {
    pub addr: IpAddr,
    pub interface: Option<String>,
}

/// Everything needed to open a session with a device found on the network.
///
/// `interface` is `Some` exactly when `address` is a link-local IPv6
/// address -- that is the only case a socket needs a scope id to connect
/// at all. It names the interface (e.g. `"enp8s0"`), not its OS-assigned
/// index: indices are reassigned across a reboot or a replugged NIC, so
/// anything meant to survive one (this struct, and `[devices.cached]` in
/// the CLI's config) stores the stable name and resolves it to an index
/// only right before it's needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub mac: String,
    pub address: IpAddr,
    pub interface: Option<String>,
    pub port: u16,
    pub public_wire: [u8; 32],
    pub curve: u8,
    pub protocol: u16,
}

impl Found {
    /// The address in the form that can be pasted into `ping` (and, minus
    /// the interface part, into a config's `[devices.cached].address`):
    /// plain for a globally usable address, `addr%iface` for a scoped
    /// link-local one.
    pub fn address_display(&self) -> String {
        display_scoped(&self.address, self.interface.as_deref())
    }
}

/// Render `addr` the way an operator would type it: plain, or `addr%iface`
/// when a scope is known. This is the `ping fe80::...%enp8s0` form.
pub fn display_scoped(addr: &IpAddr, interface: Option<&str>) -> String {
    match interface {
        Some(name) => format!("{addr}%{name}"),
        None => addr.to_string(),
    }
}

pub trait Discovery {
    /// Scan for every advertised device until `timeout` elapses.
    fn find_all(&self, timeout: Duration) -> Result<Vec<Found>, Error>;

    /// Scan for a single device by MAC among everything found within
    /// `timeout`.
    fn find(&self, mac: &str, timeout: Duration) -> Result<Option<Found>, Error>;
}


/// How long to keep listening after the device we were looking for answers.
///
/// Not zero: a device can be resolved on more than one interface, and a
/// global address is worth more than a link-local one (see `dedupe_by_mac`).
/// Stopping dead on the first record risks taking the worse of two that were
/// milliseconds apart, so the scan lingers briefly and then decides.
const GRACE_AFTER_MATCH: Duration = Duration::from_millis(300);

/// How long a scan may still run.
///
/// Pure, with the clock passed in, so the shrink-on-match behaviour can be
/// tested without waiting for anything or opening a socket.
#[derive(Debug, Clone, Copy)]
struct ScanWindow {
    overall: Instant,
    shortened: Option<Instant>,
}

impl ScanWindow {
    fn new(now: Instant, timeout: Duration) -> Self {
        Self { overall: now + timeout, shortened: None }
    }

    /// Called when the scan has what it came for. The window closes after
    /// the grace period, or at the original deadline if that comes first.
    fn satisfied(&mut self, now: Instant) {
        let candidate = now + GRACE_AFTER_MATCH;
        let deadline = candidate.min(self.overall);
        self.shortened = Some(self.shortened.map_or(deadline, |existing| existing.min(deadline)));
    }

    fn remaining(&self, now: Instant) -> Duration {
        self.shortened.unwrap_or(self.overall).saturating_duration_since(now)
    }
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

impl MdnsDiscovery {
    /// Browse until `timeout`, or until `enough` recognises what we came
    /// for and the grace period after it elapses.
    fn scan(&self, timeout: Duration, mut enough: impl FnMut(&Found) -> bool) -> Result<Vec<Found>, Error> {
        let receiver = self.daemon.browse(SERVICE_TYPE).map_err(mdns_error)?;
        let mut window = ScanWindow::new(Instant::now(), timeout);
        let mut found = Vec::new();

        loop {
            let remaining = window.remaining(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match receiver.recv_timeout(remaining) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let addresses: Vec<ScopedAddr> =
                        info.get_addresses().iter().map(scoped_addr).collect();
                    let txt: Vec<(String, String)> = info
                        .get_properties()
                        .iter()
                        .map(|prop| (prop.key().to_string(), prop.val_str().to_string()))
                        .collect();

                    if let Ok(device) =
                        parse_service(info.get_fullname(), &addresses, info.get_port(), &txt)
                    {
                        if enough(&device) {
                            window.satisfied(Instant::now());
                        }
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
        Ok(dedupe_by_mac(found))
    }
}

impl Discovery for MdnsDiscovery {
    /// Browse `SERVICE_TYPE`, collecting `ServiceResolved` events until
    /// `timeout` elapses. Each resolved record is run through
    /// `parse_service`; records that fail to parse (a malformed neighbour,
    /// an unsupported protocol version, an address made only of link-local
    /// junk) are skipped rather than aborting the whole scan.
    fn find_all(&self, timeout: Duration) -> Result<Vec<Found>, Error> {
        // Nothing satisfies a scan for everything, so it always runs its
        // full course.
        self.scan(timeout, |_| false)
    }

    fn find(&self, mac: &str, timeout: Duration) -> Result<Option<Found>, Error> {
        // Looking for one known device: once it has answered there is
        // nothing left to wait for, and waiting anyway is the difference
        // between a command that feels instant and one that takes five
        // seconds. This matters more than it sounds -- the kettle rotates
        // its keypair on every power loss, so a cached endpoint goes stale
        // every time it is lifted off its base, and every one of those costs
        // a scan.
        let devices = self.scan(timeout, |device| device.mac == mac)?;
        Ok(devices.into_iter().find(|device| device.mac == mac))
    }
}

/// `mdns-sd`'s own error type is a library-internal detail; fold it into
/// our `Error::Io` rather than growing a discovery-specific error variant
/// for it.
fn mdns_error(err: mdns_sd::Error) -> Error {
    Error::Io(std::io::Error::other(err))
}

/// Reduce one of `mdns-sd`'s own `ScopedIp` values to the two facts
/// `parse_service` needs. IPv4 addresses never carry scope information in
/// this protocol (the kettle has no reason to advertise one, and Syncleo
/// only ever needs a scope id for a *link-local IPv6* destination), so this
/// only ever sets `interface` for the `V6` case.
fn scoped_addr(scoped: &ScopedIp) -> ScopedAddr {
    match scoped {
        ScopedIp::V4(v4) => ScopedAddr { addr: IpAddr::V4(*v4.addr()), interface: None },
        ScopedIp::V6(v6) => {
            ScopedAddr { addr: IpAddr::V6(*v6.addr()), interface: Some(v6.scope_id().name.clone()) }
        }
        // `ScopedIp` is `#[non_exhaustive]`; a future `mdns-sd` release
        // could add a variant this was never written for. `to_ip_addr` is
        // the one thing every variant is guaranteed to have, so fall back
        // to it with no scope rather than failing to compile against a
        // dependency bump.
        other => ScopedAddr { addr: other.to_ip_addr(), interface: None },
    }
}

/// Parse one resolved mDNS service record into a [`Found`]. Pure and
/// network-free: everything `MdnsDiscovery` learns from the wire funnels
/// through here, so this is where malformed or unsupported records are
/// rejected.
pub fn parse_service(
    name: &str,
    addresses: &[ScopedAddr],
    port: u16,
    txt: &[(String, String)],
) -> Result<Found, Error> {
    // The instance name is "<mac>._syncleo._udp.local."; the MAC is
    // whatever precedes the first label separator.
    let mac = name.split('.').next().unwrap_or_default().to_string();

    let (address, interface) = select_address(addresses)?;

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

    Ok(Found { mac, address, interface, port, public_wire, curve, protocol })
}

/// Keep at most one [`Found`] per MAC address.
///
/// `find_all` pushes every resolved record that parses with nothing keying
/// on MAC, so a device resolved more than once within one scan window --
/// once per interface on a machine with both Wi-Fi and Ethernet on the same
/// LAN, or a bare re-announce -- used to come back as two (or more) entries
/// for the same device, each with its own address. `discover` would then
/// list it twice, and `cache_discovered`/`find` would silently take
/// whichever happened to be first, which could be the interface that's
/// about to go down.
///
/// When two records share a MAC, a globally usable address wins over a
/// link-local one -- the same preference [`select_address`] already
/// applies *within* one record, extended here to apply *across* every
/// record this scan collected. Between two records of equal "quality," the
/// first one seen wins: nothing in the protocol says which of two
/// identical-looking records is more current, so this is at least
/// deterministic. Kept pure and separate from `find_all`'s I/O loop so it
/// can be tested without a real multicast socket.
fn dedupe_by_mac(found: Vec<Found>) -> Vec<Found> {
    let mut kept: Vec<Found> = Vec::with_capacity(found.len());
    for candidate in found {
        match kept.iter().position(|f| f.mac == candidate.mac) {
            Some(i) if is_globally_usable(&candidate.address) && !is_globally_usable(&kept[i].address) => {
                kept[i] = candidate;
            }
            Some(_) => {}
            None => kept.push(candidate),
        }
    }
    kept
}

/// Pick the address to connect to, and the interface scope (if any) that
/// goes with it.
///
/// A globally usable address -- IPv4 that isn't `169.254.0.0/16`, or IPv6
/// that isn't link-local -- always wins: it needs no scope id and works
/// regardless of which interface the caller ends up sending from. Only
/// when nothing better was advertised does a link-local IPv6 address get
/// used, and then only if it carries the interface it was seen on --
/// without that, a socket can't be connected to it at all (the kernel
/// rejects the connect with `EINVAL`, unable to tell which link it means),
/// so that case is rejected here instead of being handed to a socket that
/// will fail.
fn select_address(addresses: &[ScopedAddr]) -> Result<(IpAddr, Option<String>), Error> {
    if let Some(candidate) = addresses.iter().find(|a| is_globally_usable(&a.addr)) {
        return Ok((candidate.addr, None));
    }

    let mut saw_scopeless_link_local = false;
    for candidate in addresses {
        if is_ipv6_link_local(&candidate.addr) {
            match &candidate.interface {
                Some(interface) => return Ok((candidate.addr, Some(interface.clone()))),
                None => saw_scopeless_link_local = true,
            }
        }
    }

    if saw_scopeless_link_local {
        return Err(Error::LinkLocalAddressWithoutScope);
    }
    Err(Error::NoUsableAddress)
}

/// A globally usable address: not IPv4 link-local autoconfiguration
/// (`169.254.0.0/16`, an interface that never got a real address) and not
/// IPv6 link-local (`fe80::/10`, only ever valid alongside a scope id).
fn is_globally_usable(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => !v4.is_link_local(),
        IpAddr::V6(v6) => !v6.is_unicast_link_local(),
    }
}

fn is_ipv6_link_local(addr: &IpAddr) -> bool {
    matches!(addr, IpAddr::V6(v6) if v6.is_unicast_link_local())
}

fn txt_value<'a>(txt: &'a [(String, String)], key: &str) -> Result<&'a str, Error> {
    txt.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| Error::BadServiceRecord(format!("missing TXT key {key:?}")))
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

    #[test]
    fn a_scan_with_nothing_to_satisfy_it_runs_its_full_course() {
        let now = Instant::now();
        let window = ScanWindow::new(now, Duration::from_secs(5));
        assert_eq!(window.remaining(now), Duration::from_secs(5));
        assert_eq!(window.remaining(now + Duration::from_secs(2)), Duration::from_secs(3));
    }

    #[test]
    fn finding_the_device_shortens_the_window_to_the_grace_period() {
        // The whole point: a five-second scan that gets its answer after one
        // second is over at 1.3 seconds, not at five.
        let now = Instant::now();
        let mut window = ScanWindow::new(now, Duration::from_secs(5));
        let matched = now + Duration::from_secs(1);
        window.satisfied(matched);
        assert_eq!(window.remaining(matched), GRACE_AFTER_MATCH);
        assert!(window.remaining(matched + GRACE_AFTER_MATCH).is_zero());
    }

    #[test]
    fn the_grace_period_never_extends_the_original_deadline() {
        // A match arriving just before time runs out must not buy the scan
        // extra seconds it was never allowed.
        let now = Instant::now();
        let mut window = ScanWindow::new(now, Duration::from_millis(50));
        window.satisfied(now + Duration::from_millis(40));
        assert!(window.remaining(now + Duration::from_millis(50)).is_zero());
    }

    #[test]
    fn a_later_match_does_not_push_the_window_back_out() {
        // Several records for the same device arrive in a burst; the first
        // one starts the clock and the rest must not keep resetting it.
        let now = Instant::now();
        let mut window = ScanWindow::new(now, Duration::from_secs(5));
        window.satisfied(now + Duration::from_millis(100));
        let first_close = window.remaining(now);
        window.satisfied(now + Duration::from_millis(200));
        assert_eq!(window.remaining(now), first_close, "the window drifted later");
    }
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn txt(public: &str, curve: &str, protocol: &str) -> Vec<(String, String)> {
        vec![
            ("public".into(), public.into()),
            ("curve".into(), curve.into()),
            ("protocol".into(), protocol.into()),
        ]
    }

    const PUBLIC: &str = "21d4043d930c3d75140c158c3406257204670512254e6e145eae239f354bdb57";

    fn v4(addr: Ipv4Addr) -> ScopedAddr {
        ScopedAddr { addr: addr.into(), interface: None }
    }

    fn v6(addr: Ipv6Addr, interface: Option<&str>) -> ScopedAddr {
        ScopedAddr { addr: addr.into(), interface: interface.map(str::to_string) }
    }

    /// The kettle from the field report: it advertises exactly one address,
    /// and it's link-local.
    fn kettle_link_local() -> Ipv6Addr {
        "fe80::dead:beef:dead:beef".parse().unwrap()
    }

    #[test]
    fn reads_a_well_formed_service_record() {
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v4(Ipv4Addr::new(192, 168, 1, 42))],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap();

        assert_eq!(found.mac, "aabbccddeeff");
        assert_eq!(found.port, 8888);
        assert_eq!(found.address, IpAddr::from(Ipv4Addr::new(192, 168, 1, 42)));
        assert_eq!(found.interface, None, "a global address needs no scope");
        assert_eq!(found.public_wire.len(), 32);
    }

    #[test]
    fn skips_link_local_addresses() {
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v4(Ipv4Addr::new(169, 254, 3, 4)), v4(Ipv4Addr::new(192, 168, 1, 42))],
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
    fn a_global_address_is_preferred_over_a_link_local_ipv6_one() {
        // The design brief's requirement: even when a usable link-local
        // IPv6 address (with scope) is on offer, a global address -- IPv4
        // or IPv6 -- always wins, since it needs no scope id and works
        // regardless of which interface traffic ends up leaving from.
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v6(kettle_link_local(), Some("enp8s0")), v4(Ipv4Addr::new(192, 168, 1, 42))],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap();

        assert_eq!(found.address, IpAddr::from(Ipv4Addr::new(192, 168, 1, 42)));
        assert_eq!(found.interface, None);
    }

    #[test]
    fn a_link_local_ipv6_address_with_scope_is_accepted_when_nothing_better_is_advertised() {
        // This is the real kettle's case: it advertises exactly one
        // address, and it's link-local. Rejecting it outright (the old
        // "prefer IPv4, else fail" behaviour) would leave nothing to
        // connect to; the fix is to carry the scope through instead.
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v6(kettle_link_local(), Some("enp8s0"))],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap();

        assert_eq!(found.address, IpAddr::from(kettle_link_local()));
        assert_eq!(found.interface.as_deref(), Some("enp8s0"));
    }

    #[test]
    fn a_link_local_ipv6_address_with_no_scope_information_is_rejected() {
        // Without a scope id, connecting to this address fails at the OS
        // level with EINVAL; better to say so clearly here than to hand a
        // socket something doomed to fail.
        let err = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v6(kettle_link_local(), None)],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap_err();

        assert!(
            matches!(err, Error::LinkLocalAddressWithoutScope),
            "expected LinkLocalAddressWithoutScope, got {err:?}"
        );
    }

    #[test]
    fn refuses_protocol_versions_it_was_not_written_for() {
        // Guessing at an unknown protocol version would be worse than saying so.
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v4(Ipv4Addr::new(192, 168, 1, 42))],
            8888,
            &txt(PUBLIC, "29", "3"),
        )
        .is_err());

        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v4(Ipv4Addr::new(192, 168, 1, 42))],
            8888,
            &txt(PUBLIC, "30", "2"),
        )
        .is_err());
    }

    #[test]
    fn refuses_a_record_with_no_usable_address() {
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v4(Ipv4Addr::new(169, 254, 3, 4))],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .is_err());
    }

    #[test]
    fn refuses_a_malformed_public_key() {
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[v4(Ipv4Addr::new(192, 168, 1, 42))],
            8888,
            &txt("abcd", "29", "2"),
        )
        .is_err());
    }

    fn found_with(mac: &str, address: ScopedAddr) -> Found {
        Found {
            mac: mac.into(),
            address: address.addr,
            interface: address.interface,
            port: 8888,
            public_wire: [0xAB; 32],
            curve: 29,
            protocol: 2,
        }
    }

    #[test]
    fn find_all_keeps_only_one_record_per_mac_preferring_a_global_address() {
        // Finding 16: a device resolved on more than one interface within
        // one scan window used to come back once per interface. This pins
        // the dedupe: the same MAC seen twice collapses to one entry, and
        // a global address wins over a link-local one regardless of which
        // was seen first.
        let global = found_with("aabbccddeeff", v4(Ipv4Addr::new(192, 168, 1, 42)));
        let link_local = found_with("aabbccddeeff", v6(kettle_link_local(), Some("enp8s0")));

        let link_local_first = dedupe_by_mac(vec![link_local.clone(), global.clone()]);
        assert_eq!(link_local_first, vec![global.clone()], "a global address must win regardless of order");

        let global_first = dedupe_by_mac(vec![global.clone(), link_local]);
        assert_eq!(global_first, vec![global]);
    }

    #[test]
    fn find_all_keeps_devices_with_different_macs_separate() {
        let a = found_with("aabbccddeeff", v4(Ipv4Addr::new(192, 168, 1, 42)));
        let b = found_with("112233445566", v4(Ipv4Addr::new(192, 168, 1, 43)));

        let kept = dedupe_by_mac(vec![a.clone(), b.clone()]);
        assert_eq!(kept, vec![a, b]);
    }

    #[test]
    fn find_all_keeps_the_first_seen_record_when_neither_candidate_is_better() {
        // Two link-local records for the same MAC, on different
        // interfaces: nothing in the protocol says which is more current,
        // so the first one seen must win, deterministically.
        let first = found_with("aabbccddeeff", v6(kettle_link_local(), Some("enp8s0")));
        let second = found_with("aabbccddeeff", v6(kettle_link_local(), Some("wlan0")));

        let kept = dedupe_by_mac(vec![first.clone(), second]);
        assert_eq!(kept, vec![first]);
    }

    // Ignored by default: this starts a real `mdns_sd::ServiceDaemon`,
    // which binds a UDP multicast socket and joins the mDNS group. That is
    // an OS-level operation this sandbox blocks outright, regardless of
    // whether any device answers, so running it unconditionally would make
    // the suite fail here on environment grounds, not logic. Kept for
    // deliberate runs (`cargo test -- --ignored`); exercised for real in
    // Task 12 against the physical kettle.
    #[test]
    #[ignore = "needs a real network: binds a multicast socket, which this sandbox blocks"]
    fn find_all_returns_cleanly_when_nothing_answers_in_time() {
        let discovery = MdnsDiscovery::new().expect("mdns daemon should start");
        let found = discovery.find_all(Duration::from_millis(50));
        assert!(found.is_ok(), "a timeout with no replies is not an error: {found:?}");
    }
}
