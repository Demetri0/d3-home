//! The device registry: user-defined devices and their aliases, loaded from
//! and saved to a TOML file. See `default_path` for where that file lives.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Built-in subcommands that a device alias must never shadow.
pub const RESERVED: &[&str] = &[
    "add",
    "completions",
    "__complete",
    "daemon",
    "discover",
    "devices",
    "alias",
    "help",
];

/// Hex-encode `bytes` in lowercase -- the same representation the config
/// file's `token` and `public_key` fields, and mDNS's `public` TXT record,
/// already use.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a hex string into exactly `N` raw bytes, or `None` if it is the
/// wrong length or contains anything outside `0-9a-fA-F`.
///
/// Works byte-by-byte over `hex.as_bytes()` and never slices the `&str`
/// itself. A `&str` slice like `&hex[i..i + 2]` panics if `i`/`i + 2` don't
/// land on a UTF-8 character boundary -- and a string can be exactly the
/// right *byte* length (`hex.len() == N * 2`) while still containing a
/// multi-byte character (a homoglyph pasted from a chat client, say) that
/// puts some even offset mid-character. That used to be reachable through a
/// hand-edited `token` or cached `public_key`: the length guard passed, the
/// slice panicked, and Rust's slice-boundary panic message quotes the
/// offending string -- for `token`, printing the secret to stderr on the
/// way to an exit code (101) outside this program's documented contract.
/// Indexing raw bytes has no such restriction, so this can't panic on any
/// input, and any non-hex byte at any position is reported as `None` rather
/// than decoded.
pub(crate) fn hex_decode<const N: usize>(hex: &str) -> Option<[u8; N]> {
    let bytes = hex.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Normalise a MAC address the way `d3home` compares it everywhere else:
/// lowercase, with `:` and `-` separators stripped. Every comparison
/// against a MAC (`discovery::Discovery::find`, `commands::registry`'s
/// `cache_discovered`) is a byte-for-byte `==` against what mDNS advertises
/// -- which is always lowercase with no separators, since it comes straight
/// from the device's `_syncleo._udp.local.` instance name -- so a
/// hand-typed or pasted MAC that doesn't already look like that (uppercase,
/// as most routers' DHCP tables render one; colon- or hyphen-separated, as
/// every common tool prints one) silently never matches, and every command
/// for that device fails with "not found" -- a message that reads like the
/// hardware's fault, not a formatting mismatch in the config. Applied once,
/// at load, rather than at every comparison site, so nothing downstream
/// has to remember to normalise before comparing.
pub(crate) fn normalize_mac(mac: &str) -> String {
    mac.chars()
        .filter(|c| *c != ':' && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub devices: Vec<Device>,
    /// Skipped on save while it holds nothing but defaults, so registering a
    /// device does not silently write a settings section into the config of
    /// somebody who never asked for a daemon.
    #[serde(default, skip_serializing_if = "DaemonConfig::is_default")]
    pub daemon: DaemonConfig,
}

/// How the daemon should behave. Every part is optional: a config with no
/// `[daemon]` section is the config everybody already has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Which devices to watch. `None` means all of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<String>>,
    #[serde(default)]
    pub notify: NotifyConfig,
}

impl DaemonConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyConfig {
    /// Which events deserve a notification.
    #[serde(default = "default_events")]
    pub on: Vec<String>,
    /// A command to run instead of the built-in notifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The icon every notification carries, unless a device names its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            on: default_events(),
            command: None,
            icon: None,
        }
    }
}

