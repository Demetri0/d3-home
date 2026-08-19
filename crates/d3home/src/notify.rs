//! Getting a notification in front of a person.
//!
//! No notification library is linked. `notify-rust` covers Linux, BSD and
//! macOS -- not Windows -- and pulls roughly 170 crates against this
//! project's 148 in total. Doubling the dependency tree to show a popup is a
//! bad trade, and the platform incantations are three lines each.

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

#[cfg(test)]
mod tests {
    use super::*;

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
