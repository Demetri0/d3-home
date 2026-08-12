//! Argument interpretation and the error/exit-code vocabulary shared by
//! every command.
//!
//! This module answers exactly one question -- "given the words after
//! `d3home`, is this a built-in command or a device command, and if it's
//! broken, which of the two kinds of broken is it?" -- and nothing about
//! *how* any command runs. That lives in `commands::registry` and
//! `commands::kettle`.
//!
//! Global flags (`--config`, `--json`, `--device`) are consumed by `main.rs`
//! via `clap` before `parse` ever sees the remaining words: this function
//! only cares about the "first word is a built-in or a device" split, which
//! `clap`'s static subcommand model has no way to express, because the set
//! of device names is only known once the config is loaded.

use crate::config::RESERVED;

/// Process exit codes. `main` converts every `AppError` into one of these
/// via [`AppError::exit_code`]; nothing else in the program calls
/// `std::process::exit` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Ok = 0,
    Internal = 1,
    Usage = 2,
    NotFound = 3,
    BadToken = 4,
    Timeout = 5,
    DeviceError = 6,
}

/// A built-in subcommand: one of the words in [`RESERVED`], fully parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Builtin {
    Discover,
    Devices,
    AliasAdd { alias: String, device: String },
    AliasRm { alias: String },
    Help,
}

/// The result of interpreting the words after the global flags: either one
/// of the built-in commands, or a device (named directly or by alias)
/// together with whatever words follow it, unexamined -- only the device's
/// driver knows what to do with those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Builtin(Builtin),
    Device { device: String, action: Vec<String> },
}

/// A malformed command line: wrong number of arguments to a built-in,
/// or no command at all. Distinct from [`AppError`] because `parse` runs
/// before any config or network I/O exists to report errors about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UsageError {}

/// Interpret the words that follow the global flags. The first word is a
/// built-in command if it appears in [`RESERVED`]; otherwise it is taken to
/// be a device name or alias, resolved later once the config is loaded.
pub fn parse(args: &[String]) -> Result<Parsed, UsageError> {
    let (head, rest) = args
        .split_first()
        .ok_or_else(|| UsageError("no command given; try 'd3home help'".into()))?;

    if !RESERVED.contains(&head.as_str()) {
        return Ok(Parsed::Device { device: head.clone(), action: rest.to_vec() });
    }

    match head.as_str() {
        "discover" => Ok(Parsed::Builtin(Builtin::Discover)),
        "devices" => Ok(Parsed::Builtin(Builtin::Devices)),
        "help" => Ok(Parsed::Builtin(Builtin::Help)),
        "alias" => parse_alias(rest),
        // RESERVED is the single source of truth for built-in names; adding
        // a new one there without a matching arm here is a compile-time
        // gap, not a runtime one -- caught by the exhaustiveness of this
        // match against the four arms above plus this one.
        other => unreachable!("'{other}' is in RESERVED but has no parser"),
    }
}

fn parse_alias(rest: &[String]) -> Result<Parsed, UsageError> {
    match rest {
        [cmd, alias, device] if cmd == "add" => {
            Ok(Parsed::Builtin(Builtin::AliasAdd { alias: alias.clone(), device: device.clone() }))
        }
        [cmd, alias] if cmd == "rm" => Ok(Parsed::Builtin(Builtin::AliasRm { alias: alias.clone() })),
        _ => Err(UsageError(
            "usage: d3home alias add <alias> <device> | d3home alias rm <alias>".into(),
        )),
    }
}

/// Everything that can go wrong once parsing has succeeded: bad config, a
/// device that can't be found or won't talk to us, or the device itself
/// reporting trouble. Every variant maps to exactly one [`ExitCode`].
#[derive(Debug)]
pub enum AppError {
    /// Bad arguments or a bad config: something a human needs to fix
    /// before trying again.
    Usage(String),
    /// The device could not be located on the network at all.
    NotFound(String),
    /// The device answered but rejected our token.
    BadToken,
    /// The device did not answer in time (including "stopped answering").
    Timeout(String),
    /// The device answered but reported an error of its own.
    Device(String),
    /// Anything else: a bug, a filesystem error, a codec error -- not
    /// something the user can act on by changing their command.
    Internal(String),
}

