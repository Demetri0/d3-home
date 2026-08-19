//! Shell completion.
//!
//! The candidates cannot be baked into a static script, because the first
//! word of a command is a device name taken from the user's own config. So
//! the shell scripts are thin: they ask the binary what would fit here, and
//! the binary answers from the live registry.

use std::path::Path;

use crate::cli::AppError;
use crate::config::{Config, RESERVED};

/// Actions each driver understands. A second driver adds an arm here and
/// its completions follow, with no change to the shell scripts.
fn actions_for(driver: &str) -> &'static [&'static str] {
    match driver {
        "syncleo" => &[
            "status", "start", "on", "set", "off", "stop", "watch", "trace",
        ],
        _ => &[],
    }
}

const GLOBAL_FLAGS: &[&str] = &["--json", "--device", "--config", "--help"];
const SHELLS: &[&str] = &["bash", "zsh", "fish"];

/// Candidates that would fit where the cursor is.
///
/// `words` is everything typed after the program name, with the word being
/// completed last -- possibly empty, when the cursor sits after a space.
pub fn candidates(words: &[String], config_path: &Path) -> Vec<String> {
    let (current, prefix) = match words.split_last() {
        Some((last, rest)) => (last.as_str(), rest),
        None => ("", &[][..]),
    };

    // A registry that is missing or broken must not break the shell; the
    // built-ins are still worth offering.
    let config = Config::load(config_path).ok();

    let mut out: Vec<String> = if current.starts_with('-') {
        GLOBAL_FLAGS.iter().map(|s| s.to_string()).collect()
    } else if prefix.is_empty() {
        let mut names: Vec<String> = RESERVED
            .iter()
            .filter(|w| !w.starts_with('_'))
            .map(|s| s.to_string())
            .collect();
        names.push("completions".into());
        if let Some(config) = &config {
            for device in &config.devices {
                names.push(device.name.clone());
                names.extend(device.aliases.iter().cloned());
            }
        }
        names
    } else {
        match prefix[0].as_str() {
            "alias" if prefix.len() == 1 => vec!["add".into(), "rm".into()],
            "completions" if prefix.len() == 1 => SHELLS.iter().map(|s| s.to_string()).collect(),
            "add" => vec![
                "--name".into(),
                "--mac".into(),
                "--token".into(),
                "--url".into(),
            ],
            head => config
                .as_ref()
                .and_then(|c| c.resolve(head))
                .filter(|_| prefix.len() == 1)
                .map(|d| {
                    actions_for(&d.driver)
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default(),
        }
    };

    out.retain(|c| c.starts_with(current));
    out.sort();
    out.dedup();
    out
}

/// Print the candidates, one per line -- the format every shell below reads.
pub fn complete(words: &[String], config_path: &Path) {
    for candidate in candidates(words, config_path) {
        println!("{candidate}");
    }
}

/// Print the completion script for `shell`.
pub fn script(shell: &str) -> Result<(), AppError> {
    let body = match shell {
        "bash" => BASH,
        "zsh" => ZSH,
        "fish" => FISH,
        other => {
            return Err(AppError::Usage(format!(
                "unknown shell '{other}'; try one of: {}",
                SHELLS.join(", ")
            )));
        }
    };
    print!("{body}");
    Ok(())
}

const BASH: &str = r#"# d3home completion for bash. Install with:
#   d3home completions bash > ~/.local/share/bash-completion/completions/d3home
_d3home() {
    local IFS=$'\n'
    COMPREPLY=($(d3home __complete "${COMP_WORDS[@]:1:COMP_CWORD}" 2>/dev/null))
}
complete -F _d3home d3home
"#;

const ZSH: &str = r#"# d3home completion for zsh. Install by putting this on your fpath as _d3home.
#compdef d3home
_d3home() {
    local -a candidates
    candidates=(${(f)"$(d3home __complete ${words[2,CURRENT]} 2>/dev/null)"})
    compadd -- $candidates
}
_d3home "$@"
"#;

const FISH: &str = r#"# d3home completion for fish. Install with:
#   d3home completions fish > ~/.config/fish/completions/d3home.fish
function __d3home_complete
    set -l words (commandline -opc) (commandline -ct)
    d3home __complete $words[2..-1] 2>/dev/null
end
complete -c d3home -f -a '(__d3home_complete)'
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry in its own directory, named after the calling line so
    /// tests running as threads in one process cannot collide.
    fn registry(tag: u32) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("d3home-complete-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(
            &path,
            "[[devices]]\nname = \"kettle\"\naliases = [\"k\"]\ndriver = \"syncleo\"\nmac = \"deadbeefdead\"\ntoken = \"deadbeefdeadbeefdeadbeefdeadbeef\"\n",
        )
        .unwrap();
        path
    }

    fn words(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_first_word_offers_builtins_and_the_users_own_devices() {
        let path = registry(line!());
        let got = candidates(&words(&[""]), &path);
        for expected in ["add", "discover", "devices", "kettle", "k"] {
            assert!(
                got.contains(&expected.to_string()),
                "{expected} missing from {got:?}"
            );
        }
    }

    #[test]
    fn an_alias_completes_to_its_devices_actions() {
        let path = registry(line!());
        // Completing after an alias must work exactly as after the name.
        assert_eq!(
            candidates(&words(&["k", ""]), &path),
            candidates(&words(&["kettle", ""]), &path)
        );
        assert!(candidates(&words(&["k", ""]), &path).contains(&"watch".to_string()));
    }

    #[test]
    fn the_current_word_filters_the_candidates() {
        let path = registry(line!());
        assert_eq!(
            candidates(&words(&["kettle", "st"]), &path),
            vec![
                "start".to_string(),
                "status".to_string(),
                "stop".to_string()
            ]
        );
    }

    #[test]
    fn a_dash_offers_the_global_flags() {
        let path = registry(line!());
        assert!(
            candidates(&words(&["kettle", "status", "--"]), &path).contains(&"--json".to_string())
        );
    }

    #[test]
    fn a_missing_registry_still_completes_the_builtins() {
        // A broken or absent config must never make the shell feel broken.
        let got = candidates(
            &words(&[""]),
            std::path::Path::new("/nonexistent/devices.toml"),
        );
        assert!(got.contains(&"add".to_string()), "got {got:?}");
    }

    #[test]
    fn every_advertised_shell_has_a_script_and_others_are_refused() {
        for shell in SHELLS {
            assert!(script(shell).is_ok(), "no script for {shell}");
        }
        assert!(script("powershell").is_err());
    }
}
