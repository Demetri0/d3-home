//! Getting a notification in front of a person.
//!
//! No notification library is linked. `notify-rust` covers Linux, BSD and
//! macOS -- not Windows -- and pulls roughly 170 crates against this
//! project's 148 in total. Doubling the dependency tree to show a popup is a
//! bad trade, and the platform incantations are three lines each.

use std::process::{Command, Stdio};

/// A way of putting a notification on a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    NotifySend,
    KdialogPassive,
    Zenity,
    Osascript,
    PowerShell,
}

impl Backend {
    pub fn binary(self) -> &'static str {
        match self {
            Self::NotifySend => "notify-send",
            Self::KdialogPassive => "kdialog",
            Self::Zenity => "zenity",
            Self::Osascript => "osascript",
            Self::PowerShell => "powershell.exe",
        }
    }
}

/// Preference order. `notify-send` first because it is the freedesktop
/// standard rather than one desktop's tool; the next two are what a KDE or
/// GTK system has when libnotify's binary is not installed; the last two are
/// built into macOS and Windows and are always present there.
const ORDER: [Backend; 5] = [
    Backend::NotifySend,
    Backend::KdialogPassive,
    Backend::Zenity,
    Backend::Osascript,
    Backend::PowerShell,
];

/// Pick a backend, given a way to ask whether a command exists.
///
/// Existence only -- nothing is executed and nothing appears on screen. The
/// check is a parameter so the order table can be tested without a
/// filesystem, and so a test never depends on what happens to be installed.
pub fn detect(exists: impl Fn(&str) -> bool) -> Option<Backend> {
    ORDER.into_iter().find(|backend| exists(backend.binary()))
}

/// Detection against the real `PATH`.
pub fn detect_on_path() -> Option<Backend> {
    detect(on_path)
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// One thing worth telling somebody about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub event: &'static str,
    pub device: String,
    pub title: String,
    pub body: String,
    pub temperature: Option<u8>,
    pub target: Option<u8>,
    /// A name from the icon theme, or a path. A name is the better answer,
    /// and the reason the packaging installs into `hicolor`: it survives the
    /// file being moved and it follows whatever theme the user has chosen.
    /// An unknown name costs nothing -- the notification simply arrives
    /// without a picture.
    pub icon: Option<String>,
}

/// The application name notifications are sent under, matching the
/// `d3home.desktop` the packaging installs. A desktop uses it to tie the
/// popup to an application, and through that to an icon.
const APP_ID: &str = "d3home";

/// Build the command for a built-in backend, without running it.
pub fn backend_command(backend: Backend, n: &Notification) -> Command {
    let mut command = Command::new(backend.binary());
    match backend {
        Backend::NotifySend => {
            command.args(["--app-name", APP_ID]);
            // How a freedesktop notification is tied to an installed
            // application, and the reason `d3home.desktop` exists at all.
            command.args(["--hint", &format!("string:desktop-entry:{APP_ID}")]);
            if let Some(icon) = &n.icon {
                command.args(["-i", icon]);
            }
            command.args([n.title.as_str(), n.body.as_str()]);
        }
        Backend::KdialogPassive => {
            if let Some(icon) = &n.icon {
                command.args(["--icon", icon]);
            }
            command.args(["--title", &n.title, "--passivepopup", &n.body, "10"]);
        }
        Backend::Zenity => {
            if let Some(icon) = &n.icon {
                command.args([format!("--icon={icon}")]);
            }
            command.args(["--notification", &format!("--text={}: {}", n.title, n.body)]);
        }
        Backend::Osascript => {
            command.args([
                "-e",
                &format!(
                    "display notification {} with title {}",
                    quote_applescript(&n.body),
                    quote_applescript(&n.title)
                ),
            ]);
        }
        Backend::PowerShell => {
            command.args(["-NoProfile", "-Command", &powershell_toast(n)]);
        }
    }
    command
}

/// Build a user-supplied command, without running it.
///
/// The event reaches it through the environment rather than through string
/// interpolation: a device name or body containing a quote must not be able
/// to change what runs.
pub fn custom_command(shell_command: &str, n: &Notification) -> Command {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", shell_command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", shell_command]);
        c
    };
    command.env("D3HOME_EVENT", n.event);
    command.env("D3HOME_DEVICE", &n.device);
    command.env("D3HOME_TITLE", &n.title);
    command.env("D3HOME_BODY", &n.body);
    if let Some(temperature) = n.temperature {
        command.env("D3HOME_TEMPERATURE", temperature.to_string());
    }
    if let Some(target) = n.target {
        command.env("D3HOME_TARGET", target.to_string());
    }
    command
}

