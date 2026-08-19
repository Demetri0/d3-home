//! How much the terminal on the other end can take.
//!
//! One decision, made once: whether to draw colour and box-drawing glyphs,
//! or plain ASCII. Everything in `output` asks this rather than checking
//! `isatty` for itself, so a pipe never receives an escape sequence and a
//! capable terminal is never given a bare list because one call site forgot
//! to ask.

use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// A terminal that can show colour and non-ASCII glyphs.
    Rich,
    /// A pipe, a file, or a terminal that told us to keep it simple.
    Plain,
}

impl Style {
    /// Decide from the environment.
    ///
    /// Rich output needs a terminal to draw on, so a pipe or a redirect to a
    /// file rules it out -- escape sequences in the middle of piped data are
    /// corruption, not decoration. `NO_COLOR` is honoured because it is the
    /// convention users already reach for, and `TERM=dumb` because that is
    /// the terminal saying so itself.
    pub fn detect() -> Self {
        Self::decide(
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
            std::env::var("TERM").ok().as_deref() == Some("dumb"),
        )
    }

    /// The decision itself, separated from where the answers come from so
    /// it can be tested without a terminal.
    pub fn decide(is_terminal: bool, no_color: bool, dumb_term: bool) -> Self {
        if is_terminal && !no_color && !dumb_term { Self::Rich } else { Self::Plain }
    }

    pub fn is_rich(self) -> bool {
        self == Self::Rich
    }

    /// Wrap `text` in an SGR sequence, or return it untouched when plain.
    pub fn paint(self, code: &str, text: &str) -> String {
        match self {
            Self::Rich => format!("\x1b[{code}m{text}\x1b[0m"),
            Self::Plain => text.to_string(),
        }
    }

    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    /// Bright green: the kettle is doing something.
    pub fn bright_green(self, text: &str) -> String {
        self.paint("92", text)
    }

    /// Bright yellow: reserved for the one thing the eye should find first.
    pub fn yellow(self, text: &str) -> String {
        self.paint("93", text)
    }

    /// Bright white: a real reading, just not a heat in progress. Bright
    /// enough to read as data rather than as greyed-out chrome.
    pub fn bright_white(self, text: &str) -> String {
        self.paint("97", text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_capable_terminal_gets_rich_output() {
        assert_eq!(Style::decide(true, false, false), Style::Rich);
    }

    #[test]
    fn a_pipe_never_gets_escape_sequences() {
        // The important one: escape sequences in piped data are corruption.
        assert_eq!(Style::decide(false, false, false), Style::Plain);
        assert_eq!(Style::Plain.paint("31", "hot"), "hot");
        assert!(!Style::Plain.red("hot").contains('\x1b'));
    }

    #[test]
    fn the_user_and_the_terminal_can_both_say_no() {
        assert_eq!(Style::decide(true, true, false), Style::Plain, "NO_COLOR ignored");
        assert_eq!(Style::decide(true, false, true), Style::Plain, "TERM=dumb ignored");
    }

    #[test]
    fn rich_output_actually_paints() {
        let painted = Style::Rich.green("ok");
        assert!(painted.starts_with('\x1b') && painted.ends_with("\x1b[0m"), "got {painted:?}");
        assert!(painted.contains("ok"));
    }
}
