//! What `d3home <kettle-or-alias> <action>` does. Everything here is
//! specific to the `"syncleo"` driver; a second device type (a vacuum, say)
//! would get its own sibling module rather than teaching `cli::parse` or
//! `main::dispatch` a new device type -- the action words are only ever
//! interpreted here, by the driver that owns them.

use std::net::{IpAddr, SocketAddr};
use std::ops::ControlFlow;
use std::path::Path;
use std::time::Duration;

use syncleo::client::Client;
use syncleo::codec::command::{Command, PowerMode};
use syncleo::discovery::{Discovery, Found, MdnsDiscovery};
use syncleo::transport::{UdpTransport, socket_addr};

use crate::cli::AppError;
use crate::config::{Cached, Config, ConfigError, Device, hex_encode};
use crate::output;

/// Provisional: Task 12 checks these against the real kettle and corrects
/// them if the hardware disagrees.
pub const MIN_TEMPERATURE: u8 = 35;
pub const MAX_TEMPERATURE: u8 = 100;

/// How long `connect` waits for the handshake to complete.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `discover` (used when a device has no cached endpoint) waits
/// for a reply before giving up.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `status` waits, after the last state event it saw, before
/// deciding the device's post-handshake burst is over. The burst is many
/// small messages sent close together, so a gap this long means it has
/// finished, not merely paused.
const STATUS_QUIET_WINDOW: Duration = Duration::from_millis(300);
/// The overall cap on how long `status` waits for the first state event to
/// arrive at all. The protocol has no "query state" command; the device
/// reports its full state, unprompted, right after the handshake, but on
/// the real hardware that burst has been observed to start late -- this
/// needs to be generous enough to still catch it rather than reporting
/// [`syncleo::Error::NoState`] on a device that simply hadn't gotten to it
/// yet. Comparable to `DISCOVERY_TIMEOUT` below: both cover "the device is
/// slow," not "the device is gone."
const STATUS_OVERALL_DEADLINE: Duration = Duration::from_secs(5);

/// Run one kettle action. `action` is whatever followed the device name on
/// the command line, unexamined until now. `config_path` is threaded down
/// to `connect` so a freshly discovered endpoint can be cached back into
/// the registry.
pub fn run(device: &Device, action: &[String], json: bool, config_path: &Path) -> Result<(), AppError> {
    let (verb, rest) = action
        .split_first()
        .ok_or_else(|| AppError::Usage("missing action; try status, start, off, or watch".into()))?;

    match verb.as_str() {
        "status" => status(device, json, config_path),
        "start" => start(device, rest, config_path),
        "off" => off(device, config_path),
        "watch" => watch(device, json, config_path),
        other => Err(AppError::Usage(format!(
            "unknown kettle action '{other}'; try status, start, off, or watch"
        ))),
    }
}

fn status(device: &Device, json: bool, config_path: &Path) -> Result<(), AppError> {
    let mut client = connect(device, config_path)?;
    let state = client.collect_state(STATUS_QUIET_WINDOW, STATUS_OVERALL_DEADLINE)?;
    output::print_state(&state, json);

    // The device has its own notion of an error condition (no water,
    // overheat, ...), separate from anything going wrong in the transport
    // or handshake. Surface it as a distinct exit code so a script can
    // tell "couldn't reach the kettle" apart from "reached it, and it says
    // something is wrong."
    if state.error == Some(true) {
        return Err(AppError::Device(format!("device '{}' reports an error", device.name)));
    }
    Ok(())
}

fn start(device: &Device, args: &[String], config_path: &Path) -> Result<(), AppError> {
    // Validated before connecting: a bad temperature should fail instantly,
    // not after a network round trip that was always going to be wasted.
    let target = parse_target_temperature(args)?;

    let mut client = connect(device, config_path)?;
    match target {
        None => client.send(Command::Mode(PowerMode::On))?,
        Some(temperature) => {
            // Order matters here: Custom mode first, then the target. Task
            // 12 verifies this against the real kettle and flips it if the
            // hardware wants the other order.
            client.send(Command::Mode(PowerMode::Custom))?;
            client.send(Command::TargetTemperature(temperature))?;
        }
    }
    Ok(())
}

