//! Registering a device in the config.
//!
//! Three ways in, because they suit different moments: paste the share link
//! from the vendor app, answer a few prompts, or pass every field as a flag
//! for a script. Whatever the route, the token ends up in the config and
//! nowhere else -- not on screen, not in shell history.

use std::io::IsTerminal;
use std::path::Path;

use std::time::Duration;

use crate::cli::AppError;
use crate::config::{Config, ConfigError, Device, RESERVED};

/// How long to look around the network when `add` offers a list of
/// devices to pick from.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The fields a share link carries. The vendor app produces links like
/// `https://l.polaris-iot.com/device-share/polaris/57/deadbeefdead?token=…&name=PWK%201725CGLD`
/// -- the trailing path segment is the MAC, and the query carries the token
/// and the model name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareLink {
    pub mac: String,
    pub token: String,
    pub model: Option<String>,
    /// Who made it, taken from the segment after `device-share`. The
    /// protocol never says: mDNS advertises `_syncleo._udp`, and Syncleo is
    /// the platform a brand builds on rather than the brand itself. The
    /// link is the one place the name appears at all, so it is kept rather
    /// than thrown away and guessed at later from a model prefix.
    pub vendor: Option<String>,
}

/// Pull the device out of a share link.
///
/// Deliberately lenient about the host and scheme: the point is to accept
/// what the app actually put on the clipboard, not to police URLs. It is
/// strict about the two fields that matter, because a MAC or token that is
/// subtly wrong produces a device that never connects and no clue why.
pub fn parse_share_link(input: &str) -> Result<ShareLink, AppError> {
    let bad = |what: &str| {
        AppError::Usage(format!(
            "this does not look like a device-share link: {what}"
        ))
    };

    let (path, query) = input.split_once('?').ok_or_else(|| bad("no token in it"))?;
    let mac = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("no device address in the path"))?;

    // `/device-share/polaris/57/deadbeefdead` -- the vendor follows the
    // marker. What the number between them means is not known; it looks
    // like a model id in the vendor's own catalogue.
    let mut segments = path.split('/');
    let vendor = segments
        .find(|segment| *segment == "device-share")
        .and_then(|_| segments.next())
        .filter(|segment| !segment.is_empty())
        .map(str::to_string);

    let mut token = None;
    let mut model = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("token", value)) => token = Some(value.to_string()),
            Some(("name", value)) => model = Some(percent_decode(value)),
            _ => {}
        }
    }

    let token = token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| bad("no token in it"))?;
    Ok(ShareLink {
        mac: mac.to_string(),
        token,
        model,
        vendor,
    })
}

/// Minimal percent-decoding, enough for a model name like `PWK%201725CGLD`.
/// Anything malformed is left as written rather than dropped -- a mangled
/// model string is cosmetic, a silently truncated one is confusing.
fn percent_decode(input: &str) -> String {
    let bytes = input.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether we may ask the user questions: both the prompt and the answer
/// need a terminal, and a piped stdin means nobody is there to type.
pub fn can_prompt() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Everything `add` accepts, however it was spelled on the command line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AddRequest {
    pub name: Option<String>,
    pub mac: Option<String>,
    pub token: Option<String>,
    pub model: Option<String>,
    pub vendor: Option<String>,
}

/// Interpret `add`'s own words. A bare positional that contains `://` is a
/// share link; otherwise it is the device name.
pub fn parse_args(args: &[String]) -> Result<AddRequest, AppError> {
    let mut request = AddRequest::default();
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        let mut value = |name: &str| -> Result<String, AppError> {
            iter.next()
                .cloned()
                .ok_or_else(|| AppError::Usage(format!("{name} needs a value")))
        };
        match arg.as_str() {
            "--name" => request.name = Some(value("--name")?),
            "--mac" => request.mac = Some(value("--mac")?),
            "--token" => request.token = Some(value("--token")?),
            "--model" => request.model = Some(value("--model")?),
            "--vendor" => request.vendor = Some(value("--vendor")?),
            "--url" => apply_link(&mut request, &value("--url")?)?,
            other if other.contains("://") => apply_link(&mut request, other)?,
            other if other.starts_with('-') => {
                return Err(AppError::Usage(format!("unknown option '{other}' for add")));
            }
            other => request.name = Some(other.to_string()),
        }
    }

    Ok(request)
}

fn apply_link(request: &mut AddRequest, url: &str) -> Result<(), AppError> {
    let link = parse_share_link(url)?;
    request.mac = Some(link.mac);
    request.token = Some(link.token);
    if request.model.is_none() {
        request.model = link.model;
    }
    if request.vendor.is_none() {
        request.vendor = link.vendor;
    }
    Ok(())
}

