//! The device registry: user-defined devices and their aliases, loaded from
//! and saved to a TOML file. See `default_path` for where that file lives.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Built-in subcommands that a device alias must never shadow.
pub const RESERVED: &[&str] = &["discover", "devices", "alias", "help"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub devices: Vec<Device>,
}

#[derive(Clone, Serialize, Deserialize)]
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

/// Hand-written so the token never reaches a `{:?}`, `dbg!`, or log line: it
/// is the key to the device. Every other field prints normally; `Config`'s
/// derived `Debug` inherits this redaction automatically through `Vec<Device>`.
impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .field("driver", &self.driver)
            .field("model", &self.model)
            .field("mac", &self.mac)
            .field("token", &"<redacted>")
            .field("cached", &self.cached)
            .finish()
    }
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

    /// Deliberately just a message plus, when available, a line/column --
    /// never the raw `toml` error's `Display`. `toml`'s own error message
    /// quotes the offending source line verbatim, and the offending line
    /// can be a malformed `token = "..."`; echoing that would print a
    /// device secret to whatever reads this error (a terminal, a log, shell
    /// history). `parse_error` below is the only place this is built from a
    /// live `toml::de::Error` encountered while loading a real config; it
    /// is also built directly from a `toml::ser::Error`'s `Display` in
    /// [`Config::save`] (serialisation failures do not echo source text,
    /// so that is not a leak path) and, in `#[cfg(test)]` helpers only,
    /// from `toml::de::Error`'s own `Display` for brevity.
    #[error("cannot parse the config file: {0}")]
    Parse(String),

    #[error("device '{name}' is declared more than once")]
    DuplicateDevice { name: String },

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
        let config: Config = toml::from_str(&text).map_err(|e| parse_error(&e, &text))?;
        config.validate()?;
        Ok(config)
    }

    /// Check the four conflicts a device registry must never contain: two
    /// devices sharing a name, an alias that shadows a built-in command, an
    /// alias reused across two devices, and an alias that collides with
    /// another device's name.
    ///
    /// Duplicate device names are checked first, then reserved-word aliases
    /// over every device, before any duplicate/shadow alias check runs;
    /// which of two simultaneous violations is reported first is otherwise
    /// unspecified and not meant to be relied on.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut seen_names: HashSet<&str> = HashSet::new();
        for device in &self.devices {
            if !seen_names.insert(device.name.as_str()) {
                return Err(ConfigError::DuplicateDevice {
                    name: device.name.clone(),
                });
            }
        }

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
    ///
    /// Writes to a temp file created directly with mode 0600 in the same
    /// directory as `path`, then renames it over the target. `mode()` on
    /// `OpenOptions` only governs permissions at *creation*, so overwriting
    /// an existing file in place would write the fresh token to disk before
    /// a follow-up chmod ever ran, briefly exposing it under the old file's
    /// permissions. The rename is atomic on the same filesystem, so a reader
    /// never observes a half-written config either.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;

        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("devices.toml");
        let tmp_path = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));

        let write_result: Result<(), ConfigError> = (|| {
            let mut tmp_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)?;
            tmp_file.write_all(text.as_bytes())?;
            tmp_file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }

        if let Err(err) = std::fs::rename(&tmp_path, path) {
            // The temp file already holds a full copy of the fresh token; a
            // failed rename must not leave it sitting in the config
            // directory. If cleanup itself fails, that's swallowed — the
            // caller needs to see why the rename failed, not why the
            // cleanup did.
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err.into());
        }

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

/// Turn a `toml` parse failure into a [`ConfigError::Parse`] that carries
/// only `toml`'s own structured message plus a computed line/column --
/// never `toml::de::Error`'s `Display`, which quotes the offending source
/// line (see the doc comment on [`ConfigError::Parse`]).
fn parse_error(err: &toml::de::Error, source: &str) -> ConfigError {
    let message = match err.span() {
        Some(span) => {
            let (line, column) = line_col(source, span.start);
            format!("{} (line {line}, column {column})", err.message())
        }
        None => err.message().to_string(),
    };
    ConfigError::Parse(message)
}