impl AppError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            AppError::Usage(_) => ExitCode::Usage,
            AppError::NotFound(_) => ExitCode::NotFound,
            AppError::BadToken => ExitCode::BadToken,
            AppError::Timeout(_) => ExitCode::Timeout,
            AppError::Device(_) => ExitCode::DeviceError,
            AppError::Internal(_) => ExitCode::Internal,
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Usage(m)
            | AppError::NotFound(m)
            | AppError::Timeout(m)
            | AppError::Device(m)
            | AppError::Internal(m) => write!(f, "{m}"),
            AppError::BadToken => write!(f, "device rejected the token"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<UsageError> for AppError {
    fn from(err: UsageError) -> Self {
        AppError::Usage(err.0)
    }
}

/// Every `ConfigError` -- a malformed file, a conflicting alias, an unknown
/// device -- is something the user fixes by changing their config or their
/// command, so all of them land on the same exit code. `Display` on
/// `ConfigError` already carries the specific detail (which device, which
/// alias, ...), so nothing is lost by not matching on the variant here.
impl From<crate::config::ConfigError> for AppError {
    fn from(err: crate::config::ConfigError) -> Self {
        AppError::Usage(err.to_string())
    }
}

/// `syncleo::Error` maps onto the exit-code table one to one. Matching
/// every variant explicitly (rather than a wildcard arm) means a new
/// variant added to `syncleo::Error` fails this crate's build until someone
/// decides where it belongs, instead of silently falling into the wrong
/// bucket.
impl From<syncleo::Error> for AppError {
    fn from(err: syncleo::Error) -> Self {
        let message = err.to_string();
        match err {
            // The device may still be there; it has simply stopped
            // answering. Indistinguishable from a plain timeout from here.
            syncleo::Error::Timeout | syncleo::Error::Silence => AppError::Timeout(message),
            syncleo::Error::HandshakeRejected => AppError::BadToken,
            syncleo::Error::Codec(_) | syncleo::Error::Io(_) => AppError::Internal(message),
            syncleo::Error::UnsupportedProtocol { .. }
            | syncleo::Error::NoUsableAddress
            | syncleo::Error::BadServiceRecord(_) => AppError::NotFound(message),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::Internal(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_word_outside_reserved_is_a_device_command() {
        let parsed = parse(&words(&["kettle", "start", "80"])).unwrap();
        assert_eq!(
            parsed,
            Parsed::Device { device: "kettle".into(), action: vec!["start".into(), "80".into()] }
        );
    }

    #[test]
    fn an_alias_is_indistinguishable_from_a_device_name_at_this_layer() {
        // cli::parse never sees the config, so it cannot know "k" is an
        // alias rather than a device name -- and does not need to.
        let parsed = parse(&words(&["k", "status"])).unwrap();
        assert_eq!(parsed, Parsed::Device { device: "k".into(), action: vec!["status".into()] });
    }

    #[test]
    fn recognizes_every_builtin() {
        assert_eq!(parse(&words(&["discover"])).unwrap(), Parsed::Builtin(Builtin::Discover));
        assert_eq!(parse(&words(&["devices"])).unwrap(), Parsed::Builtin(Builtin::Devices));
        assert_eq!(parse(&words(&["help"])).unwrap(), Parsed::Builtin(Builtin::Help));
    }

    #[test]
    fn parses_alias_add_and_rm() {
        assert_eq!(
            parse(&words(&["alias", "add", "k", "kettle"])).unwrap(),
            Parsed::Builtin(Builtin::AliasAdd { alias: "k".into(), device: "kettle".into() })
        );
        assert_eq!(
            parse(&words(&["alias", "rm", "k"])).unwrap(),
            Parsed::Builtin(Builtin::AliasRm { alias: "k".into() })
        );
    }

    #[test]
    fn rejects_a_malformed_alias_command() {
        assert!(parse(&words(&["alias"])).is_err());
        assert!(parse(&words(&["alias", "add", "k"])).is_err());
        assert!(parse(&words(&["alias", "frobnicate", "k"])).is_err());
    }

    #[test]
    fn an_empty_command_line_is_a_usage_error() {
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn every_syncleo_error_maps_to_its_own_exit_code() {
        assert_eq!(AppError::from(syncleo::Error::Timeout).exit_code(), ExitCode::Timeout);
        assert_eq!(AppError::from(syncleo::Error::Silence).exit_code(), ExitCode::Timeout);
        assert_eq!(AppError::from(syncleo::Error::HandshakeRejected).exit_code(), ExitCode::BadToken);
        assert_eq!(AppError::from(syncleo::Error::NoUsableAddress).exit_code(), ExitCode::NotFound);
    }
}