fn off(device: &Device, config_path: &Path) -> Result<(), AppError> {
    let mut client = connect(device, config_path)?;
    client.send(Command::Mode(PowerMode::Off))?;
    Ok(())
}

/// Stream device events until the connection is lost (or the process is
/// killed). Each event is handed straight to `output::print_event` as it
/// arrives -- no buffering -- so this stays a genuine stream: a future
/// caller (a notifier, a TUI) can swap in a different callback without
/// this function changing shape.
fn watch(device: &Device, json: bool, config_path: &Path) -> Result<(), AppError> {
    let mut client = connect(device, config_path)?;
    client.watch(|event| {
        output::print_event(&event, json);
        ControlFlow::Continue(())
    })?;
    Ok(())
}

/// `[]` means plain `start` (turn on at the default target); anything else
/// is parsed as the one temperature argument and checked against the
/// supported range.
fn parse_target_temperature(args: &[String]) -> Result<Option<u8>, AppError> {
    match args {
        [] => Ok(None),
        [temperature] => {
            let value: u8 = temperature
                .parse()
                .map_err(|_| AppError::Usage(format!("'{temperature}' is not a valid temperature")))?;
            if !(MIN_TEMPERATURE..=MAX_TEMPERATURE).contains(&value) {
                return Err(AppError::Usage(format!(
                    "temperature must be between {MIN_TEMPERATURE} and {MAX_TEMPERATURE} degrees C, got {value}"
                )));
            }
            Ok(Some(value))
        }
        _ => Err(AppError::Usage("usage: d3home <device> start [temperature]".into())),
    }
}

/// Perform the handshake with `device`.
///
/// If the config has a cached endpoint, try it first. A cached endpoint
/// that rejects the handshake (`HandshakeRejected`) is left alone --  a
/// wrong token is not a stale address, and falling back would silently
/// mask a real problem behind a slow, confusing retry. A cached endpoint
/// that simply doesn't answer (`Timeout`) is far more likely a DHCP lease
/// having moved the device than a device that vanished, so that case falls
/// back to a fresh mDNS lookup and updates the cache with whatever it
/// finds.
///
/// With no cache at all, this goes straight to mDNS and caches the result
/// on success.
fn connect(device: &Device, config_path: &Path) -> Result<Client, AppError> {
    let token = device.token_bytes()?;

    if let Some(cached) = &device.cached {
        let public_wire = decode_public_key(&cached.public_key, &device.name)?;
        let addr = cached_socket_addr(cached, &device.name)?;
        match evaluate_cached_attempt(try_connect(addr, public_wire, token)) {
            CachedAttempt::Connected(client) => return Ok(client),
            CachedAttempt::StaleFallBackToDiscovery => {
                // Fall through to discovery below.
            }
            CachedAttempt::Failed(err) => return Err(err),
        }
    }

    let found = discover_device(device)?;
    cache_endpoint(
        config_path,
        &device.name,
        found.address,
        found.port,
        found.public_wire,
        found.interface.clone(),
    );

    // `parse_service` never hands back a link-local address without an
    // interface (see `discovery::select_address`), so this can't actually
    // hit the "no scope" error path -- but it still goes through the same
    // scope-aware constructor as the cached path rather than a bare
    // `SocketAddr::new`, so a link-local IPv6 destination is never handed
    // to the socket without its scope id.
    let addr = socket_addr(found.address, found.port, found.interface.as_deref())?;
    try_connect(addr, found.public_wire, token).map_err(Into::into)
}