/// 1-based line and column of the byte offset `at` within `source`.
///
/// `at` comes from another crate's `toml::de::Error::span()`; walks
/// `char_indices` and breaks at the offset instead of slicing `source` at
/// `at` directly, so a byte offset that does not land on a char boundary
/// can never panic here (see the doc comment on [`ConfigError::Parse`]: a
/// panic on this path would print up to 256 bytes of surrounding source --
/// possibly a token -- to stderr as part of Rust's slice-fail message).
fn line_col(source: &str, at: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for (i, ch) in source.char_indices() {
        if i >= at {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
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
    fn refuses_two_devices_with_the_same_name() {
        // Otherwise `resolve("kettle")` would silently return whichever
        // device happens to come first in the Vec.
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"kettle\"\naliases = []\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"a0a1a2a3a4a5a6a7a8a9aaabacadaeaf\"\n"
        );
        assert!(matches!(parse(&toml), Err(ConfigError::DuplicateDevice { .. })));
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
    fn a_malformed_token_line_never_echoes_into_the_parse_error() {
        // An unterminated string is a TOML *syntax* error, not one this
        // crate's own validation catches -- exactly the kind of failure
        // where `toml`'s own `Display` would quote the source line
        // containing the token.
        let token = "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf";
        let malformed = KETTLE.replace(&format!("token = \"{token}\""), &format!("token = \"{token}"));

        let dir = std::env::temp_dir().join(format!("d3home-test-parse-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, &malformed).unwrap();

        let err = Config::load(&path).unwrap_err();
        let rendered = err.to_string();

        assert!(
            !rendered.contains(token),
            "the malformed token must never be echoed into an error message: {rendered}"
        );
        assert!(matches!(err, ConfigError::Parse(_)));

        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn fixes_permissions_on_a_pre_existing_config_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("d3home-test-overwrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");

        std::fs::write(&path, "stale = true\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        parse(KETTLE).unwrap().save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "overwriting an existing file must not keep its looser permissions"
        );

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("kettle"), "the new config must actually be written");
        assert!(!contents.contains("stale"), "the old contents must be replaced");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaves_the_existing_file_untouched_when_the_write_cannot_be_atomic() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("d3home-test-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, "original").unwrap();

        // Deny writes to the directory itself: the file can still be opened
        // and truncated in place, but a fresh temp file cannot be created
        // alongside it. A save that writes-then-renames must fail outright
        // here rather than falling back to an in-place truncate that would
        // briefly expose the new token under the old, looser permissions.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = parse(KETTLE).unwrap().save(&path);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "save must fail rather than write in place");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "original",
            "a failed save must not have touched the existing file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_the_temp_file_when_the_rename_itself_fails() {
        let dir = std::env::temp_dir().join(format!("d3home-test-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        // A regular file cannot be renamed onto an existing directory
        // (POSIX rename fails with EISDIR), so putting a directory at the
        // target path forces `rename` itself to fail *after* the temp file
        // has already been created, written, and synced -- unlike
        // `leaves_the_existing_file_untouched_when_the_write_cannot_be_atomic`,
        // which denies directory-write permission and so fails earlier, at
        // temp-file creation, never reaching rename at all.
        std::fs::create_dir(&path).unwrap();

        let result = parse(KETTLE).unwrap().save(&path);

        assert!(result.is_err(), "save must fail when the rename fails");

        let leftover_names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != path.file_name().unwrap())
            .collect();
        assert!(
            leftover_names.is_empty(),
            "the temp file holding the fresh token must not be left behind: {leftover_names:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn redacts_the_token_when_debug_formatted() {
        let config = parse(KETTLE).unwrap();
        let device = config.resolve("kettle").unwrap();

        let debug = format!("{device:?}");

        assert!(
            !debug.contains(&device.token),
            "Debug output must never contain the raw token: {debug}"
        );
    }
}
