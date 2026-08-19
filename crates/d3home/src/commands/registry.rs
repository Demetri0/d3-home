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
        crate::output::print_warning(&format!("could not update the device cache: {err}"));
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
        // Nothing configured yet is the normal state before the first
        // device is added; discovery is still useful, it just has no
        // registry to write its findings into.
        Err(ConfigError::NoRegistry { .. }) => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let mut changed = false;
    for device in &mut config.devices {
        if let Some(f) = found.iter().find(|f| f.mac == device.mac) {
            device.cached = Some(Cached {
                address: f.address,
                port: f.port,
                public_key: hex_encode(&f.public_wire),
                interface: f.interface.clone(),
            });
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
    print!("{}", help_text());
}

/// The help text, as a string so a test can assert it still mentions every
/// command that exists. Help drifting out of step with the program is the
/// usual failure here, and it is silent.
pub fn help_text() -> String {
    let mut out = String::new();
    out.push_str("d3home -- control smart home devices over the local network\n\n");
    out.push_str("USAGE:\n");
    out.push_str("    d3home <device-or-alias> <action> [args...]\n");
    out.push_str("    d3home <builtin> [args...]\n\n");
    out.push_str("The first word is a device name or any alias you gave it, taken from\n");
    out.push_str("your config -- so `d3home k start 80` works once `k` is an alias.\n\n");
    out.push_str("KETTLE ACTIONS:\n");
    out.push_str("    status              show mode, temperature and flags\n");
    out.push_str("    start, on           heat to 100 C\n");
    out.push_str("    start <temp>        heat to <temp> C\n");
    out.push_str("    set <temp>          set the target without starting, or retarget\n");
    out.push_str("                        a heat already running\n");
    out.push_str("    off, stop           stop heating\n");
    out.push_str("    watch               stream events until q or Ctrl-C; reconnects\n");
    out.push_str("                        by itself when the kettle is put back\n");
    out.push_str("    trace               every report the device makes, with protocol\n");
    out.push_str("                        codes -- for taking the protocol apart\n\n");
    out.push_str("BUILTINS:\n");
    out.push_str("    add [link]          register a device: paste the share link from\n");
    out.push_str("                        the vendor app, or answer prompts, or pass\n");
    out.push_str("                        --name/--mac/--token\n");
    out.push_str("    discover            find devices on the local network\n");
    out.push_str("    devices             list what is configured\n");
    out.push_str("    alias add <a> <d>   give device <d> the alias <a>\n");
    out.push_str("    alias rm <a>        remove alias <a>\n");
    out.push_str("    completions <shell> print a completion script for bash, zsh or fish\n");
    out.push_str("    help                this text\n\n");
    out.push_str("OPTIONS (accepted in any position):\n");
    out.push_str("    --json              machine-readable output\n");
    out.push_str("    --device <name>     device to act on, instead of the first word\n");
    out.push_str("    --config <path>     device registry to use\n");
    out.push_str("    -h, --help          this text\n");
    out.push_str("    -V, --version       print the version and exit\n\n");
    out.push_str("EXIT CODES:\n");
    out.push_str("    0 ok   1 internal   2 usage   3 not found\n");
    out.push_str("    4 wrong token   5 timeout   6 device error\n");
    out
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sample_kettle_toml() -> String {
        "[[devices]]\nname = \"kettle\"\ndriver = \"syncleo\"\nmac = \"deadbeefdead\"\ntoken = \"deadbeefdeadbeefdeadbeefdeadbeef\"\n".into()
    }

    fn found(mac: &str) -> Found {
        Found {
            mac: mac.into(),
            address: Ipv4Addr::new(192, 168, 1, 99).into(),
            interface: None,
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

        cache_discovered(&path, &[found("deadbeefdead")]).unwrap();

        let reloaded = Config::load(&path).unwrap();
        let cached = reloaded.resolve("kettle").unwrap().cached.as_ref().expect("cache was written");
        assert_eq!(cached.port, 9999);
        assert_eq!(cached.public_key, hex_encode(&[0x55; 32]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_caches_the_interface_of_a_link_local_device() {
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-cache-discovered-interface-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, sample_kettle_toml()).unwrap();

        let mut link_local = found("deadbeefdead");
        link_local.address = "fe80::dead:beef:dead:beef".parse().unwrap();
        link_local.interface = Some("enp8s0".into());
        cache_discovered(&path, &[link_local]).unwrap();

        let reloaded = Config::load(&path).unwrap();
        let cached = reloaded.resolve("kettle").unwrap().cached.as_ref().expect("cache was written");
        assert_eq!(cached.interface.as_deref(), Some("enp8s0"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_ignores_a_device_that_does_not_match_any_configured_mac() {
        let dir = std::env::temp_dir()
            .join(format!("d3home-test-cache-discovered-nomatch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, sample_kettle_toml()).unwrap();

        // A MAC deliberately different from the configured device, so the
        // discovered result must be ignored rather than cached.
        cache_discovered(&path, &[found("deadbeefcafe")]).unwrap();

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

        assert!(cache_discovered(&path, &[found("deadbeefdead")]).is_ok());
    }
}