fn quote_applescript(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn powershell_toast(n: &Notification) -> String {
    // The body goes through a PowerShell single-quoted string, where the
    // only escape needed is a doubled quote.
    //
    // Windows takes its icon from the executable's own resources, where the
    // build script puts it -- that platform has neither an icon theme to
    // name nor a convention of a file beside the binary. The fallback
    // covers a build with no resource compiled in.
    let escape = |s: &str| s.replace('\'', "''");
    format!(
        "[reflection.assembly]::LoadWithPartialName('System.Windows.Forms') > $null; \
         $b = New-Object System.Windows.Forms.NotifyIcon; \
         $b.Icon = try {{ \
             [System.Drawing.Icon]::ExtractAssociatedIcon((Get-Process -id $pid).Path) \
         }} catch {{ [System.Drawing.SystemIcons]::Information }}; \
         $b.Visible = $true; \
         $b.ShowBalloonTip(10000, '{}', '{}', 'Info')",
        escape(&n.title),
        escape(&n.body)
    )
}

/// Delivers notifications, by whichever route was chosen once at startup.
pub struct Notifier {
    custom: Option<String>,
    backend: Option<Backend>,
}

impl Notifier {
    pub fn new(custom: Option<String>) -> Self {
        let backend = if custom.is_some() {
            None
        } else {
            detect_on_path()
        };
        Self { custom, backend }
    }

    /// What this will actually do, for the startup line. The choice should
    /// never be a mystery to somebody wondering why nothing appeared.
    pub fn describe(&self) -> String {
        match (&self.custom, self.backend) {
            (Some(command), _) => format!("a command: {command}"),
            (None, Some(backend)) => backend.binary().to_string(),
            (None, None) => "nothing -- no notifier found on PATH".to_string(),
        }
    }

    /// Send one. Failure is reported and never fatal: missing a popup is not
    /// a reason to stop watching a kettle.
    ///
    /// A zero exit means the command ran, not that anything appeared on a
    /// screen -- the same distinction as the device's frame acknowledgement.
    /// Nothing here claims delivery.
    pub fn deliver(&self, n: &Notification) {
        let mut command = match (&self.custom, self.backend) {
            (Some(shell_command), _) => custom_command(shell_command, n),
            (None, Some(backend)) => backend_command(backend, n),
            (None, None) => return,
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                // Reaped so the daemon does not accumulate zombies over a
                // long life; the exit status is not evidence of anything.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(err) => eprintln!("d3home: could not run the notifier: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Notification {
        Notification {
            event: "boiled",
            device: "kettle".into(),
            title: "PWK 1725CGLD \u{b7} kettle".into(),
            body: "Heating complete".into(),
            temperature: Some(98),
            target: Some(100),
            icon: Some("d3home".into()),
        }
    }

    fn args_of(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn every_backend_gets_the_text_into_its_own_arguments() {
        for backend in [
            Backend::NotifySend,
            Backend::KdialogPassive,
            Backend::Zenity,
            Backend::Osascript,
            Backend::PowerShell,
        ] {
            let command = backend_command(backend, &sample());
            let joined = args_of(&command).join(" ");
            assert!(
                joined.contains("Heating complete"),
                "{backend:?} lost the body: {joined}"
            );
            assert!(
                joined.contains("PWK 1725CGLD"),
                "{backend:?} lost the title: {joined}"
            );
            assert_eq!(command.get_program(), backend.binary());
        }
    }

    #[test]
    fn a_backend_that_can_show_an_icon_is_given_one() {
        for backend in [
            Backend::NotifySend,
            Backend::KdialogPassive,
            Backend::Zenity,
        ] {
            let joined = args_of(&backend_command(backend, &sample())).join(" ");
            assert!(
                joined.contains("d3home"),
                "{backend:?} dropped the icon: {joined}"
            );
        }
    }

    #[test]
    fn no_icon_configured_means_no_icon_argument() {
        // An empty `-i` would be worse than none: notify-send would take the
        // title as the icon name and lose it from the notification.
        let mut n = sample();
        n.icon = None;
        let joined = args_of(&backend_command(Backend::NotifySend, &n)).join(" ");
        assert!(
            !joined.contains(" -i "),
            "an icon flag appeared without an icon: {joined}"
        );
        assert!(
            joined.contains("Heating complete"),
            "the body went missing: {joined}"
        );
    }

    #[test]
    fn a_freedesktop_notification_names_the_application_it_came_from() {
        // The desktop-entry hint ties the popup to the installed
        // d3home.desktop, and through it to the icon theme.
        let joined = args_of(&backend_command(Backend::NotifySend, &sample())).join(" ");
        assert!(
            joined.contains("string:desktop-entry:d3home"),
            "no desktop-entry hint: {joined}"
        );
    }

    #[test]
    fn a_custom_command_receives_the_event_in_its_environment() {
        // Through the environment rather than interpolated into the string:
        // a body containing a quote must not be able to change what runs.
        let command = custom_command("ntfy publish kettle \"$D3HOME_BODY\"", &sample());
        let env: std::collections::HashMap<String, String> = command
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();

        assert_eq!(env.get("D3HOME_EVENT").map(String::as_str), Some("boiled"));
        assert_eq!(env.get("D3HOME_DEVICE").map(String::as_str), Some("kettle"));
        assert_eq!(
            env.get("D3HOME_TEMPERATURE").map(String::as_str),
            Some("98")
        );
        assert_eq!(env.get("D3HOME_TARGET").map(String::as_str), Some("100"));
    }

    #[test]
    fn a_hostile_body_cannot_escape_into_the_command() {
        let mut n = sample();
        n.body = "\"; rm -rf ~; echo \"".into();
        let command = custom_command("echo \"$D3HOME_BODY\"", &n);
        let joined = args_of(&command).join(" ");
        assert!(
            !joined.contains("rm -rf"),
            "the body reached the command line: {joined}"
        );
    }

    #[test]
    fn a_missing_optional_reading_is_simply_absent() {
        let mut n = sample();
        n.target = None;
        let command = custom_command("true", &n);
        let names: Vec<String> = command
            .get_envs()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.contains(&"D3HOME_TARGET".to_string()),
            "an empty variable is worse than none"
        );
    }

    #[test]
    fn describe_names_what_will_actually_be_used() {
        // The daemon prints this at startup so the choice is never a mystery.
        assert!(
            Notifier::new(Some("ntfy publish x".into()))
                .describe()
                .contains("ntfy")
        );
        assert!(!Notifier::new(None).describe().is_empty());
    }

    fn only<'a>(available: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |name: &str| available.contains(&name)
    }

    #[test]
    fn nothing_available_means_nothing_to_deliver_with() {
        // The normal state on a server reached over SSH. Not an error.
        assert_eq!(detect(only(&[])), None);
    }

    #[test]
    fn the_freedesktop_tool_wins_when_present() {
        // notify-send is a standard rather than one desktop's tool: GNOME,
        // KDE, dunst, mako and swaync all answer the same interface.
        assert_eq!(
            detect(only(&["notify-send", "kdialog", "zenity"])),
            Some(Backend::NotifySend)
        );
    }

    #[test]
    fn each_fallback_is_reachable_in_order() {
        assert_eq!(
            detect(only(&["kdialog", "zenity"])),
            Some(Backend::KdialogPassive)
        );
        assert_eq!(detect(only(&["zenity"])), Some(Backend::Zenity));
        assert_eq!(detect(only(&["osascript"])), Some(Backend::Osascript));
        assert_eq!(detect(only(&["powershell.exe"])), Some(Backend::PowerShell));
    }

    #[test]
    fn the_development_machines_shape_is_handled() {
        // Measured, not imagined: the machine this was written on has no
        // notify-send but does have kdialog and zenity. An implementation
        // that assumed notify-send would have failed on the first machine
        // it met.
        assert_eq!(
            detect(only(&["kdialog", "zenity"])),
            Some(Backend::KdialogPassive)
        );
    }

    #[test]
    fn every_backend_names_a_binary() {
        for backend in [
            Backend::NotifySend,
            Backend::KdialogPassive,
            Backend::Zenity,
            Backend::Osascript,
            Backend::PowerShell,
        ] {
            assert!(!backend.binary().is_empty(), "{backend:?} has no binary");
        }
    }
}