/// Offer the devices currently visible on the network and return the MAC of
/// whichever one the user points at.
///
/// Only called when there is a terminal to draw on. `None` means "nothing
/// found, or the user chose to type it in", and the caller falls back to
/// asking -- a device that is off its base right now should not stop
/// someone from registering it.
pub fn pick_discovered() -> Option<String> {
    use syncleo::discovery::{Discovery, MdnsDiscovery};

    let discovery = MdnsDiscovery::new().ok()?;
    let found = discovery.find_all(DISCOVERY_TIMEOUT).ok()?;
    if found.is_empty() {
        return None;
    }

    let mut labels: Vec<String> = found
        .iter()
        .map(|f| format!("{}  at {}:{}", f.mac, f.address, f.port))
        .collect();
    labels.push("type the address myself".to_string());

    let choice = dialoguer::Select::new()
        .with_prompt("which device")
        .items(&labels)
        .default(0)
        .interact()
        .ok()?;

    found.get(choice).map(|f| f.mac.clone())
}

/// Validate a name before it reaches the config, so the failure names the
/// problem instead of surfacing later as a device that cannot be addressed.
fn check_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::Usage("a device name cannot be empty".into()));
    }
    if name.starts_with('-') {
        return Err(AppError::Usage(format!(
            "'{name}' starts with a dash, which would be read as an option"
        )));
    }
    if RESERVED.contains(&name) {
        return Err(AppError::Usage(format!(
            "'{name}' is a built-in command, so a device by that name could never be reached"
        )));
    }
    Ok(())
}

/// Build the device to write, filling gaps by asking when there is someone
/// to ask and failing with a precise complaint when there is not.
pub fn resolve(request: AddRequest, prompt: bool) -> Result<Device, AppError> {
    let missing = |what: &str, how: &str| {
        AppError::Usage(format!(
            "no {what} given; pass {how}, or run `d3home add` in a terminal"
        ))
    };

    let mac = match request.mac {
        Some(mac) => mac,
        None if prompt => ask("device address (mac)")?,
        None => return Err(missing("device address", "a share link or --mac")),
    };
    let token = match request.token {
        Some(token) => token,
        None if prompt => ask_secret("device token")?,
        None => return Err(missing("token", "a share link or --token")),
    };
    let name = match request.name {
        Some(name) => name,
        None if prompt => ask_with_default("name for this device", "kettle")?,
        None => return Err(missing("name", "--name")),
    };

    check_name(&name)?;
    let device = Device {
        name,
        aliases: Vec::new(),
        driver: "syncleo".into(),
        model: request.model,
        vendor: request.vendor,
        icon: None,
        mac,
        token,
        cached: None,
    };
    device
        .token_bytes()
        .map_err(|_| AppError::Usage("the token must be 32 hexadecimal characters".into()))?;
    Ok(device)
}

/// Add `device` to the registry at `path`, creating the registry if this is
/// the first device ever configured.
pub fn write(path: &Path, device: Device, json: bool) -> Result<(), AppError> {
    let mut config = match Config::load(path) {
        Ok(config) => config,
        // The first device is exactly when the file is supposed to appear.
        Err(ConfigError::NoRegistry { .. }) => Config::default(),
        Err(err) => return Err(err.into()),
    };

    if config.resolve(&device.name).is_some() {
        return Err(AppError::Usage(format!(
            "a device called '{}' is already configured",
            device.name
        )));
    }

    let name = device.name.clone();
    config.devices.push(device);
    config.validate()?;
    config.save(path)?;
    crate::output::print_added(&name, json);
    Ok(())
}

