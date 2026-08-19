//! A line pinned to the top of the terminal while output scrolls beneath it.
//!
//! Done with the terminal's own scroll region: rows 1 and 2 are taken out of
//! it, so everything printed normally scrolls in the rows below and never
//! disturbs what is written above. The region is terminal-wide state -- a
//! process that exits without resetting it leaves the user's shell scrolling
//! inside a box -- so resetting it is wired into the same signal handling
//! that restores the terminal mode.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set while a scroll region is in force, so the signal handler in `keys`
/// knows whether it also has to reset one.
pub static REGION_SET: AtomicBool = AtomicBool::new(false);

/// Reset the scroll region. Safe to call from a signal handler: one write to
/// a file descriptor, no allocation and no locks.
pub fn reset_region_raw() {
    if REGION_SET.swap(false, Ordering::SeqCst) {
        const RESET: &[u8] = b"\x1b[r";
        unsafe {
            libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
        }
    }
}

/// Rows reserved at the top: the bar itself, then a blank line separating it
/// from the log.
const RESERVED: u16 = 2;

pub struct TopBar {
    active: bool,
}

impl TopBar {
    /// Reserve the top of the screen. Does nothing at all without a terminal
    /// to reserve it on, so piped output is untouched.
    pub fn new(enabled: bool) -> Self {
        if !enabled || !std::io::stdout().is_terminal() {
            return Self { active: false };
        }
        let Some(rows) = terminal_rows() else {
            return Self { active: false };
        };
        if rows <= RESERVED + 1 {
            // Too short to give any away and still have a log worth reading.
            return Self { active: false };
        }

        let mut out = std::io::stdout();
        // Scroll the reserved rows into existence first, so the region is
        // carved out of blank space rather than out of whatever was on
        // screen when the command started.
        let _ = write!(out, "\n\n\x1b[{};{}r\x1b[{};1H", RESERVED + 1, rows, RESERVED + 1);
        let _ = out.flush();
        REGION_SET.store(true, Ordering::SeqCst);
        Self { active: true }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Redraw the pinned line, leaving the cursor exactly where it was so the
    /// log below carries on undisturbed.
    pub fn draw(&self, line: &str) {
        if !self.active {
            return;
        }
        let mut out = std::io::stdout();
        // Save cursor, go home, clear the row, write, restore cursor.
        let _ = write!(out, "\x1b7\x1b[1;1H\x1b[2K{line}\x1b8");
        let _ = out.flush();
    }
}

impl Drop for TopBar {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        reset_region_raw();
        // Leave the cursor below the reserved rows rather than inside them,
        // so the shell prompt does not land on top of the bar.
        let mut out = std::io::stdout();
        let _ = writeln!(out);
        let _ = out.flush();
    }
}

fn terminal_rows() -> Option<u16> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_row > 0).then_some(size.ws_row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_terminal_nothing_is_reserved() {
        // Under a test harness, and in a pipe, there is no screen to carve
        // up -- and an escape sequence in piped output is corruption.
        let bar = TopBar::new(true);
        assert!(!bar.is_active());
        assert!(!REGION_SET.load(Ordering::SeqCst));
    }

    #[test]
    fn disabled_means_disabled_even_with_a_terminal() {
        let bar = TopBar::new(false);
        assert!(!bar.is_active());
    }

    #[test]
    fn resetting_twice_writes_once() {
        REGION_SET.store(true, Ordering::SeqCst);
        reset_region_raw();
        assert!(!REGION_SET.load(Ordering::SeqCst));
        // The second call must be a no-op rather than emitting a stray reset
        // into whatever is on the terminal by then.
        reset_region_raw();
        assert!(!REGION_SET.load(Ordering::SeqCst));
    }
}
