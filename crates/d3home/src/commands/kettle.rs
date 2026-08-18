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
use crate::config::{Cached, Config, ConfigError, Device, hex_decode, hex_encode};
use crate::output;
use crate::progress::{Phase, with_spinner};

/// Confirmed against the vendor app, which offers 30-100 in steps of 5.
/// The step is deliberately *not* enforced here: the wire format carries a
/// raw byte, and there is no evidence the device itself rejects an
/// intermediate value -- only that the app never offers one. If the device
/// does reject one, `Client::send` now surfaces that as
/// `syncleo::Error::DeviceNak` (exit code 6), which is an honest answer
/// grounded in what the hardware actually said, rather than a client-side
/// guess about a step the protocol may not enforce at all.
pub const MIN_TEMPERATURE: u8 = 30;
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

/// The backoff `watch` waits between reconnect attempts once the device
/// has gone away, starting here and doubling on every further failure
/// (see [`WATCH_RECONNECT_BACKOFF_CEILING`]). Short enough that a brief
/// hiccup recovers almost immediately.
const WATCH_RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(500);
/// The cap the backoff above grows to and then holds at. A kettle that's
/// genuinely off its base for a while shouldn't be hit with a full connect
/// cycle -- a handshake attempt and, if that times out, an mDNS scan --
/// more than about once every few seconds.
const WATCH_RECONNECT_BACKOFF_CEILING: Duration = Duration::from_secs(5);

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
    let state = with_spinner(Phase::WaitingForState, || {
        client.collect_state(STATUS_QUIET_WINDOW, STATUS_OVERALL_DEADLINE)
    })?;
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
    with_spinner(Phase::Sending, || -> Result<(), AppError> {
        match target {
            None => client.send(Command::Mode(PowerMode::On))?,
            Some(temperature) => {
                // Order matters here: Custom mode first, then the target.
                // Task 12 verifies this against the real kettle and flips
                // it if the hardware wants the other order.
                client.send(Command::Mode(PowerMode::Custom))?;
                client.send(Command::TargetTemperature(temperature))?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

fn off(device: &Device, config_path: &Path) -> Result<(), AppError> {
    let mut client = connect(device, config_path)?;
    with_spinner(Phase::Sending, || client.send(Command::Mode(PowerMode::Off)))?;
    Ok(())
}

/// Stream device events until the process is killed (or a fatal error --
/// see below -- ends it). Each event is handed straight to
/// `output::print_event` as it arrives -- no buffering -- so this stays a
/// genuine stream: a future caller (a notifier, a TUI) can swap in a
/// different callback without this function changing shape.
///
/// A lost connection does not end `watch`. Lifting the kettle off its base
/// cuts its power outright, and that happens often enough in ordinary use
/// (see `docs/TODO.md`'s note on why the session can't simply be resumed)
/// that exiting on it would make `watch` worse than useless -- the whole
/// point of watching a kettle is not having to notice it went quiet and
/// restart the command by hand. So a connectivity failure -- a timeout,
/// silence, or a command going unacknowledged, all indistinguishable from
/// "the kettle isn't powered right now" -- is reported, waited through
/// (with backoff; see `retry_with_backoff`), and followed by a fresh
/// connect cycle: re-resolve the endpoint, handshake again with the same
/// token, new session. `connect` already falls back to discovery on its
/// own if the cached address stops answering, exactly as any other
/// command does.
///
/// Only an error retrying can never fix -- a rejected handshake (the token
/// is wrong), a malformed config, anything internal -- ends `watch`, with
/// its usual exit code. `is_connectivity_failure` is the one place that
/// line is drawn.
///
/// A downstream reader going away (`| head -1`, a killed notifier, a
/// closed terminal) is a third kind of ending, distinct from both of the
/// above: `output::print_event`/`output::print_watch_reconnected` report a
/// failed write instead of panicking on it (see their doc comments), and
/// this treats that exactly like the callback asking to stop -- quietly,
/// exit 0, since nobody is left to see either an error or an event.
fn watch(device: &Device, json: bool, config_path: &Path) -> Result<(), AppError> {
    let mut client = connect(device, config_path)?;

    loop {
        let result = client.watch(|event| match output::print_event(&event, json) {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(()),
        });

        let err = match result {
            Ok(()) => return Ok(()),
            Err(err) => AppError::from(err),
        };
        if !is_connectivity_failure(&err) {
            return Err(err);
        }

        eprintln!("d3home: kettle went away ({err}); waiting for it to come back");
        client = retry_with_backoff(
            || connect(device, config_path),
            |backoff| with_spinner(Phase::WaitingToReconnect, || std::thread::sleep(backoff)),
        )?;
        // The device replays its whole post-handshake state burst on
        // every connection; without this marker in the stream, that
        // repeated block of events would look like a glitch rather than
        // what it is. If even this can't be written, the reader is
        // already gone -- stop now rather than reconnect once more only
        // to find the very first event fails the same way.
        if output::print_watch_reconnected(json).is_err() {
            return Ok(());
        }
    }
}

/// Whether `err` is worth waiting through and retrying in `watch`'s
/// reconnect loop, as opposed to something retrying can never fix.
///
/// Connectivity failures -- the device didn't answer, wasn't found on a
/// fresh discovery scan, or (having been connected) went silent or
/// stopped acknowledging -- are all exactly what lifting the kettle off
/// its base looks like from here, and are worth waiting through.
/// `NotFound` belongs in this set for the same reason `connect`'s own
/// `not_found_message` already gives it: a kettle spends much of its life
/// off its base, so "mDNS found nothing" is far more often that than a
/// real network fault. Everything else means the identical attempt would
/// fail the identical way every time: a rejected handshake means the
/// token is wrong, and a `Usage`/`Device`/`Internal` error means something
/// in the config or this process is broken, not the network -- looping on
/// those would only hide a real problem behind a spinner.
fn is_connectivity_failure(err: &AppError) -> bool {
    match err {
        AppError::Timeout(_) | AppError::NotFound(_) => true,
        AppError::Usage(_) | AppError::BadToken | AppError::Device(_) | AppError::Internal(_) => false,
    }
}

/// Retry `attempt` with a growing backoff between failures, until it
/// succeeds or fails for a reason [`is_connectivity_failure`] says
/// retrying cannot fix. `sleep` is taken as a parameter, rather than
/// calling `std::thread::sleep` directly, so a test can replace real
/// waiting with an instant, recorded no-op and still observe the schedule
/// this would have waited on -- without spending the wall-clock time on
/// it.
fn retry_with_backoff<T>(
    mut attempt: impl FnMut() -> Result<T, AppError>,
    mut sleep: impl FnMut(Duration),
) -> Result<T, AppError> {
    let mut backoff = WATCH_RECONNECT_BACKOFF_INITIAL;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) if is_connectivity_failure(&err) => {
                sleep(backoff);
                backoff = (backoff * 2).min(WATCH_RECONNECT_BACKOFF_CEILING);
            }
            Err(err) => return Err(err),
        }
    }
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
/// that simply doesn't answer, or can't even be turned into a socket
/// address at all (`Timeout`, or an I/O failure -- see
/// [`evaluate_cached_attempt`]), is far more likely a DHCP lease having
/// moved the device, or a NIC that got renamed/replugged, than a device
/// that vanished, so both cases fall back to a fresh mDNS lookup and update
/// the cache with whatever it finds.
///
/// With no cache at all, this goes straight to mDNS and caches the result
/// on success.
fn connect(device: &Device, config_path: &Path) -> Result<Client, AppError> {
    // `MdnsDiscovery::new` (binding a multicast socket) is passed as a
    // factory, not called here: see `connect_with`'s doc comment for why.
    connect_with(device, config_path, MdnsDiscovery::new)
}

/// Does the work of [`connect`], but takes a *factory* for the
/// [`Discovery`] implementation to fall back to, rather than an
/// already-constructed one.
///
/// This is the seam the cached-fails -> discover -> re-cache -> connect
/// path was missing: `MdnsDiscovery::new()` used to be constructed inline
/// inside this function, with no way to substitute a fake, so that whole
/// path could only be exercised against a real multicast socket -- which
/// this project's test suite must never do (a MAC that matches nothing is
/// how `tests/cli.rs`'s fixtures stay safe today; this is what actually
/// makes that necessary). A factory rather than a plain `&dyn Discovery`
/// parameter matters for a reason beyond testability, too: constructing
/// `MdnsDiscovery` stands up an mDNS daemon and binds a socket, and the
/// common case here is a cached endpoint that answers immediately --
/// discovery must not be paid for on every call, only on the ones that
/// actually fall back to it.
fn connect_with<D: Discovery>(
    device: &Device,
    config_path: &Path,
    discovery: impl FnOnce() -> Result<D, syncleo::Error>,
) -> Result<Client, AppError> {
    let token = device.token_bytes()?;

    if let Some(cached) = &device.cached {
        let public_wire = decode_public_key(&cached.public_key, &device.name)?;
        match try_cached_endpoint(cached, &device.name, public_wire, token) {
            CachedAttempt::Connected(client) => return Ok(client),
            CachedAttempt::StaleFallBackToDiscovery => {
                // Fall through to discovery below.
            }
            CachedAttempt::Failed(err) => return Err(err),
        }
    }

    let discovery = discovery()?;
    let found = with_spinner(Phase::Searching, || discover_device(&discovery, device))?;
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
    with_spinner(Phase::Connecting, || try_connect(addr, found.public_wire, token)).map_err(Into::into)
}

/// Resolve `cached` into a socket address and attempt the handshake against
/// it, folding both ways a cached endpoint can turn out to be stale into
/// the same discovery-fallback decision: the address doesn't even build
/// (an interface name that no longer resolves -- `cached_socket_addr`
/// returning anything other than the one deliberately-hard `Usage` error
/// below), or it builds but nothing ever answers (`evaluate_cached_attempt`
/// on the handshake attempt itself).
fn try_cached_endpoint(
    cached: &Cached,
    device_name: &str,
    public_wire: [u8; 32],
    token: [u8; 16],
) -> CachedAttempt {
    let addr = match cached_socket_addr(cached, device_name) {
        Ok(addr) => addr,
        // Only one case is a genuinely actionable config problem: a
        // link-local address was cached with no interface ever recorded
        // for it at all (`cached_socket_addr`'s own `Usage` message names
        // the fix). Everything else `cached_socket_addr` can return is an
        // `Io` failure resolving the recorded interface *name* to the
        // OS's current index for it -- exactly what happens when the NIC
        // behind that name is replugged, renamed, or removed, which is
        // precisely the situation storing a name instead of an index
        // exists to survive. Treating that as a permanent, unrecoverable
        // error (as it used to be, surfacing as exit 1 "internal error"
        // forever) would defeat the whole design; falling back to
        // discovery, same as a plain timeout, lets a single fresh mDNS
        // scan repair the cache instead.
        Err(err @ AppError::Usage(_)) => return CachedAttempt::Failed(err),
        Err(_) => return CachedAttempt::StaleFallBackToDiscovery,
    };

    let attempt = with_spinner(Phase::Connecting, || try_connect(addr, public_wire, token));
    evaluate_cached_attempt(attempt)
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
    /// The cached endpoint didn't pan out: either it didn't answer at all,
    /// or it couldn't even be turned into a socket address (a cached
    /// interface name that no longer resolves). Both are far more likely a
    /// stale DHCP lease or a replugged/renamed NIC than a device that
    /// stopped existing, so both are worth a fresh mDNS lookup.
    StaleFallBackToDiscovery,
    /// Anything else, most importantly `HandshakeRejected`: a wrong token
    /// is not a stale address, and retrying via discovery would silently
    /// mask that behind a slow, confusing retry instead of reporting it.
    Failed(AppError),
}

fn evaluate_cached_attempt(result: Result<Client, syncleo::Error>) -> CachedAttempt {
    match result {
        Ok(client) => CachedAttempt::Connected(client),
        // A plain timeout (nothing answered) and an I/O failure (the route
        // is gone, the interface is down, the peer refused the connection)
        // are both what a stale cached endpoint looks like from here --
        // neither means the device itself rejected anything, so both are
        // worth a fresh discovery scan rather than a permanent failure.
        Err(syncleo::Error::Timeout) | Err(syncleo::Error::Io(_)) => CachedAttempt::StaleFallBackToDiscovery,
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

/// Locate `device` on the network by its MAC address, via whichever
/// [`Discovery`] `connect_with` was given.
fn discover_device(discovery: &impl Discovery, device: &Device) -> Result<Found, AppError> {
    discovery
        .find(&device.mac, DISCOVERY_TIMEOUT)?
        .ok_or_else(|| AppError::NotFound(not_found_message(&device.name, &device.mac)))
}

/// The message for "mDNS produced nothing for this MAC within the timeout."
/// Kept as its own pure function, separate from `discover_device`, so the
/// wording can be pinned by a test without a real socket or multicast
/// traffic.
///
/// This is where the real kettle's evidence lives: it was lifted off its
/// base mid-session, the cached endpoint stopped answering, discovery came
/// back empty, and the honest-but-unhelpful message at the time was just
/// "was not found on the network." A kettle spends much of its life off
/// its base -- and therefore unpowered -- so that is by far the likeliest
/// reason this fires, more likely than an actual network fault. The
/// message says so without asserting it as fact: the device could still be
/// powered and merely unreachable.
fn not_found_message(name: &str, mac: &str) -> String {
    format!(
        "device '{name}' (mac {mac}) was not found on the network\n\
         a kettle that has been lifted off its base is unpowered and won't answer -- that's \
         the likeliest reason here, though a real network problem is still possible"
    )
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
    // See `config::hex_decode`'s doc comment: this used to byte-slice `hex`
    // directly (`&hex[i * 2..i * 2 + 2]`), which panics if the string is
    // the right *byte* length but contains a multi-byte character at an
    // even offset -- the identical bug already fixed for `token_bytes`.
    hex_decode::<32>(hex)
        .ok_or_else(|| AppError::Usage(format!("device '{device_name}' has a malformed cached public key")))
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
        assert_eq!(parse_target_temperature(&["30".to_string()]).unwrap(), Some(30));
        assert_eq!(parse_target_temperature(&["100".to_string()]).unwrap(), Some(100));
    }

    #[test]
    fn a_temperature_not_a_multiple_of_five_is_still_accepted() {
        // The vendor app only *offers* multiples of five; nothing says the
        // device itself enforces that step. Rejecting 83 client-side would
        // be a guess this project has no evidence for.
        assert_eq!(parse_target_temperature(&["83".to_string()]).unwrap(), Some(83));
    }

    #[test]
    fn a_temperature_outside_the_range_is_a_usage_error() {
        let err = parse_target_temperature(&["250".to_string()]).unwrap_err();
        assert_eq!(err.exit_code(), crate::cli::ExitCode::Usage);
        assert!(err.to_string().contains("30"));

        assert!(parse_target_temperature(&["29".to_string()]).is_err());
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
    fn the_not_found_message_keeps_the_mac_and_names_the_unpowered_kettle_case() {
        // Pinned loosely on purpose: this checks the load-bearing content
        // (the mac, and the off-base/unpowered hint) survives a future
        // rewording, not the exact sentence.
        let message = not_found_message("kettle", "aabbccddeeff");
        assert!(message.contains("aabbccddeeff"), "mac missing from: {message}");
        assert!(message.contains("kettle"), "device name missing from: {message}");
        let lower = message.to_lowercase();
        assert!(lower.contains("base"), "off-base hint missing from: {message}");
        assert!(lower.contains("unpowered") || lower.contains("power"), "power hint missing from: {message}");
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
    fn a_multibyte_cached_public_key_that_is_the_right_byte_length_does_not_panic() {
        // Same mechanism as config::tests's token regression: 21 3-byte "€"
        // characters plus one ASCII byte is exactly 64 bytes, so the old
        // `hex.len() != 64` guard passed, but slicing into it panicked
        // mid-character.
        let hex = "€".repeat(21) + "a";
        assert_eq!(hex.len(), 64, "fixture must be exactly 64 bytes to reach the old guard");

        let err = decode_public_key(&hex, "kettle").unwrap_err();
        assert_eq!(err.exit_code(), crate::cli::ExitCode::Usage);
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

    #[test]
    fn an_io_failure_on_the_cached_endpoint_falls_back_to_discovery() {
        // Finding 3: an interface that no longer resolves (ENODEV, the NIC
        // was renamed/replugged/removed) or a route that's gone
        // (ENETUNREACH) surfaces as Error::Io from `try_connect`. This used
        // to be treated as `Failed` -- a permanent exit-1 "internal error"
        // with no fallback, even though this is exactly the "stale cached
        // endpoint" case discovery exists to repair.
        assert!(matches!(
            evaluate_cached_attempt(Err(syncleo::Error::Io(std::io::Error::other("no such device")))),
            CachedAttempt::StaleFallBackToDiscovery
        ));
    }

    struct FakeDiscovery(Option<Found>);

    impl Discovery for FakeDiscovery {
        fn find_all(&self, _timeout: Duration) -> Result<Vec<Found>, syncleo::Error> {
            Ok(self.0.clone().into_iter().collect())
        }
        fn find(&self, mac: &str, _timeout: Duration) -> Result<Option<Found>, syncleo::Error> {
            Ok(self.0.clone().filter(|f| f.mac == mac))
        }
    }

    #[test]
    fn a_cached_endpoint_with_a_stale_interface_falls_back_to_discovery_and_reconnects() {
        // The structural gap the analysis called out: `discover_device`
        // used to construct `MdnsDiscovery::new()` inline, so the whole
        // cached-fails -> discover -> re-cache -> connect path had no test
        // coverage without a real multicast socket. `connect_with`'s
        // injected `Discovery` factory closes that gap. This drives the
        // exact scenario A from finding 3: a cached link-local address
        // whose recorded interface name no longer resolves must fall back
        // to discovery (here: a fake one, pointing at a real simulator on
        // loopback) rather than dying as a permanent internal error.
        const TOKEN: [u8; 16] = [
            0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
        ];
        let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();

        let dir = std::env::temp_dir()
            .join(format!("d3home-test-stale-interface-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(
            &path,
            format!(
                "[[devices]]\nname = \"kettle\"\ndriver = \"syncleo\"\nmac = \"aabbccddeeff\"\n\
                 token = \"{}\"\n\n[devices.cached]\naddress = \"fe80::dead:beef:dead:beef\"\n\
                 port = 8888\npublic_key = \"{}\"\ninterface = \"d3home-no-such-iface\"\n",
                hex_encode(&TOKEN),
                "ab".repeat(32),
            ),
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        let device = config.resolve("kettle").unwrap();

        let found = Found {
            mac: "aabbccddeeff".into(),
            address: handle.addr.ip(),
            interface: None,
            port: handle.addr.port(),
            public_wire: handle.public_wire,
            curve: 29,
            protocol: 2,
        };

        let client = connect_with(device, &path, || Ok::<_, syncleo::Error>(FakeDiscovery(Some(found))));
        assert!(client.is_ok(), "must fall back to the injected discovery and connect: {:?}", client.err());

        let reloaded = Config::load(&path).unwrap();
        let cached = reloaded.resolve("kettle").unwrap().cached.as_ref().expect("must re-cache on success");
        assert_eq!(cached.address, handle.addr.ip());
        assert_eq!(cached.port, handle.addr.port());
        assert_eq!(cached.interface, None, "the freshly discovered endpoint needs no interface");

        handle.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cached_endpoint_with_a_stale_interface_and_nothing_discoverable_is_not_found() {
        // The other half of the same gap: when discovery *also* comes up
        // empty, the final error must be the ordinary "not found" a caller
        // already knows how to wait through in `watch` -- not the stale
        // Internal/exit-1 error the cached endpoint alone used to produce.
        const TOKEN: [u8; 16] = [0xb0; 16];
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-stale-interface-no-discovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(
            &path,
            format!(
                "[[devices]]\nname = \"kettle\"\ndriver = \"syncleo\"\nmac = \"aabbccddeeff\"\n\
                 token = \"{}\"\n\n[devices.cached]\naddress = \"fe80::dead:beef:dead:beef\"\n\
                 port = 8888\npublic_key = \"{}\"\ninterface = \"d3home-no-such-iface\"\n",
                hex_encode(&TOKEN),
                "ab".repeat(32),
            ),
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        let device = config.resolve("kettle").unwrap();

        let err = connect_with(device, &path, || Ok::<FakeDiscovery, syncleo::Error>(FakeDiscovery(None)))
            .expect_err("nothing was ever discoverable");
        assert_eq!(err.exit_code(), crate::cli::ExitCode::NotFound);

        std::fs::remove_dir_all(&dir).ok();
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
    fn a_cached_link_local_address_with_an_unresolvable_interface_is_not_a_hard_usage_error() {
        // Finding 3: this is *not* the "never had an interface at all"
        // case above (a genuine config problem `cached_socket_addr`
        // deliberately reports loudly). An interface name that used to
        // resolve but no longer does (the NIC was renamed, replugged, or
        // removed) is exactly what the interface-*name* cache design
        // exists to survive: `try_cached_endpoint` must recognize this as
        // something other than `AppError::Usage` so it falls back to
        // discovery instead of failing outright.
        let cached = Cached {
            address: "fe80::dead:beef:dead:beef".parse().unwrap(),
            port: 8888,
            public_key: "ab".repeat(32),
            interface: Some("d3home-no-such-iface".into()),
        };

        let err = cached_socket_addr(&cached, "kettle").unwrap_err();
        assert_ne!(
            err.exit_code(),
            crate::cli::ExitCode::Usage,
            "an unresolvable interface must not be reported as the same hard failure as no \
             interface at all: {err}"
        );
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
    fn connectivity_failures_are_exactly_timeout_and_not_found() {
        assert!(is_connectivity_failure(&AppError::Timeout("gone".into())));
        assert!(is_connectivity_failure(&AppError::NotFound("gone".into())));
    }

    #[test]
    fn a_rejected_handshake_a_bad_config_and_a_device_error_are_never_retried() {
        // These are exactly the errors retrying can never fix -- a wrong
        // token, a broken config, this process itself being wrong -- and
        // must end `watch` outright rather than feed the reconnect loop.
        assert!(!is_connectivity_failure(&AppError::BadToken));
        assert!(!is_connectivity_failure(&AppError::Usage("bad config".into())));
        assert!(!is_connectivity_failure(&AppError::Device("nak".into())));
        assert!(!is_connectivity_failure(&AppError::Internal("bug".into())));
    }

    #[test]
    fn retry_with_backoff_returns_ok_immediately_on_the_first_success_without_sleeping() {
        let mut calls = 0;
        let mut sleeps: Vec<Duration> = Vec::new();
        let result: Result<i32, AppError> =
            retry_with_backoff(|| { calls += 1; Ok(42) }, |d| sleeps.push(d));

        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls, 1);
        assert!(sleeps.is_empty(), "a first-try success must never wait at all");
    }

    #[test]
    fn a_fatal_error_during_a_watch_retry_ends_the_loop_instead_of_looping_forever() {
        // The scenario the design brief calls out by name: a handshake
        // rejected while `watch` is retrying must exit -- via
        // `AppError::BadToken`, which `cli::ExitCode` maps to 4 -- rather
        // than being treated as one more connectivity hiccup to wait out.
        let mut attempts = 0;
        let mut sleeps: Vec<Duration> = Vec::new();
        let result: Result<(), AppError> = retry_with_backoff(
            || {
                attempts += 1;
                if attempts <= 2 { Err(AppError::Timeout("still gone".into())) } else { Err(AppError::BadToken) }
            },
            |d| sleeps.push(d),
        );

        assert_eq!(attempts, 3, "must stop trying the moment a fatal error appears");
        assert_eq!(result.unwrap_err().exit_code(), crate::cli::ExitCode::BadToken);
        assert_eq!(sleeps.len(), 2, "one wait per retryable failure, none after the fatal one");
    }

    #[test]
    fn retry_with_backoff_waits_between_every_attempt_growing_up_to_the_ceiling() {
        // "Does not spin hot": every one of these waits must be a real,
        // non-zero delay, and the schedule must actually grow -- not fire
        // back-to-back attempts with the caller's `sleep` reduced to a
        // no-op.
        let mut attempts = 0;
        let mut sleeps: Vec<Duration> = Vec::new();
        let result: Result<(), AppError> = retry_with_backoff(
            || {
                attempts += 1;
                if attempts <= 4 { Err(AppError::Timeout("still gone".into())) } else { Err(AppError::Usage("stop".into())) }
            },
            |d| sleeps.push(d),
        );

        assert!(result.is_err());
        assert_eq!(sleeps.len(), 4);
        assert!(sleeps.iter().all(|d| !d.is_zero()), "every backoff must actually delay: {sleeps:?}");
        assert_eq!(sleeps[0], WATCH_RECONNECT_BACKOFF_INITIAL);
        assert_eq!(sleeps[1], WATCH_RECONNECT_BACKOFF_INITIAL * 2);
        assert_eq!(sleeps[2], WATCH_RECONNECT_BACKOFF_INITIAL * 4);
        // The fourth wait would be INITIAL * 8 (4s) uncapped, which is
        // still under the 5s ceiling here -- pinned separately below at a
        // point that actually crosses it.
        for pair in sleeps.windows(2) {
            assert!(pair[1] >= pair[0], "backoff must never shrink: {sleeps:?}");
        }
    }

    #[test]
    fn the_backoff_never_exceeds_its_ceiling() {
        let mut attempts = 0;
        let mut sleeps: Vec<Duration> = Vec::new();
        let _: Result<(), AppError> = retry_with_backoff(
            || {
                attempts += 1;
                if attempts <= 8 { Err(AppError::Timeout("still gone".into())) } else { Err(AppError::BadToken) }
            },
            |d| sleeps.push(d),
        );

        assert_eq!(sleeps.len(), 8);
        assert!(
            sleeps.iter().all(|d| *d <= WATCH_RECONNECT_BACKOFF_CEILING),
            "backoff must be capped at the ceiling: {sleeps:?}"
        );
        assert_eq!(*sleeps.last().unwrap(), WATCH_RECONNECT_BACKOFF_CEILING, "it should have reached the cap by the 8th wait");
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