fn ask(what: &str) -> Result<String, AppError> {
    use std::io::Write;
    eprint!("{what}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| AppError::Usage(format!("could not read {what}: {e}")))?;
    Ok(line.trim().to_string())
}

fn ask_with_default(what: &str, default: &str) -> Result<String, AppError> {
    let answer = ask(&format!("{what} [{default}]"))?;
    Ok(if answer.is_empty() {
        default.to_string()
    } else {
        answer
    })
}

/// Read a secret without echoing it. The token is the key to the device;
/// it has no business on the screen or in a scrollback buffer.
fn ask_secret(what: &str) -> Result<String, AppError> {
    dialoguer::Password::new()
        .with_prompt(what)
        .interact()
        .map_err(|e| AppError::Usage(format!("could not read {what}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "https://l.polaris-iot.com/device-share/polaris/57/deadbeefdead?token=deadbeefdeadbeefdeadbeefdeadbeef&name=PWK%201725CGLD";

    #[test]
    fn reads_a_share_link_from_the_vendor_app() {
        let link = parse_share_link(LINK).unwrap();
        assert_eq!(link.mac, "deadbeefdead");
        assert_eq!(link.token, "deadbeefdeadbeefdeadbeefdeadbeef");
        assert_eq!(link.model.as_deref(), Some("PWK 1725CGLD"));
    }

    #[test]
    fn a_share_link_names_who_made_the_device() {
        // The protocol never says -- mDNS advertises `_syncleo._udp`, and
        // Syncleo is the platform, not the brand. The link is the only
        // place the name appears, and it used to be thrown away.
        assert_eq!(
            parse_share_link(LINK).unwrap().vendor.as_deref(),
            Some("polaris")
        );
    }

    #[test]
    fn a_link_of_another_shape_leaves_the_vendor_unknown() {
        // Better unknown than guessed: a wrong vendor would pick a wrong
        // icon and put another company's name on somebody's kettle.
        let link = parse_share_link(
            "https://example.com/deadbeefdead?token=deadbeefdeadbeefdeadbeefdeadbeef",
        )
        .unwrap();
        assert_eq!(link.vendor, None);
        assert_eq!(link.mac, "deadbeefdead");
    }

    #[test]
    fn an_explicit_vendor_outranks_the_one_in_the_link() {
        let args = [
            "--vendor".to_string(),
            "Polaris".to_string(),
            LINK.to_string(),
        ];
        assert_eq!(
            parse_args(&args).unwrap().vendor.as_deref(),
            Some("Polaris")
        );
    }

    #[test]
    fn refuses_a_link_with_nothing_useful_in_it() {
        for bad in [
            "https://example.com/device-share/polaris/57/deadbeefdead",
            "https://example.com/?token=",
            "not a url at all",
        ] {
            assert!(parse_share_link(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn a_bare_url_argument_is_understood_as_a_link() {
        let request = parse_args(&[LINK.to_string()]).unwrap();
        assert_eq!(request.mac.as_deref(), Some("deadbeefdead"));
        assert_eq!(request.model.as_deref(), Some("PWK 1725CGLD"));
        assert!(
            request.name.is_none(),
            "the link carries a model, not a name"
        );
    }

    #[test]
    fn a_bare_word_argument_is_understood_as_the_name() {
        let request = parse_args(&["kettle".to_string()]).unwrap();
        assert_eq!(request.name.as_deref(), Some("kettle"));
    }

    #[test]
    fn flags_and_a_link_can_be_mixed_with_flags_winning_on_the_name() {
        let args: Vec<String> = ["--name", "k", LINK]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let request = parse_args(&args).unwrap();
        assert_eq!(request.name.as_deref(), Some("k"));
        assert_eq!(
            request.token.as_deref(),
            Some("deadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    #[test]
    fn without_a_terminal_a_missing_field_names_itself() {
        let request = AddRequest {
            name: Some("kettle".into()),
            ..Default::default()
        };
        let err = resolve(request, false).unwrap_err().to_string();
        assert!(err.contains("device address"), "got: {err}");

        let request = AddRequest {
            name: Some("kettle".into()),
            mac: Some("deadbeefdead".into()),
            ..Default::default()
        };
        let err = resolve(request, false).unwrap_err().to_string();
        assert!(err.contains("token"), "got: {err}");
    }

    #[test]
    fn a_token_that_could_never_work_is_refused_before_it_reaches_the_config() {
        let request = AddRequest {
            name: Some("kettle".into()),
            mac: Some("deadbeefdead".into()),
            token: Some("nothex".into()),
            ..Default::default()
        };
        assert!(resolve(request, false).is_err());
    }

    #[test]
    fn a_name_that_could_never_be_typed_is_refused() {
        for name in ["", "--json", "discover"] {
            let request = AddRequest {
                name: Some(name.into()),
                mac: Some("deadbeefdead".into()),
                token: Some("deadbeefdeadbeefdeadbeefdeadbeef".into()),
                ..Default::default()
            };
            assert!(resolve(request, false).is_err(), "accepted name {name:?}");
        }
    }

    #[test]
    fn percent_escapes_in_a_model_name_are_decoded() {
        assert_eq!(percent_decode("PWK%201725CGLD"), "PWK 1725CGLD");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%", "a stray % is left alone");
    }
}
