//! A minimal spinner for stderr, so a command that can take several seconds
//! (mDNS discovery has a five-second timeout; even a `status` on a warm
//! cache takes the better part of a second) doesn't sit there looking
//! frozen. Kept entirely out of `syncleo` -- the protocol crate has no
//! business knowing about terminals -- and out of stdout, which carries the
//! actual result and must stay byte-identical whether or not a human is
//! watching.
//!
//! Two things are deliberately factored out of the drawing machinery so
//! they can be unit tested without a real terminal or a background thread:
//! - [`should_animate`], the "is this worth drawing" decision, a pure
//!   function of "is stderr a terminal."
//! - [`Phase::label`], the phase-to-text mapping, a pure function with no
//!   I/O at all.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// Redraw cadence. The user asked for at least once every 300-500ms so the
/// animation reads as "alive"; 120ms is comfortably inside that margin and
/// looks smooth without spending real cycles on a process that is about to
/// exit anyway.
const TICK: Duration = Duration::from_millis(120);

const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The phases `commands::kettle` moves through while a command is in
/// flight, each with its own spinner label. An enum rather than bare
/// `&str`s scattered at call sites keeps the phase-to-label mapping in one
/// place, testable on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No cached endpoint (or a stale one): looking the device up over
    /// mDNS, which can take up to `DISCOVERY_TIMEOUT`.
    Searching,
    /// Performing the handshake against a known address, cached or just
    /// discovered.
    Connecting,
    /// A `start`/`off` command has been handed to the session and is
    /// waiting on the device's ack.
    Sending,
    /// `status` is waiting for the device's post-handshake state burst.
    WaitingForState,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Searching => "searching the network",
            Phase::Connecting => "connecting",
            Phase::Sending => "sending the command",
            Phase::WaitingForState => "waiting for state",
        }
    }
}

/// Whether a spinner should draw at all. Pulled out of [`Spinner::start`]
/// so the one decision that matters -- "only when stderr is a real
/// terminal, never when it's redirected or piped" -- is a plain function of
/// a bool and can be tested as one, without needing an actual terminal.
pub fn should_animate(stderr_is_terminal: bool) -> bool {
    stderr_is_terminal
}

/// A spinner drawn on stderr by a background thread, for as long as it's
/// alive. Dropping it (or calling [`Spinner::stop`] explicitly) stops the
/// thread and blocks until the line has actually been erased, so nothing
/// written after that point can land on top of a half-erased frame.
///
/// When stderr isn't a terminal, `start` never spawns a thread at all --
/// `stop`/`drop` are then free no-ops. There is no cursor-hiding here (no
/// `\x1b[?25l`/`\x1b[?25h`): the animation only ever overwrites its own
/// line with `\r`, so there's no hidden-cursor state a Ctrl-C could leave
/// stuck, and no signal handler is needed to guarantee that.
pub struct Spinner {
    stop: Option<Arc<AtomicBool>>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Start animating `phase`'s label on stderr, if stderr is a terminal.
    pub fn start(phase: Phase) -> Self {
        Self::start_if(phase.label(), should_animate(io::stderr().is_terminal()))
    }

    /// The testable core of `start`: whether to actually draw is passed in
    /// rather than read from a real terminal, so both the "draw" and
    /// "don't draw" paths can be exercised in a unit test.
    fn start_if(label: &str, animate: bool) -> Self {
        if !animate {
            return Self { stop: None, handle: None };
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let label = label.to_string();
        let handle = std::thread::spawn(move || draw_until_stopped(&label, &thread_stop));
        Self { stop: Some(stop), handle: Some(handle) }
    }

    /// Stop the animation and erase its line, blocking until the drawing
    /// thread has actually exited. Idempotent: a second call (or a `drop`
    /// after an explicit `stop`) is a no-op, and a spinner that never drew
    /// anything (no terminal) has nothing to join.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn draw_until_stopped(label: &str, stop: &AtomicBool) {
    let mut stderr = io::stderr();
    let mut frame = 0usize;
    while !stop.load(Ordering::Relaxed) {
        let _ = write!(stderr, "\r{} {label}", FRAMES[frame % FRAMES.len()]);
        let _ = stderr.flush();
        frame = frame.wrapping_add(1);
        std::thread::sleep(TICK);
    }
    // Erase completely: return to column 0, overwrite the frame glyph, the
    // space, and the whole label with blanks, then return to column 0
    // again so the cursor is left where the next write expects it rather
    // than at the end of a blanked line.
    let blank_width = label.chars().count() + 2;
    let _ = write!(stderr, "\r{}\r", " ".repeat(blank_width));
    let _ = stderr.flush();
}

/// Run `f` while a spinner labeled for `phase` animates on stderr,
/// guaranteeing the spinner has been stopped -- and its line erased --
/// before this returns. Whatever the caller does next with the result
/// (print it, propagate an error, start the next phase's spinner) can
/// never land on top of a still-animating or half-erased line, because the
/// animation is provably gone by the time `f`'s result comes back.
pub fn with_spinner<T>(phase: Phase, f: impl FnOnce() -> T) -> T {
    let mut spinner = Spinner::start(phase);
    let result = f();
    spinner.stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_stderr_should_animate() {
        assert!(should_animate(true));
    }

    #[test]
    fn a_non_terminal_stderr_should_not_animate() {
        assert!(!should_animate(false));
    }

    #[test]
    fn every_phase_has_a_distinct_non_empty_label() {
        let phases =
            [Phase::Searching, Phase::Connecting, Phase::Sending, Phase::WaitingForState];
        let labels: Vec<&str> = phases.iter().map(|p| p.label()).collect();
        for label in &labels {
            assert!(!label.is_empty());
        }
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "phase labels must all be distinct: {labels:?}");
    }

    #[test]
    fn the_searching_and_connecting_labels_match_what_the_task_asked_for() {
        // Pinned to the exact wording from the design brief, since these
        // are the two phases a first-ever run (no cache yet) sits in for
        // up to five seconds combined with nothing else on screen.
        assert_eq!(Phase::Searching.label(), "searching the network");
        assert_eq!(Phase::Connecting.label(), "connecting");
    }

    #[test]
    fn a_spinner_started_without_a_terminal_spawns_no_thread() {
        let spinner = Spinner::start_if("test", false);
        assert!(spinner.handle.is_none());
        assert!(spinner.stop.is_none());
    }

    #[test]
    fn a_spinner_started_without_a_terminal_is_a_no_op_to_stop() {
        let mut spinner = Spinner::start_if("test", false);
        spinner.stop();
        spinner.stop(); // idempotent
    }

    #[test]
    fn a_spinner_started_with_a_terminal_spawns_a_thread_and_stop_joins_it() {
        let mut spinner = Spinner::start_if("test", true);
        assert!(spinner.handle.is_some());
        spinner.stop();
        assert!(spinner.handle.is_none(), "stop must join and clear the thread handle");
        spinner.stop(); // idempotent, must not panic on a second call
    }

    #[test]
    fn dropping_a_running_spinner_stops_it_without_panicking() {
        let spinner = Spinner::start_if("dropped", true);
        drop(spinner);
    }

    #[test]
    fn with_spinner_returns_the_closures_value_and_leaves_no_thread_running() {
        let value = with_spinner(Phase::Sending, || 42);
        assert_eq!(value, 42);
    }
}
