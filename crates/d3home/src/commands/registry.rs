//! The built-in, device-agnostic commands: `discover`, `devices`, `alias`,
//! and `help`. None of these know anything about a specific device driver
//! -- that is `commands::kettle`'s job, and a future driver's.

use std::path::Path;
use std::time::Duration;

use syncleo::discovery::{Discovery, Found, MdnsDiscovery};

use crate::cli::AppError;
use crate::config::{Cached, Config, ConfigError, hex_encode};
use crate::output;

/// How long `discover` scans before reporting what it found.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Scan the local network for every advertised device, print what was
/// found (including each device's public key, which is what the config's
/// `[devices.cached]` escape hatch needs when mDNS is blocked and the
/// operator has to hand-write the cache themselves), and cache the
/// endpoint of anything that matches an already-configured device.
pub fn discover(config_path: &Path, json: bool) -> Result<(), AppError> {
    let discovery = MdnsDiscovery::new()?;
    let found = discovery.find_all(DISCOVERY_TIMEOUT)?;
    output::print_found(&found, json);

    if let Err(err) = cache_discovered(config_path, &found) {
        eprintln!("d3home: warning: could not update the device cache: {err}");
    }
    Ok(())
}

/// Update `[devices.cached]` for every configured device (matched by MAC)
/// that turned up in `found`. Missing the config file entirely is not an
/// error here -- `discover` is useful before any device has ever been
/// configured -- but any other failure (a malformed config, a save that
/// could not complete) is reported so `discover` can warn about it.
fn cache_discovered(config_path: &Path, found: &[Found]) -> Result<(), AppError> {
    let mut config = match Config::load(config_path) {
        Ok(config) => config,
        Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let mut changed = false;
    for device in &mut config.devices {
        if let Some(f) = found.iter().find(|f| f.mac == device.mac) {
            device.cached =
                Some(Cached { address: f.address, port: f.port, public_key: hex_encode(&f.public_wire) });
            changed = true;
        }
    }

    if changed {
        config.save(config_path)?;
    }
    Ok(())
}

/// List the configured devices.
pub fn devices(config: &Config, json: bool) {
    output::print_devices(config, json);
}

/// Add `alias` to `device` and persist it. Refuses (via `Config::validate`)
/// an alias that collides with a built-in command, another device's name,
/// or another device's alias -- the same three checks Task 10 already
/// enforces on every load.
pub fn alias_add(config_path: &Path, alias: &str, device: &str) -> Result<(), AppError> {
    let mut config = Config::load(config_path)?;
    let target = config
        .devices
        .iter_mut()
        .find(|d| d.name == device)
        .ok_or_else(|| ConfigError::UnknownDevice { name: device.to_string() })?;
    target.aliases.push(alias.to_string());

    config.validate()?;
    config.save(config_path)?;
    Ok(())
}

/// Remove `alias` from whichever device has it.
pub fn alias_rm(config_path: &Path, alias: &str) -> Result<(), AppError> {
    let mut config = Config::load(config_path)?;
    let owner = config.devices.iter_mut().find(|d| d.aliases.iter().any(|a| a == alias));

    match owner {
        Some(device) => device.aliases.retain(|a| a != alias),
        None => return Err(AppError::Usage(format!("no alias '{alias}' is defined"))),
    }

    config.save(config_path)?;
    Ok(())
}

pub fn help() {
    println!("d3home -- control smart home devices over the local network\n");
    println!("USAGE:");
    println!("    d3home [--config <path>] [--json] <device-or-alias> <action> [args...]");
    println!("    d3home discover");
    println!("    d3home devices");
    println!("    d3home alias add <alias> <device>");
    println!("    d3home alias rm <alias>");
    println!();
    println!("Kettle actions: status, start [temperature], off, watch");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sample_kettle_toml() -> String {
        "[[devices]]\nname = \"kettle\"\ndriver = \"syncleo\"\nmac = \"aabbccddeeff\"\ntoken = \"a0a1a2a3a4a5a6a7a8a9aaabacadaeaf\"\n".into()
    }

    fn found(mac: &str) -> Found {
        Found {
            mac: mac.into(),
            address: Ipv4Addr::new(192, 168, 1, 99).into(),
            port: 9999,
            public_wire: [0x55; 32],
            curve: 29,
            protocol: 2,
        }
    }

    #[test]
    fn discover_caches_the_endpoint_of_a_matching_configured_device() {
        let dir =
            std::env::temp_dir().join(format!("d3home-test-cache-discovered-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, sample_kettle_toml()).unwrap();

        cache_discovered(&path, &[found("aabbccddeeff")]).unwrap();

        let reloaded = Config::load(&path).unwrap();
        let cached = reloaded.resolve("kettle").unwrap().cached.as_ref().expect("cache was written");
        assert_eq!(cached.port, 9999);
        assert_eq!(cached.public_key, hex_encode(&[0x55; 32]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_ignores_a_device_that_does_not_match_any_configured_mac() {
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-cache-discovered-nomatch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, sample_kettle_toml()).unwrap();

        cache_discovered(&path, &[found("aabbccddeeff")]).unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert!(reloaded.resolve("kettle").unwrap().cached.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_without_a_config_file_yet_is_not_an_error() {
        // `discover` is useful before any device has ever been configured;
        // a missing config file must not turn into a warning on every run.
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-cache-discovered-missing-{}", std::process::id()));
        let path = dir.join("devices.toml");

        assert!(cache_discovered(&path, &[found("aabbccddeeff")]).is_ok());
    }
}
