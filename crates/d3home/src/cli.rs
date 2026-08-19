//! Argument interpretation and the error/exit-code vocabulary shared by
//! every command.
//!
//! This module answers exactly one question -- "given the words after
//! `d3home`, is this a built-in command or a device command, and if it's
//! broken, which of the two kinds of broken is it?" -- and nothing about
//! *how* any command runs. That lives in `commands::registry` and
//! `commands::kettle`.
//!
//! Global flags (`--config`, `--json`, `--device`, `--help`) are stripped
//! out by [`split_globals`] first, from *any* position on the command line,
//! and [`parse`] then sees only the remaining words. They are handled here
//! rather than by `clap` because the first word is a device name drawn from
//! the user's config, which forces a dynamic subcommand -- and a dynamic
//! subcommand swallows everything after it verbatim, which is exactly how
//! a trailing `--json` came to be silently ignored.

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

/// The global options, which may appear anywhere on the command line --
/// before the device word, after the action, or after the action's own
/// arguments. They are all "how to run this", never "what to run".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Globals {
    pub config: Option<std::path::PathBuf>,
    pub json: bool,
    pub device: Option<String>,
    pub help: bool,
}

/// Pull the global options out of `argv`, wherever they appear, and return
/// them alongside the words that remain.
///
/// A bare `--` stops option processing, so a device or alias whose name
/// begins with a dash is still reachable. An unrecognised `--flag` is an
/// error rather than a word: silently treating `--jsno` as a device name
/// would send the user hunting through their config for a device they never
/// created.
pub fn split_globals(argv: &[String]) -> Result<(Globals, Vec<String>), UsageError> {
    let mut globals = Globals::default();
    let mut words = Vec::new();
    let mut iter = argv.iter().peekable();
    let mut literal = false;

    while let Some(arg) = iter.next() {
        if literal {
            words.push(arg.clone());
            continue;
        }

        // A value that needs its own argument: `--flag value` or `--flag=value`.
        let mut take_value = |name: &str| -> Result<String, UsageError> {
            match arg.split_once('=') {
                Some((_, value)) if !value.is_empty() => Ok(value.to_string()),
                Some((_, _)) => Err(UsageError(format!("{name} needs a value"))),
                None => iter
                    .next()
                    .cloned()
                    .ok_or_else(|| UsageError(format!("{name} needs a value"))),
            }
        };

        let name = arg.split('=').next().unwrap_or(arg);
        match name {
            "--" => literal = true,
            "--json" => globals.json = true,
            "--help" | "-h" => globals.help = true,
            "--config" => globals.config = Some(take_value("--config")?.into()),
            "--device" => globals.device = Some(take_value("--device")?),
            other if other.starts_with("--") => {
                return Err(UsageError(format!("unknown option '{other}'; try 'd3home help'")));
            }
            _ => words.push(arg.clone()),
        }
    }

    Ok((globals, words))
}

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
        // RESERVED is the single source of truth for built-in names, but
        // it's a runtime slice with no compiler-enforced link to the arms
        // above: nothing stops someone from adding a word to RESERVED
        // without adding a matching arm here. This panics rather than
        // silently treating an unrecognized reserved word as a device name,
        // so that gap fails loudly (in the test suite, at the latest)
        // instead of quietly shadowing a command that was meant to exist.
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
            // NoState joins them: the device answered the handshake but
            // told us nothing at all within the window, which -- absent a
            // real "query state" command in the protocol -- is the closest
            // thing to "did not respond" a status check can observe.
            syncleo::Error::Timeout | syncleo::Error::Silence | syncleo::Error::NoState => {
                AppError::Timeout(message)
            }
            syncleo::Error::HandshakeRejected => AppError::BadToken,
            // The device answered clearly and rejected the command outright
            // -- distinct from every Timeout/Silence/NoState case above,
            // where the most it says is silence.
            syncleo::Error::DeviceNak => AppError::Device(message),
            // A failure below the protocol layer -- a socket call failing
            // (ENETUNREACH, EHOSTUNREACH), a cached interface name that no
            // longer resolves to an index (ENODEV: the NIC was replugged,
            // renamed, or removed), the mDNS daemon failing to start -- is,
            // from the operator's chair, indistinguishable from the device
            // simply not answering: both say "the network's not reaching it
            // right now, maybe try again or run discover," neither says
            // "this is a bug in d3home." Grouping it with Timeout (rather
            // than Codec below) rather than `Internal` also means
            // `commands::kettle::is_connectivity_failure` retries it in
            // `watch`'s reconnect loop, and `commands::kettle::connect`
            // falls back from a cached endpoint to a fresh discovery scan
            // on it exactly as it does for a plain timeout -- see
            // `evaluate_cached_attempt`. Before this, a renamed/removed
            // network interface made every command exit 1 "internal error"
            // forever, with no fallback, even though the interface-*name*
            // cache design exists precisely to survive this.
            syncleo::Error::Io(_) => AppError::Timeout(message),
            // A malformed frame from the device never actually reaches
            // here: `Session::on_packet` swallows a `CodecError`
            // internally (garbage on the wire is silently dropped, not
            // surfaced as an `Err`). This stays `Internal` as the honest
            // "should not happen" bucket, distinct from the `Io` case
            // above -- if it is ever reachable, it means a corrupted
            // frame got past decryption, not a network hiccup, and
            // retrying the identical bytes would not help.
            syncleo::Error::Codec(_) => AppError::Internal(message),
            syncleo::Error::UnsupportedProtocol { .. }
            | syncleo::Error::NoUsableAddress
            | syncleo::Error::BadServiceRecord(_) => AppError::NotFound(message),
            // Reachable when a hand-edited (or pre-upgrade) config's
            // `[devices.cached]` has a link-local address but no
            // `interface`; commands::kettle::cached_socket_addr normally
            // intercepts this first with a message that names the device,
            // so this arm is the fallback for anywhere else the error
            // could surface.
            syncleo::Error::LinkLocalAddressWithoutScope => AppError::Usage(message),
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


    #[test]
    fn a_global_flag_is_honoured_wherever_it_appears() {
        // The motivating bug: `--json` after the action was silently dropped,
        // so the user got human output and no hint that the flag was ignored.
        for argv in [
            words(&["--json", "kettle", "status"]),
            words(&["kettle", "--json", "status"]),
            words(&["kettle", "status", "--json"]),
            words(&["kettle", "start", "80", "--json"]),
        ] {
            let (globals, rest) = split_globals(&argv).expect("parses");
            assert!(globals.json, "--json lost in {argv:?}");
            assert!(!rest.contains(&"--json".to_string()), "--json leaked into words");
        }
    }

    #[test]
    fn a_flag_taking_a_value_accepts_both_spellings_anywhere() {
        let (a, rest_a) = split_globals(&words(&["kettle", "status", "--config", "/tmp/x.toml"])).unwrap();
        let (b, rest_b) = split_globals(&words(&["--config=/tmp/x.toml", "kettle", "status"])).unwrap();

        assert_eq!(a.config.as_deref(), Some(std::path::Path::new("/tmp/x.toml")));
        assert_eq!(a.config, b.config);
        assert_eq!(rest_a, words(&["kettle", "status"]));
        assert_eq!(rest_b, rest_a);
    }

    #[test]
    fn a_value_flag_with_nothing_after_it_is_an_error() {
        assert!(split_globals(&words(&["kettle", "status", "--config"])).is_err());
        assert!(split_globals(&words(&["--config=", "kettle"])).is_err());
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_taken_for_a_device() {
        // Treating `--jsno` as a device name would send the user hunting
        // through their config for something they never created.
        let err = split_globals(&words(&["kettle", "status", "--jsno"])).unwrap_err();
        assert!(err.to_string().contains("--jsno"), "got: {err}");
    }

    #[test]
    fn a_double_dash_lets_a_literal_flag_through_as_a_word() {
        let (globals, rest) = split_globals(&words(&["alias", "add", "--", "--json", "kettle"])).unwrap();
        assert!(!globals.json, "after -- it is a word, not a flag");
        assert_eq!(rest, words(&["alias", "add", "--json", "kettle"]));
    }

    #[test]
    fn help_is_recognised_from_any_position() {
        for argv in [words(&["--help"]), words(&["kettle", "-h"]), words(&["kettle", "status", "--help"])] {
            assert!(split_globals(&argv).unwrap().0.help, "help lost in {argv:?}");
        }
    }

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
        assert_eq!(
            AppError::from(syncleo::Error::LinkLocalAddressWithoutScope).exit_code(),
            ExitCode::Usage
        );
    }

    #[test]
    fn an_io_failure_is_a_timeout_not_an_internal_error() {
        // Finding 3: a stale cached interface (renamed/replugged NIC) or an
        // unreachable network surfaces as `syncleo::Error::Io`. Before this,
        // that mapped to `Internal` -- exit 1, "a bug in this program" --
        // with no way for `watch`'s reconnect loop to tell it apart from a
        // real bug. It belongs with `Timeout`: both mean "couldn't reach
        // the device right now," and both are worth retrying.
        let err = AppError::from(syncleo::Error::Io(std::io::Error::other("no such device")));
        assert_eq!(err.exit_code(), ExitCode::Timeout);
    }

    #[test]
    fn a_codec_failure_stays_an_internal_error() {
        // Distinct from the Io case above: a `CodecError` reaching this far
        // would mean corrupted bytes got past decryption, not a network
        // hiccup, so retrying the identical bytes would not help.
        let err = AppError::from(syncleo::Error::Codec(syncleo::error::CodecError::EmptyBody));
        assert_eq!(err.exit_code(), ExitCode::Internal);
    }
}