/// Build the socket address for a cached endpoint. A link-local IPv6
/// address with no recorded interface is turned into a message that names
/// the device and the fix, rather than the generic
/// `syncleo::Error::LinkLocalAddressWithoutScope`.
fn cached_socket_addr(cached: &Cached, device_name: &str) -> Result<SocketAddr, AppError> {
    socket_addr(cached.address, cached.port, cached.interface.as_deref()).map_err(|err| match err {
        syncleo::Error::LinkLocalAddressWithoutScope => AppError::Usage(format!(
            "device '{device_name}' has a cached link-local address ({}) with no interface \
             recorded; run 'd3home discover' again, or add `interface = \"<name>\"` under \
             [devices.cached]",
            cached.address
        )),
        other => other.into(),
    })
}

/// What to do after trying the cached endpoint, kept as a small pure
/// function separate from `connect` so the one decision this finding is
/// about -- fall back to discovery on a timeout, never on a rejected
/// handshake -- can be unit tested without a real socket or session.
enum CachedAttempt {
    Connected(Client),
    /// The cached address didn't answer at all: far more likely a stale
    /// DHCP lease than a device that stopped existing, so it's worth a
    /// fresh mDNS lookup.
    StaleFallBackToDiscovery,
    /// Anything else, most importantly `HandshakeRejected`: a wrong token
    /// is not a stale address, and retrying via discovery would silently
    /// mask that behind a slow, confusing retry instead of reporting it.
    Failed(AppError),
}

fn evaluate_cached_attempt(result: Result<Client, syncleo::Error>) -> CachedAttempt {
    match result {
        Ok(client) => CachedAttempt::Connected(client),
        Err(syncleo::Error::Timeout) => CachedAttempt::StaleFallBackToDiscovery,
        Err(other) => CachedAttempt::Failed(other.into()),
    }
}

fn try_connect(
    addr: SocketAddr,
    public_wire: [u8; 32],
    token: [u8; 16],
) -> Result<Client, syncleo::Error> {
    let transport = UdpTransport::connect(addr).map_err(syncleo::Error::Io)?;
    // A fresh key pair per connection: nothing in the protocol expects our
    // side's private key to be stable across runs, and generating one lets
    // the CLI stay stateless between invocations.
    let our_private = rand::random::<[u8; 32]>();
    Client::connect(Box::new(transport), our_private, public_wire, token, CONNECT_TIMEOUT)
}

/// Locate `device` on the network by its MAC address over mDNS.
fn discover_device(device: &Device) -> Result<Found, AppError> {
    let discovery = MdnsDiscovery::new()?;
    discovery.find(&device.mac, DISCOVERY_TIMEOUT)?.ok_or_else(|| {
        AppError::NotFound(format!(
            "device '{}' (mac {}) was not found on the network",
            device.name, device.mac
        ))
    })
}

/// Persist a freshly discovered endpoint into `device_name`'s
/// `[devices.cached]` entry and save the config. Reloads the config from
/// disk rather than threading a `&mut Config` down from `main`, matching
/// the pattern `commands::registry::alias_add`/`alias_rm` already use for
/// the same reason: this is a one-shot process, so there is no in-memory
/// config to keep in sync with the file besides what we re-read here.
///
/// A failure here (the config vanished, got wedged, the disk is full, ...)
/// must not fail the command the user actually asked for -- the endpoint
/// still works for *this* run, caching it is purely an optimisation for
/// the next one. So this only warns on stderr and carries on.
fn cache_endpoint(
    config_path: &Path,
    device_name: &str,
    address: IpAddr,
    port: u16,
    public_wire: [u8; 32],
    interface: Option<String>,
) {
    if let Err(err) = try_cache_endpoint(config_path, device_name, address, port, public_wire, interface) {
        eprintln!("d3home: warning: could not cache the discovered endpoint for '{device_name}': {err}");
    }
}

fn try_cache_endpoint(
    config_path: &Path,
    device_name: &str,
    address: IpAddr,
    port: u16,
    public_wire: [u8; 32],
    interface: Option<String>,
) -> Result<(), AppError> {
    let mut config = Config::load(config_path)?;
    let device = config
        .devices
        .iter_mut()
        .find(|d| d.name == device_name)
        .ok_or_else(|| ConfigError::UnknownDevice { name: device_name.to_string() })?;
    device.cached = Some(Cached { address, port, public_key: hex_encode(&public_wire), interface });
    config.save(config_path)?;
    Ok(())
}

