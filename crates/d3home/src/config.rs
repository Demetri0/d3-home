//! The device registry: user-defined devices and their aliases, loaded from
//! and saved to a TOML file. See `default_path` for where that file lives.
//!
//! This module's public API is exercised by its own test suite; main.rs does
//! not wire it into a CLI yet (that is Task 11), so some items have no
//! caller within this crate today. Silence the resulting dead-code warnings
//! rather than inventing premature CLI behaviour to use them.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Built-in subcommands that a device alias must never shadow.
pub const RESERVED: &[&str] = &["discover", "devices", "alias", "help"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub driver: String,
    pub model: Option<String>,
    pub mac: String,
    pub token: String,
    pub cached: Option<Cached>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cached {
    pub address: IpAddr,
    pub port: u16,
    pub public_key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot access the config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("cannot parse the config file: {0}")]
    Parse(String),

    #[error("alias '{alias}' on device '{device}' collides with a built-in command")]
    ReservedAlias { alias: String, device: String },

    #[error("alias '{alias}' is used by both '{first}' and '{second}'")]
    DuplicateAlias {
        alias: String,
        first: String,
        second: String,
    },

    #[error("alias '{alias}' on device '{device}' shadows another device's name")]
    AliasShadowsDevice { alias: String, device: String },

    #[error("device '{device}' has a malformed token")]
    BadToken { device: String },

    #[error("no device matches '{name}'")]
    UnknownDevice { name: String },
}

impl Config {
    /// Load and validate the registry from `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        let config: Config =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Check the three conflicts a device registry must never contain: an
    /// alias that shadows a built-in command, an alias reused across two
    /// devices, and an alias that collides with another device's name.
    ///
    /// Reserved-word aliases are checked first, over every device, before any
    /// duplicate/shadow check runs; which of two simultaneous violations is
    /// reported first is otherwise unspecified and not meant to be relied on.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for device in &self.devices {
            for alias in &device.aliases {
                if RESERVED.contains(&alias.as_str()) {
                    return Err(ConfigError::ReservedAlias {
                        alias: alias.clone(),
                        device: device.name.clone(),
                    });
                }
            }
        }

        let device_names: HashSet<&str> = self.devices.iter().map(|d| d.name.as_str()).collect();
        let mut owners: HashMap<&str, &str> = HashMap::new();

        for device in &self.devices {
            for alias in &device.aliases {
                if let Some(&first) = owners.get(alias.as_str()) {
                    return Err(ConfigError::DuplicateAlias {
                        alias: alias.clone(),
                        first: first.to_string(),
                        second: device.name.clone(),
                    });
                }
                if alias.as_str() != device.name.as_str() && device_names.contains(alias.as_str())
                {
                    return Err(ConfigError::AliasShadowsDevice {
                        alias: alias.clone(),
                        device: device.name.clone(),
                    });
                }
                owners.insert(alias.as_str(), device.name.as_str());
            }
        }

        Ok(())
    }

    /// Find a device by its name or by any of its aliases.
    pub fn resolve(&self, name: &str) -> Option<&Device> {
        self.devices
            .iter()
            .find(|d| d.name == name || d.aliases.iter().any(|a| a == name))
    }

    /// Write the registry to `path` with owner-only permissions: it holds
    /// device tokens. Creates the parent directory if it does not exist yet.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())?;
        // Belt and suspenders: `mode()` above only applies when the file is
        // freshly created, so pin the permissions explicitly in case a file
        // from a previous, looser-permissioned run is still sitting there.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;

        Ok(())
    }

    /// `$XDG_CONFIG_HOME/d3home/devices.toml`, falling back to
    /// `~/.config/d3home/devices.toml` when that variable is unset or empty.
    pub fn default_path() -> PathBuf {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                home.join(".config")
            });
        config_home.join("d3home").join("devices.toml")
    }
}

impl Device {
    /// Decode the hex-encoded token into the 16 raw bytes the protocol uses.
    pub fn token_bytes(&self) -> Result<[u8; 16], ConfigError> {
        let bad_token = || ConfigError::BadToken {
            device: self.name.clone(),
        };

        if self.token.len() != 32 {
            return Err(bad_token());
        }

        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let hex_pair = &self.token[i * 2..i * 2 + 2];
            *byte = u8::from_str_radix(hex_pair, 16).map_err(|_| bad_token())?;
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    const KETTLE: &str = r#"
[[devices]]
name = "kettle"
aliases = ["k", "чайник"]
driver = "syncleo"
mac = "aabbccddeeff"
token = "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"
"#;

    #[test]
    fn resolves_a_device_by_name_or_alias() {
        let config = parse(KETTLE).unwrap();

        assert_eq!(config.resolve("kettle").unwrap().name, "kettle");
        assert_eq!(config.resolve("k").unwrap().name, "kettle");
        assert_eq!(config.resolve("чайник").unwrap().name, "kettle");
        assert!(config.resolve("teapot").is_none());
    }

    #[test]
    fn reads_the_cached_endpoint_when_present() {
        let config = parse(&format!(
            "{KETTLE}\n[devices.cached]\naddress = \"192.168.1.42\"\nport = 8888\npublic_key = \"ab\"\n"
        ))
        .unwrap();

        let cached = config.resolve("kettle").unwrap().cached.as_ref().unwrap();
        assert_eq!(cached.port, 8888);
    }

    #[test]
    fn refuses_an_alias_that_shadows_a_builtin_command() {
        // Silently losing `d3home discover` to an alias would be a nasty surprise.
        let toml = KETTLE.replace(r#"["k", "чайник"]"#, r#"["discover"]"#);
        assert!(matches!(parse(&toml), Err(ConfigError::ReservedAlias { .. })));
    }

    #[test]
    fn refuses_the_same_alias_on_two_devices() {
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"other\"\naliases = [\"k\"]\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"a0a1a2a3a4a5a6a7a8a9aaabacadaeaf\"\n"
        );
        assert!(matches!(parse(&toml), Err(ConfigError::DuplicateAlias { .. })));
    }

    #[test]
    fn refuses_an_alias_that_shadows_another_device_name() {
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"other\"\naliases = [\"kettle\"]\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"a0a1a2a3a4a5a6a7a8a9aaabacadaeaf\"\n"
        );
        assert!(matches!(parse(&toml), Err(ConfigError::AliasShadowsDevice { .. })));
    }

    #[test]
    // The brief's assertion form (`matches!(.., Err(_))`) is kept verbatim;
    // clippy would rather see `.is_err()`.
    #[allow(clippy::redundant_pattern_matching)]
    fn parses_the_token_into_sixteen_bytes() {
        let config = parse(KETTLE).unwrap();
        assert_eq!(config.resolve("k").unwrap().token_bytes().unwrap(), [
            0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
            0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
        ]);

        let bad = KETTLE.replace("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf", "nothex");
        assert!(matches!(parse(&bad).unwrap().resolve("k").unwrap().token_bytes(), Err(_)));
    }

    #[test]
    fn saves_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("d3home-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");

        parse(KETTLE).unwrap().save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the file holds a device secret");

        std::fs::remove_dir_all(&dir).ok();
    }
}