fn default_events() -> Vec<String> {
    crate::commands::daemon::DEFAULT_EVENTS
        .iter()
        .map(|e| e.to_string())
        .collect()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub driver: String,
    pub model: Option<String>,
    /// Who made it, when it is known -- taken from the share link, since the
    /// protocol itself never says. Skipped on save when absent so a config
    /// written before this existed round-trips unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// The icon its notifications carry: a name from the icon theme, or a
    /// path. Unset means the one set for the daemon, else d3home's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
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
    /// The interface a link-local IPv6 `address` was seen on, e.g.
    /// `"enp8s0"`. Stored as a name rather than a kernel interface index
    /// because indices are reassigned across a reboot or a replugged NIC;
    /// the name is resolved to whatever index the OS currently has for it
    /// right before connecting. Always `None` for an address that doesn't
    /// need a scope id (any IPv4 address, or a globally routable IPv6
    /// one); required -- and checked at connect time, not here -- for a
    /// link-local IPv6 address.
    #[serde(default)]
    pub interface: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no device registry at {path} yet, so no devices are configured")]
    NoRegistry { path: String },
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

    #[error(
        "alias {alias:?} on device '{device}' is invalid: an alias must be non-empty and must not \
         start with '-' -- one that does can never actually be typed, since the CLI parses anything \
         starting with '-' as a flag before alias resolution ever runs"
    )]
    InvalidAlias { alias: String, device: String },

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

    #[error("'{name}' is not an event d3home knows about; try one of: {known}")]
    UnknownEvent { name: String, known: String },

    #[error("the daemon is told to watch '{name}', which is not a configured device")]
    UnknownDaemonDevice { name: String },

    #[error("the daemon is told to watch nothing at all; remove `devices` to watch everything")]
    NothingToWatch,

    #[error("no device matches '{name}'")]
    UnknownDevice { name: String },
}

impl Config {
    /// Load and validate the registry from `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            // A missing registry is the normal state before any device has
            // been added, not an I/O fault worth quoting errno for.
            if e.kind() == std::io::ErrorKind::NotFound {
                ConfigError::NoRegistry {
                    path: path.display().to_string(),
                }
            } else {
                ConfigError::Io(e)
            }
        })?;
        warn_if_permissions_are_too_loose(path);
        let mut config: Config = toml::from_str(&text).map_err(|e| parse_error(&e, &text))?;
        for device in &mut config.devices {
            device.mac = normalize_mac(&device.mac);
        }
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
            if RESERVED.contains(&device.name.as_str()) {
                // A device named e.g. "help" would load fine and then be
                // permanently unreachable, since `d3home help ...` always
                // routes to the built-in, never to device resolution.
                return Err(ConfigError::ReservedAlias {
                    alias: device.name.clone(),
                    device: device.name.clone(),
                });
            }
            for alias in &device.aliases {
                // Finding 12: an empty alias, or one starting with '-',
                // parses fine here and gets written to the config, but can
                // then never actually be typed -- `d3home --json status`
                // has `--json` consumed as the global flag long before
                // alias resolution runs, and an empty positional word
                // never reaches this device at all. Caught here rather
                // than left to be silently permanent.
                if alias.is_empty() || alias.starts_with('-') {
                    return Err(ConfigError::InvalidAlias {
                        alias: alias.clone(),
                        device: device.name.clone(),
                    });
                }
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
                if alias.as_str() != device.name.as_str() && device_names.contains(alias.as_str()) {
                    return Err(ConfigError::AliasShadowsDevice {
                        alias: alias.clone(),
                        device: device.name.clone(),
                    });
                }
                owners.insert(alias.as_str(), device.name.as_str());
            }
        }

        for name in &self.daemon.notify.on {
            // A typo here would otherwise mean waiting for a notification
            // that was never going to come, with nothing to diagnose.
            if !crate::commands::daemon::EVENTS.contains(&name.as_str()) {
                return Err(ConfigError::UnknownEvent {
                    name: name.clone(),
                    known: crate::commands::daemon::EVENTS.join(", "),
                });
            }
        }

        if let Some(devices) = &self.daemon.devices {
            if devices.is_empty() {
                return Err(ConfigError::NothingToWatch);
            }
            for name in devices {
                if self.resolve(name).is_none() {
                    return Err(ConfigError::UnknownDaemonDevice { name: name.clone() });
                }
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
        // Only tighten a directory we are creating ourselves. Chmod-ing one
        // the user already had would be an unpleasant surprise; leaving one
        // we just made world-traversable, with a token inside it, would be
        // worse.
        let existed = parent.exists();
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        // Windows has no mode bits to set: a file there is protected by the
        // access control list it inherits from the directory it is in, and
        // tightening that needs an API this project does not link. The token
        // is therefore as private as the user's profile directory, which is
        // the platform's own answer rather than ours.
        #[cfg(not(unix))]
        let _ = existed;

        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("devices.toml");
        let (tmp_path, mut tmp_file) = create_temp_file(parent, file_name)?;

        let write_result: Result<(), ConfigError> = (|| {
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
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                home.join(".config")
            });
        config_home.join("d3home").join("devices.toml")
    }
}