fn decode_public_key(hex: &str, device_name: &str) -> Result<[u8; 32], AppError> {
    let malformed = || AppError::Usage(format!("device '{device_name}' has a malformed cached public key"));

    if hex.len() != 64 {
        return Err(malformed());
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| malformed())?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_with_no_argument_means_no_target_temperature() {
        assert_eq!(parse_target_temperature(&[]).unwrap(), None);
    }

    #[test]
    fn start_with_a_temperature_in_range_is_accepted() {
        assert_eq!(parse_target_temperature(&["80".to_string()]).unwrap(), Some(80));
        assert_eq!(parse_target_temperature(&["35".to_string()]).unwrap(), Some(35));
        assert_eq!(parse_target_temperature(&["100".to_string()]).unwrap(), Some(100));
    }

    #[test]
    fn a_temperature_outside_the_range_is_a_usage_error() {
        let err = parse_target_temperature(&["250".to_string()]).unwrap_err();
        assert_eq!(err.exit_code(), crate::cli::ExitCode::Usage);
        assert!(err.to_string().contains("35"));

        assert!(parse_target_temperature(&["34".to_string()]).is_err());
        assert!(parse_target_temperature(&["101".to_string()]).is_err());
    }

    #[test]
    fn a_non_numeric_temperature_is_a_usage_error() {
        assert!(parse_target_temperature(&["hot".to_string()]).is_err());
    }

    #[test]
    fn only_a_single_temperature_argument_is_accepted() {
        assert!(parse_target_temperature(&["80".to_string(), "90".to_string()]).is_err());
    }

    #[test]
    fn decodes_a_well_formed_cached_public_key() {
        let hex = "21d4043d930c3d75140c158c3406257204670512254e6e145eae239f354bdb57";
        assert!(decode_public_key(hex, "kettle").is_ok());
    }

    #[test]
    fn refuses_a_malformed_cached_public_key() {
        assert!(decode_public_key("not hex", "kettle").is_err());
        assert!(decode_public_key("ab", "kettle").is_err());
    }

    #[test]
    fn a_stale_cached_endpoint_falls_back_to_discovery() {
        // A DHCP lease moving the device looks exactly like this from the
        // client's side: the cached address simply never answers.
        assert!(matches!(
            evaluate_cached_attempt(Err(syncleo::Error::Timeout)),
            CachedAttempt::StaleFallBackToDiscovery
        ));
    }

    #[test]
    fn a_rejected_handshake_on_the_cached_endpoint_never_falls_back() {
        // A wrong token is not a stale address; retrying via discovery
        // would silently mask the real problem behind a slow, confusing
        // retry that was always going to fail the same way.
        match evaluate_cached_attempt(Err(syncleo::Error::HandshakeRejected)) {
            CachedAttempt::Failed(err) => assert_eq!(err.exit_code(), crate::cli::ExitCode::BadToken),
            CachedAttempt::StaleFallBackToDiscovery => {
                panic!("a rejected handshake must not fall back to discovery")
            }
            CachedAttempt::Connected(_) => unreachable!("Err(_) cannot produce Connected"),
        }
    }

    #[test]
    fn a_silence_timeout_on_the_cached_endpoint_does_not_fall_back() {
        // Only Error::Timeout triggers the fallback. Error::Silence -- the
        // session having *been* connected and then gone quiet -- cannot
        // occur inside the initial handshake this path is on, but if it
        // ever did, it should not be treated the same as never having
        // connected at all.
        match evaluate_cached_attempt(Err(syncleo::Error::Silence)) {
            CachedAttempt::Failed(_) => {}
            _ => panic!("Error::Silence must not trigger a fall back to discovery"),
        }
    }

    fn sample_kettle_toml(token: &str) -> String {
        format!(
            "[[devices]]\nname = \"kettle\"\ndriver = \"syncleo\"\nmac = \"aabbccddeeff\"\ntoken = \"{token}\"\n"
        )
    }

    #[test]
    fn caching_a_discovered_endpoint_persists_address_port_and_public_key() {
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-cache-endpoint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, sample_kettle_toml("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf")).unwrap();

        let public_wire = [0x77u8; 32];
        try_cache_endpoint(&path, "kettle", "192.168.1.42".parse().unwrap(), 8888, public_wire, None)
            .expect("caching a known device must succeed");

        let reloaded = Config::load(&path).unwrap();
        let cached = reloaded.resolve("kettle").unwrap().cached.as_ref().expect("cache was written");
        assert_eq!(cached.address, "192.168.1.42".parse::<IpAddr>().unwrap());
        assert_eq!(cached.port, 8888);
        assert_eq!(cached.public_key, hex_encode(&public_wire));
        assert_eq!(cached.interface, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn caching_a_link_local_discovery_persists_its_interface() {
        // The real kettle this was fixed against advertises only a
        // link-local IPv6 address; without the interface surviving the
        // cache round trip, the next `status` would hit EINVAL all over
        // again.
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-cache-endpoint-interface-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, sample_kettle_toml("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf")).unwrap();

        try_cache_endpoint(
            &path,
            "kettle",
            "fe80::dead:beef:dead:beef".parse().unwrap(),
            8888,
            [0x77u8; 32],
            Some("enp8s0".into()),
        )
        .expect("caching a known device must succeed");

        let reloaded = Config::load(&path).unwrap();
        let cached = reloaded.resolve("kettle").unwrap().cached.as_ref().expect("cache was written");
        assert_eq!(cached.interface.as_deref(), Some("enp8s0"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn caching_an_endpoint_for_an_unknown_device_is_reported() {
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-cache-endpoint-unknown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, sample_kettle_toml("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf")).unwrap();

        let result = try_cache_endpoint(
            &path,
            "teapot",
            "192.168.1.42".parse().unwrap(),
            8888,
            [0u8; 32],
            None,
        );
        assert!(result.is_err(), "caching an endpoint for a device that isn't in the config must fail");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cached_link_local_address_without_an_interface_is_a_clear_usage_error() {
        // This is the "clear error the user can act on" the design calls
        // for: a hand-edited or pre-upgrade config with a link-local
        // cached address but no interface must fail loudly, not hand a
        // socket something that will fail with EINVAL two layers down.
        let cached = Cached {
            address: "fe80::dead:beef:dead:beef".parse().unwrap(),
            port: 8888,
            public_key: "ab".repeat(32),
            interface: None,
        };

        let err = cached_socket_addr(&cached, "kettle").unwrap_err();
        assert_eq!(err.exit_code(), crate::cli::ExitCode::Usage);
        let message = err.to_string();
        assert!(message.contains("kettle"), "error should name the device: {message}");
        assert!(message.contains("interface"), "error should point at the fix: {message}");
    }

    #[test]
    fn a_cached_link_local_address_with_an_interface_resolves_to_a_scoped_socket_addr() {
        let cached = Cached {
            address: "fe80::dead:beef:dead:beef".parse().unwrap(),
            port: 8888,
            public_key: "ab".repeat(32),
            interface: Some("lo".into()),
        };

        let addr = cached_socket_addr(&cached, "kettle").expect("lo always resolves");
        assert!(matches!(addr, SocketAddr::V6(_)));
    }

    #[test]
    fn a_cached_global_address_needs_no_interface() {
        let cached = Cached {
            address: "192.168.1.42".parse().unwrap(),
            port: 8888,
            public_key: "ab".repeat(32),
            interface: None,
        };

        let addr = cached_socket_addr(&cached, "kettle").unwrap();
        assert_eq!(addr, SocketAddr::new("192.168.1.42".parse().unwrap(), 8888));
    }
}
