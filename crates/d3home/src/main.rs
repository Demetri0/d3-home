//! The `d3home` entry point: turn `argv` into global options plus a word
//! list, hand the word list to `cli::parse`, and route whatever comes back
//! to the module that knows how to run it. Nothing about *how* a command
//! behaves belongs here -- see `commands::registry` and `commands::kettle`.

mod cli;
mod commands;
mod config;
mod output;

use std::path::{Path, PathBuf};

use clap::Parser;

use cli::{AppError, Builtin, ExitCode, Parsed};
use commands::{kettle, registry};
use config::{Config, ConfigError};

/// Global options, plus every word that follows them. Whether the first of
/// those words is a built-in command or a device name is decided later, by
/// `cli::parse` -- see that module for why.
#[derive(Parser)]
#[command(name = "d3home", about = "Control smart home devices over the local network")]
struct Args {
    /// Path to the device registry. Defaults to `Config::default_path()`.
    /// Tests always pass this explicitly, so they never touch the real one.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Emit machine-readable JSON instead of the human-readable summary.
    #[arg(long)]
    json: bool,

    /// Use this device for the action that follows, instead of taking the
    /// device from the first positional word.
    #[arg(long)]
    device: Option<String>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    rest: Vec<String>,
}

fn main() {
    let args = Args::parse();
    let config_path = args.config.clone().unwrap_or_else(Config::default_path);

    let words: Vec<String> = match args.device {
        Some(device) => std::iter::once(device).chain(args.rest).collect(),
        None => args.rest,
    };

    let result = cli::parse(&words)
        .map_err(AppError::from)
        .and_then(|parsed| dispatch(parsed, &config_path, args.json));

    match result {
        Ok(()) => std::process::exit(ExitCode::Ok as i32),
        Err(err) => {
            eprintln!("d3home: {err}");
            std::process::exit(err.exit_code() as i32);
        }
    }
}

/// Route a parsed command to the module that knows how to run it. Builtins
/// that need the config load it themselves here, since they're one-shot;
/// device commands load it too, to resolve the name, before handing off to
/// the driver.
fn dispatch(parsed: Parsed, config_path: &Path, json: bool) -> Result<(), AppError> {
    match parsed {
        Parsed::Builtin(Builtin::Discover) => registry::discover(config_path, json),
        Parsed::Builtin(Builtin::Devices) => {
            let config = Config::load(config_path)?;
            registry::devices(&config, json);
            Ok(())
        }
        Parsed::Builtin(Builtin::AliasAdd { alias, device }) => {
            registry::alias_add(config_path, &alias, &device)
        }
        Parsed::Builtin(Builtin::AliasRm { alias }) => registry::alias_rm(config_path, &alias),
        Parsed::Builtin(Builtin::Help) => {
            registry::help();
            Ok(())
        }
        Parsed::Device { device, action } => {
            let config = Config::load(config_path)?;
            let found = config
                .resolve(&device)
                .cloned()
                .ok_or_else(|| ConfigError::UnknownDevice { name: device.clone() })?;
            run_device(&found, &action, json, config_path)
        }
    }
}

/// Dispatch by driver. `"syncleo"` is the only driver this task implements
/// (the kettle); a second driver would get its own arm here and its own
/// sibling of `commands::kettle`, without anything above this function
/// changing.
fn run_device(device: &config::Device, action: &[String], json: bool, config_path: &Path) -> Result<(), AppError> {
    match device.driver.as_str() {
        "syncleo" => kettle::run(device, action, json, config_path),
        other => Err(AppError::Usage(format!("device '{}' has unknown driver '{other}'", device.name))),
    }
}