impl Device {
    /// Decode the hex-encoded token into the 16 raw bytes the protocol uses.
    pub fn token_bytes(&self) -> Result<[u8; 16], ConfigError> {
        hex_decode::<16>(&self.token).ok_or_else(|| ConfigError::BadToken {
            device: self.name.clone(),
        })
    }
}

/// Create a fresh, exclusively-owned temp file next to `dir` for `save` to
/// write through, mode `0600` from the moment it is created.
///
/// Uses `create_new` rather than `create` + `truncate`: `create` happily
/// opens (and then writes the fresh token through) a symlink another local
/// user pre-planted at the temp path, pointing at a file of their choosing.
/// That is unreachable against the real `~/.config/d3home` directory (only
/// this user can write there), but under `--config /tmp/x.toml` on a
/// shared `/tmp`, another local user could pre-create
/// `.x.toml.tmp-<pid>` as a symlink to a file they can read -- `mode(0600)`
/// is ignored when the target already exists, since `mode()` only governs
/// permissions at creation. `create_new` fails outright if anything
/// (including a symlink) already exists at the chosen name, so a random
/// suffix plus a bounded retry replaces the old pid-based name, which an
/// attacker could predict and pre-plant before the process even started.
fn create_temp_file(dir: &Path, file_name: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    const ATTEMPTS: u32 = 32;
    for _ in 0..ATTEMPTS {
        let suffix: u64 = rand::random();
        let candidate = dir.join(format!(".{file_name}.tmp-{suffix:016x}"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Owner-only from the moment it exists, so a token is never briefly
        // world-readable. See the note in `save` about Windows, which has no
        // such bit to set.
        #[cfg(unix)]
        options.mode(0o600);

        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "could not create a unique temp file in {} after {ATTEMPTS} attempts",
        dir.display()
    )))
}

/// Print a warning to stderr if `path`'s permission bits grant read or
/// write access to anyone but its owner. Never fails or blocks `load`: a
/// permission bit that can't even be checked (the file vanished between
/// `read_to_string` and here, an exotic filesystem) is not a reason to
/// refuse an otherwise-working config, only to say nothing.
///
/// `save` is meticulous about 0600 from the moment a file is created (see
/// its own doc comment); `load` used to check nothing at all. A config
/// restored from a backup, copied with plain `cp` (which does not preserve
/// mode), or written by hand under a permissive umask could sit at, say,
/// 0644 -- readable by every other local user -- holding a device token,
/// and every command would read it happily forever without ever saying so.
/// This does not refuse to load such a file: a wrong permission bit is a
/// reason to fix the file, not to break every command against an
/// otherwise-working config.
#[cfg(unix)]
fn warn_if_permissions_are_too_loose(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if let Some(warning) = loose_permission_warning(path, metadata.permissions().mode()) {
        crate::output::print_warning(&warning);
    }
}

/// Windows reports no owner/group/other bits to be too loose about, so
/// there is nothing here to warn on.
#[cfg(not(unix))]
fn warn_if_permissions_are_too_loose(_path: &Path) {}

