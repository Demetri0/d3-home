//! The `d3home` entry point: split `argv` into global options plus a word
//! list, hand the word list to `cli::parse`, and route whatever comes back
//! to the module that knows how to run it. Nothing about *how* a command
//! behaves belongs here -- see `commands::registry` and `commands::kettle`.

mod bar;
mod cli;
mod commands;
mod config;
mod keys;
mod output;
mod progress;
mod style;

use std::path::Path;

use cli::{AppError, Builtin, ExitCode, Parsed};
use commands::{add, complete, kettle, registry};
use config::{Config, ConfigError};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let result = run(&argv);

    match result {
        Ok(()) => std::process::exit(ExitCode::Ok as i32),
        Err(err) => {
            eprintln!("d3home: {err}");
            std::process::exit(err.exit_code() as i32);
        }
    }
}

/// Everything `main` does apart from turning the outcome into an exit code,
/// split out so it is reachable from a test.
fn run(argv: &[String]) -> Result<(), AppError> {
    let (globals, words) = cli::split_globals(argv)?;

    // `--help` anywhere wins over whatever else was typed: someone who asks
    // for help has already stopped wanting the command to run.
    if globals.help || words.first().is_some_and(|w| w == "help") {
        registry::help();
        return Ok(());
    }

    let config_path = globals.config.clone().unwrap_or_else(Config::default_path);
    let words: Vec<String> = match globals.device {
        Some(device) => std::iter::once(device).chain(words).collect(),
        None => words,
    };

    let parsed = cli::parse(&words)?;
    dispatch(parsed, &config_path, globals.json)
}

/// Route a parsed command to the module that knows how to run it. Builtins
/// that need the config load it themselves here, since they're one-shot;
/// device commands load it too, to resolve the name, before handing off to
/// the driver.
fn dispatch(parsed: Parsed, config_path: &Path, json: bool) -> Result<(), AppError> {
    match parsed {
        Parsed::Builtin(Builtin::Add { args }) => {
            let mut request = add::parse_args(&args)?;
            let prompt = add::can_prompt();
            // Look around the network before asking anyone to type a MAC by
            // hand -- the device they want is usually sitting right there.
            if prompt && request.mac.is_none() {
                request.mac = add::pick_discovered();
            }
            let device = add::resolve(request, prompt)?;
            add::write(config_path, device)
        }
        Parsed::Builtin(Builtin::Completions { shell }) => complete::script(&shell),
        Parsed::Builtin(Builtin::Complete { words }) => {
            complete::complete(&words, config_path);
            Ok(())
        }
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
