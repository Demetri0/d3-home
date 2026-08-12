//! The built-in, device-agnostic commands: `discover`, `devices`, `alias`,
//! and `help`. None of these know anything about a specific device driver
//! -- that is `commands::kettle`'s job, and a future driver's.

use std::path::Path;
use std::time::Duration;

use syncleo::discovery::{Discovery, MdnsDiscovery};

use crate::cli::AppError;
use crate::config::{Config, ConfigError};
use crate::output;

/// How long `discover` scans before reporting what it found.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Scan the local network for every advertised device.
pub fn discover(json: bool) -> Result<(), AppError> {
    let discovery = MdnsDiscovery::new()?;
    let found = discovery.find_all(DISCOVERY_TIMEOUT)?;
    output::print_found(&found, json);
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