/// The warning `warn_if_permissions_are_too_loose` prints, or `None` if
/// `mode`'s owner-only bits (`0600`) are already as tight as `save`
/// produces. Kept pure and separate from the actual printing so it can be
/// tested without capturing this process's own stderr.
fn loose_permission_warning(path: &Path, mode: u32) -> Option<String> {
    if mode & 0o077 == 0 {
        return None;
    }
    // No "d3home: warning:" prefix here: the printer owns the prefix, so the
    // same sentence can also be wrapped as JSON without it leaking inside.
    Some(format!(
        "{} is readable or writable by more than its owner (mode {:03o}); it \
         holds a device token -- consider `chmod 600 {}`",
        path.display(),
        mode & 0o777,
        path.display(),
    ))
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
mac = "deadbeefdead"
token = "deadbeefdeadbeefdeadbeefdeadbeef"
"#;

    #[test]
    fn a_device_registered_before_vendors_existed_still_loads() {
        // KETTLE has no `vendor` line, which is every config written so far.
        let config = parse(KETTLE).unwrap();
        assert!(config.resolve("kettle").unwrap().vendor.is_none());
    }

    #[test]
    fn a_device_with_no_vendor_does_not_grow_an_empty_one_when_saved() {
        let config = parse(KETTLE).unwrap();
        let written = toml::to_string_pretty(&config).unwrap();
        assert!(
            !written.contains("vendor"),
            "an absent vendor leaked into the file: {written}"
        );
    }

    #[test]
    fn a_vendor_survives_a_round_trip() {
        let toml_text = format!("{KETTLE}vendor = \"polaris\"\n");
        let config = parse(&toml_text).unwrap();
        assert_eq!(
            config.resolve("kettle").unwrap().vendor.as_deref(),
            Some("polaris")
        );
        let written = toml::to_string_pretty(&config).unwrap();
        assert!(
            written.contains("polaris"),
            "the vendor was dropped: {written}"
        );
    }

    #[test]
    fn a_config_without_a_daemon_section_still_has_defaults() {
        // Every config in existence predates this feature.
        let config = parse(KETTLE).unwrap();
        assert_eq!(
            config.daemon.notify.on,
            vec!["boiled".to_string(), "error".to_string()]
        );
        assert!(
            config.daemon.devices.is_none(),
            "none means watch everything"
        );
        assert!(config.daemon.notify.command.is_none());
    }

    #[test]
    fn the_daemon_section_is_read_when_present() {
        let toml = format!(
            "{KETTLE}\n[daemon]\ndevices = [\"kettle\"]\n\n\
             [daemon.notify]\non = [\"boiled\", \"started\"]\ncommand = \"ntfy publish x\"\n"
        );
        let config = parse(&toml).unwrap();
        assert_eq!(
            config.daemon.devices.as_deref(),
            Some(&["kettle".to_string()][..])
        );
        assert_eq!(
            config.daemon.notify.on,
            vec!["boiled".to_string(), "started".to_string()]
        );
        assert_eq!(
            config.daemon.notify.command.as_deref(),
            Some("ntfy publish x")
        );
    }

    #[test]
    fn an_event_name_that_does_not_exist_is_refused_at_load() {
        // Silently ignoring it would mean waiting for a notification that
        // was never going to come, with nothing to diagnose.
        let toml = format!("{KETTLE}\n[daemon.notify]\non = [\"boilded\"]\n");
        let err = parse(&toml).unwrap_err().to_string();
        assert!(
            err.contains("boilded"),
            "the error must name the typo: {err}"
        );
    }

    #[test]
    fn a_daemon_device_that_is_not_configured_is_refused_at_load() {
        let toml = format!("{KETTLE}\n[daemon]\ndevices = [\"teapot\"]\n");
        let err = parse(&toml).unwrap_err().to_string();
        assert!(err.contains("teapot"), "the error must name it: {err}");
    }

    #[test]
    fn an_empty_watch_list_is_refused_rather_than_silently_idle() {
        let toml = format!("{KETTLE}\n[daemon]\ndevices = []\n");
        assert!(
            parse(&toml).is_err(),
            "a daemon with nothing to watch is a mistake"
        );
    }

    #[test]
    fn a_daemon_device_may_be_named_by_its_alias() {
        // `resolve` accepts an alias everywhere else; the daemon's list must
        // not be the one place where the alias is rejected.
        let toml = format!("{KETTLE}\n[daemon]\ndevices = [\"k\"]\n");
        assert!(parse(&toml).is_ok());
    }

    #[test]
    fn saving_a_config_does_not_invent_a_daemon_section() {
        // Registering a device must not rewrite somebody's config with
        // settings for a daemon they have never run.
        let config = parse(KETTLE).unwrap();
        let written = toml::to_string_pretty(&config).unwrap();
        assert!(
            !written.contains("[daemon]"),
            "defaults leaked into the file: {written}"
        );
    }

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
        assert_eq!(
            cached.interface, None,
            "a config written before this field existed still loads"
        );
    }

    #[test]
    fn round_trips_the_cached_interface_name() {
        // The real kettle on the network this was fixed against advertises
        // only a link-local IPv6 address, which cannot be reached without
        // the interface it was seen on -- so this has to survive a
        // save/load cycle, not just a single parse.
        let dir = std::env::temp_dir().join(format!(
            "d3home-test-cached-interface-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");

        let mut config = parse(KETTLE).unwrap();
        config.devices[0].cached = Some(Cached {
            address: "fe80::dead:beef:dead:beef".parse().unwrap(),
            port: 8888,
            public_key: "ab".into(),
            interface: Some("enp8s0".into()),
        });
        config.save(&path).unwrap();

        let reloaded = Config::load(&path).unwrap();
        let cached = reloaded.resolve("kettle").unwrap().cached.as_ref().unwrap();
        assert_eq!(cached.interface.as_deref(), Some("enp8s0"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_an_alias_that_shadows_a_builtin_command() {
        // Silently losing `d3home discover` to an alias would be a nasty surprise.
        let toml = KETTLE.replace(r#"["k", "чайник"]"#, r#"["discover"]"#);
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::ReservedAlias { .. })
        ));
    }

    #[test]
    fn refuses_a_device_named_after_a_builtin_command() {
        // Without this check a device named "help" loads fine and is then
        // permanently unreachable: `d3home help status` always routes to
        // the built-in help text, never to device resolution.
        let toml = KETTLE.replace(r#"name = "kettle""#, r#"name = "help""#);
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::ReservedAlias { .. })
        ));
    }

    #[test]
    fn an_alias_with_a_colon_loads_and_resolves() {
        // Grouping by room -- `kitchen:kettle`, `bath:heater` -- is the
        // obvious use for a punctuation character here, and nothing in the
        // config layer treats a colon specially.
        let toml = KETTLE.replace(r#"["k", "чайник"]"#, r#"["kitchen:kettle"]"#);
        let config = parse(&toml).unwrap();
        assert_eq!(config.resolve("kitchen:kettle").unwrap().name, "kettle");
    }
    #[test]
    fn refuses_an_empty_alias() {
        let toml = KETTLE.replace(r#"["k", "чайник"]"#, r#"[""]"#);
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::InvalidAlias { .. })
        ));
    }

    #[test]
    fn refuses_an_alias_that_looks_like_a_flag() {
        // Finding 12: `d3home alias add --json kettle` parses fine today
        // (`--json` just looks like the alias word to `alias add`'s own
        // trailing_var_arg parsing), gets written to the config, and can
        // then never be used: `d3home --json status` has the parser consume
        // `--json` as the global flag long before alias resolution runs.
        let toml = KETTLE.replace(r#"["k", "чайник"]"#, r#"["--json"]"#);
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::InvalidAlias { .. })
        ));
    }

    #[test]
    fn refuses_the_same_alias_on_two_devices() {
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"other\"\naliases = [\"k\"]\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"deadbeefdeadbeefdeadbeefdeadbeef\"\n"
        );
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::DuplicateAlias { .. })
        ));
    }

    #[test]
    fn refuses_an_alias_that_shadows_another_device_name() {
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"other\"\naliases = [\"kettle\"]\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"deadbeefdeadbeefdeadbeefdeadbeef\"\n"
        );
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::AliasShadowsDevice { .. })
        ));
    }

    #[test]
    fn refuses_two_devices_with_the_same_name() {
        // Otherwise `resolve("kettle")` would silently return whichever
        // device happens to come first in the Vec.
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"kettle\"\naliases = []\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"deadbeefdeadbeefdeadbeefdeadbeef\"\n"
        );
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::DuplicateDevice { .. })
        ));
    }

    #[test]
    // The brief's assertion form (`matches!(.., Err(_))`) is kept verbatim;
    // clippy would rather see `.is_err()`.
    #[allow(clippy::redundant_pattern_matching)]
    fn parses_the_token_into_sixteen_bytes() {
        let config = parse(KETTLE).unwrap();
        assert_eq!(
            config.resolve("k").unwrap().token_bytes().unwrap(),
            [
                0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0xbe, 0xef, 0xde, 0xad,
                0xbe, 0xef,
            ]
        );

        let bad = KETTLE.replace("deadbeefdeadbeefdeadbeefdeadbeef", "nothex");
        assert!(matches!(
            parse(&bad).unwrap().resolve("k").unwrap().token_bytes(),
            Err(_)
        ));
    }

    #[test]
    fn a_multibyte_token_that_is_the_right_byte_length_is_a_bad_token_not_a_panic() {
        // Regression test for the token_bytes sibling of 29b4ed2's line_col
        // fix: ten 3-byte "€" characters plus two ASCII bytes is exactly 32
        // *bytes*, so the old `self.token.len() != 32` guard passed, but
        // `&self.token[0..2]` then sliced into the middle of the first "€"
        // and panicked -- quoting the token itself in the panic message.
        let token = "€€€€€€€€€€ab";
        assert_eq!(
            token.len(),
            32,
            "fixture must be exactly 32 bytes to reach the old guard"
        );
        assert_eq!(token.chars().count(), 12, "and clearly not 32 *characters*");

        let toml = KETTLE.replace("deadbeefdeadbeefdeadbeefdeadbeef", token);
        let device = parse(&toml).unwrap().devices.into_iter().next().unwrap();

        let err = device.token_bytes().unwrap_err();
        assert!(matches!(err, ConfigError::BadToken { .. }));
        assert!(
            !err.to_string().contains(token),
            "the malformed token must never be echoed into the error: {err}"
        );
    }

    #[test]
    fn a_malformed_token_line_never_echoes_into_the_parse_error() {
        // An unterminated string is a TOML *syntax* error, not one this
        // crate's own validation catches -- exactly the kind of failure
        // where `toml`'s own `Display` would quote the source line
        // containing the token.
        let token = "deadbeefdeadbeefdeadbeefdeadbeef";
        let malformed = KETTLE.replace(
            &format!("token = \"{token}\""),
            &format!("token = \"{token}"),
        );

        let dir =
            std::env::temp_dir().join(format!("d3home-test-parse-error-{}", std::process::id()));
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
    fn a_multibyte_line_above_a_malformed_token_never_panics_or_leaks() {
        // Regression test for 29b4ed2: `toml::de::Error::span()` returns a
        // byte offset into the source, and `line_col` used to slice
        // `source` at that offset directly. A multi-byte character earlier
        // in the file (like the "чайник" alias below) shifts every later
        // byte offset away from the character count a naive slice assumes,
        // which used to be able to panic mid-slice -- dumping up to 256
        // bytes of surrounding source, possibly the token itself, into the
        // panic message. `line_col` now walks `char_indices` instead, so
        // this must neither panic nor echo the token.
        let token = "deadbeefdeadbeefdeadbeefdeadbeef";
        let toml = format!(
            "[[devices]]\nname = \"kettle\"\naliases = [\"k\", \"чайник\"]\ndriver = \"syncleo\"\nmac = \"deadbeefdead\"\ntoken = \"{token}\n"
        );

        let dir =
            std::env::temp_dir().join(format!("d3home-test-multibyte-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, &toml).unwrap();

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
    #[cfg(unix)]
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
    #[cfg(unix)]
    fn fixes_permissions_on_a_pre_existing_config_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("d3home-test-overwrite-{}", std::process::id()));
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
        assert!(
            contents.contains("kettle"),
            "the new config must actually be written"
        );
        assert!(
            !contents.contains("stale"),
            "the old contents must be replaced"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
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
    fn a_mac_typed_in_uppercase_still_matches_what_discovery_would_find() {
        // Finding 7: mDNS's instance name (and therefore `Found::mac`) is
        // always lowercase, with no separators. A MAC pasted from a
        // router's DHCP table -- typically uppercase, colon-separated --
        // must still end up equal to that, or every lookup for the device
        // silently never matches.
        let dir = std::env::temp_dir().join(format!("d3home-test-mac-case-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(
            &path,
            KETTLE.replace(r#"mac = "deadbeefdead""#, r#"mac = "DE:AD:BE:EF:DE:AD""#),
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.resolve("kettle").unwrap().mac, "deadbeefdead");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_mac_lowercases_and_strips_common_separators() {
        assert_eq!(normalize_mac("deadbeefdead"), "deadbeefdead");
        assert_eq!(normalize_mac("DEADBEEFDEAD"), "deadbeefdead");
        assert_eq!(normalize_mac("DE:AD:BE:EF:DE:AD"), "deadbeefdead");
        assert_eq!(normalize_mac("de-ad-be-ef-de-ad"), "deadbeefdead");
    }

    #[test]
    fn two_saves_interleaved_around_one_shared_load_lose_the_earlier_one_cleanly() {
        // Finding 17: nothing coordinates two `d3home` processes saving the
        // config at once. Each `save` is atomic on its own (create_new
        // plus rename -- see its doc comment: a reader never sees a torn
        // file, and a token is never exposed under loose permissions
        // mid-write), but two writers that both loaded before either one's
        // write landed still clobber each other with no error and no
        // merge -- this is a known, accepted tradeoff (lost updates, never
        // corruption), not something this pins as a bug. What it does pin
        // is the *shape* of the loss: a clean "later rename wins" outcome,
        // never a torn or merged file.
        let dir = std::env::temp_dir().join(format!(
            "d3home-test-interleaved-save-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        parse(KETTLE).unwrap().save(&path).unwrap();

        // Both "processes" load the same starting file before either one
        // writes anything back.
        let mut first_writer = Config::load(&path).unwrap();
        let mut second_writer = Config::load(&path).unwrap();

        first_writer.devices[0].aliases.push("first".into());
        second_writer.devices[0].aliases.push("second".into());

        first_writer.save(&path).unwrap();
        second_writer.save(&path).unwrap();

        let final_config = Config::load(&path).unwrap();
        let aliases = &final_config.resolve("kettle").unwrap().aliases;
        assert_eq!(
            aliases,
            &vec!["k".to_string(), "чайник".to_string(), "second".to_string()],
            "the later save must win outright -- not merge with the earlier one, not corrupt"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_world_or_group_readable_config_gets_a_warning() {
        let path = Path::new("devices.toml");
        assert!(loose_permission_warning(path, 0o644).is_some());
        assert!(loose_permission_warning(path, 0o640).is_some());
        assert!(loose_permission_warning(path, 0o604).is_some());
        assert!(loose_permission_warning(path, 0o666).is_some());
    }

    #[test]
    fn an_owner_only_config_gets_no_warning() {
        assert!(loose_permission_warning(Path::new("devices.toml"), 0o600).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_loose_permission_config_still_loads_successfully() {
        // Finding 18: the warning must never turn into a hard failure --
        // an otherwise-working config with the wrong mode still has to
        // work for every command, the same way it always did.
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("d3home-test-loose-perms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(&path, KETTLE).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            Config::load(&path).is_ok(),
            "a loose permission must warn, not refuse to load"
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
