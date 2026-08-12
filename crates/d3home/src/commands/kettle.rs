//! What `d3home <kettle-or-alias> <action>` does. Everything here is
//! specific to the `"syncleo"` driver; a second device type (a vacuum, say)
//! would get its own sibling module rather than teaching `cli::parse` or
//! `main::dispatch` a new device type -- the action words are only ever
//! interpreted here, by the driver that owns them.

use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::time::Duration;

use syncleo::client::Client;
use syncleo::codec::command::{Command, PowerMode};
use syncleo::discovery::{Discovery, MdnsDiscovery};
use syncleo::transport::UdpTransport;

use crate::cli::AppError;
use crate::config::Device;
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
/// How long `status` listens for the device's state reports. The protocol
/// has no "query state" command; the device reports its full state,
/// unprompted, right after the handshake, so this only needs to be long
/// enough to catch that burst.
const STATUS_WINDOW: Duration = Duration::from_millis(500);

/// Run one kettle action. `action` is whatever followed the device name on
/// the command line, unexamined until now.
pub fn run(device: &Device, action: &[String], json: bool) -> Result<(), AppError> {
    let (verb, rest) = action
        .split_first()
        .ok_or_else(|| AppError::Usage("missing action; try status, start, off, or watch".into()))?;

    match verb.as_str() {
        "status" => status(device, json),
        "start" => start(device, rest),
        "off" => off(device),
        "watch" => watch(device, json),
        other => Err(AppError::Usage(format!(
            "unknown kettle action '{other}'; try status, start, off, or watch"
        ))),
    }
}

fn status(device: &Device, json: bool) -> Result<(), AppError> {
    let mut client = connect(device)?;
    let state = client.collect_state(STATUS_WINDOW)?;
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

fn start(device: &Device, args: &[String]) -> Result<(), AppError> {
    // Validated before connecting: a bad temperature should fail instantly,
    // not after a network round trip that was always going to be wasted.
    let target = parse_target_temperature(args)?;

    let mut client = connect(device)?;
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

fn off(device: &Device) -> Result<(), AppError> {
    let mut client = connect(device)?;
    client.send(Command::Mode(PowerMode::Off))?;
    Ok(())
}

/// Stream device events until the connection is lost (or the process is
/// killed). Each event is handed straight to `output::print_event` as it
/// arrives -- no buffering -- so this stays a genuine stream: a future
/// caller (a notifier, a TUI) can swap in a different callback without
/// this function changing shape.
fn watch(device: &Device, json: bool) -> Result<(), AppError> {
    let mut client = connect(device)?;
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

/// Perform the handshake with `device`: the cached endpoint if the config
/// has one, otherwise a fresh mDNS lookup by MAC.
fn connect(device: &Device) -> Result<Client, AppError> {
    let token = device.token_bytes()?;
    let (addr, public_wire) = endpoint(device)?;

    let transport = UdpTransport::connect(addr)?;
    // A fresh key pair per connection: nothing in the protocol expects our
    // side's private key to be stable across runs, and generating one lets
    // the CLI stay stateless between invocations.
    let our_private = rand::random::<[u8; 32]>();

    Ok(Client::connect(Box::new(transport), our_private, public_wire, token, CONNECT_TIMEOUT)?)
}

/// Where to reach `device`, and the public key needed to derive the
/// session keys: from its cached endpoint if it has one, otherwise from a
/// fresh mDNS scan by MAC address.
fn endpoint(device: &Device) -> Result<(SocketAddr, [u8; 32]), AppError> {
    if let Some(cached) = &device.cached {
        let public_wire = decode_public_key(&cached.public_key, &device.name)?;
        Ok((SocketAddr::new(cached.address, cached.port), public_wire))
    } else {
        let discovery = MdnsDiscovery::new()?;
        let found = discovery.find(&device.mac, DISCOVERY_TIMEOUT)?.ok_or_else(|| {
            AppError::NotFound(format!(
                "device '{}' (mac {}) was not found on the network",
                device.name, device.mac
            ))
        })?;
        Ok((SocketAddr::new(found.address, found.port), found.public_wire))
    }
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
}
