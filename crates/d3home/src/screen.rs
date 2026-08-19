//! A block of output that is redrawn in place.
//!
//! The bar, a blank line, and the last few log lines are treated as one
//! rectangle: to update it, move the cursor back to its top, wipe from there
//! down, and print the whole thing again. Nothing about the terminal itself
//! is changed -- no scroll region, no alternate screen, no modes -- so there
//! is nothing to restore and nothing that can be left behind if the process
//! dies badly.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};

/// How many log lines to keep under the bar. Enough to see what just
/// happened without the block taking over the screen.
const DEFAULT_LINES: usize = 15;

pub struct Block {
    active: bool,
    /// Rows the last render occupied, and therefore how far up the cursor
    /// has to travel to redraw over it.
    drawn_rows: usize,
    capacity: usize,
    width: usize,
    lines: VecDeque<String>,
}

impl Block {
    /// Without a terminal there is nothing to redraw over -- a pipe records
    /// every line, so a block that overwrote itself would lose most of them.
    pub fn new(enabled: bool) -> Self {
        let terminal = enabled && std::io::stdout().is_terminal();
        let (rows, cols) = size().unwrap_or((24, 80));
        // Leave room for the bar, the blank line, and a line of breathing
        // space, so the block never grows taller than the screen it is
        // redrawn on: the cursor arithmetic assumes nothing scrolled away.
        let capacity = DEFAULT_LINES
            .min(usize::from(rows).saturating_sub(4))
            .max(1);
        Self {
            active: terminal,
            drawn_rows: 0,
            capacity,
            width: usize::from(cols),
            lines: VecDeque::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Add a log line. Older lines fall off the top once the block is full.
    pub fn push(&mut self, line: String) {
        self.lines.push_back(line);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }

    /// Redraw the whole block: bar, blank line, log.
    pub fn render(&mut self, bar: &str) {
        if !self.active {
            return;
        }
        let mut out = String::new();
        if self.drawn_rows > 0 {
            // Back to the top of what we drew last time, then wipe from the
            // cursor to the end of the screen.
            out.push_str(&format!("\x1b[{}A", self.drawn_rows));
        }
        out.push_str("\x1b[J");
        out.push_str(&truncate(bar, self.width));
        out.push_str("\n\n");
        for line in &self.lines {
            out.push_str(&truncate(line, self.width));
            out.push('\n');
        }
        self.drawn_rows = 2 + self.lines.len();

        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }

    /// Stop redrawing and leave what is on screen where it is.
    pub fn finish(&mut self) {
        self.drawn_rows = 0;
    }
}

/// Cut a line to the terminal width, counting only what is actually visible:
/// an escape sequence takes bytes but no columns, and a line that wrapped
/// would occupy two rows and throw the cursor arithmetic out.
fn truncate(line: &str, width: usize) -> String {
    let mut out = String::new();
    let mut visible = 0usize;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            out.push(ch);
            // Copy the sequence through without counting it.
            for esc in chars.by_ref() {
                out.push(esc);
                if esc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible >= width {
            // Anything dropped may have left colour on; turn it off so the
            // rest of the screen is not tinted by a half-written line.
            out.push_str("\x1b[0m");
            break;
        }
        out.push(ch);
        visible += 1;
    }
    out
}

fn size() -> Option<(u16, u16)> {
    let mut winsize: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut winsize) } == 0;
    (ok && winsize.ws_row > 0 && winsize.ws_col > 0).then_some((winsize.ws_row, winsize.ws_col))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_terminal_nothing_is_drawn() {
        // In a pipe every line matters and nothing can be overwritten, so the
        // block must stay out of the way entirely.
        let mut block = Block::new(true);
        assert!(!block.is_active());
        block.push("a line".into());
        block.render("bar");
        assert_eq!(block.drawn_rows, 0, "drew over a pipe");
    }

    #[test]
    fn the_log_keeps_only_the_most_recent_lines() {
        let mut block = Block::new(false);
        block.capacity = 3;
        for i in 0..10 {
            block.push(format!("line {i}"));
        }
        assert_eq!(block.lines.len(), 3);
        assert_eq!(
            block.lines.front().unwrap(),
            "line 7",
            "oldest should fall off"
        );
        assert_eq!(block.lines.back().unwrap(), "line 9");
    }

    #[test]
    fn truncation_counts_columns_not_bytes() {
        // Colour is invisible on screen but not in the string; counting bytes
        // would cut a coloured line far too early.
        let painted = "\x1b[32mgreen\x1b[0m and more";
        assert_eq!(visible_len(&truncate(painted, 5)), 5);
        assert_eq!(visible_len(&truncate(painted, 100)), "green and more".len());
    }

    #[test]
    fn a_truncated_line_does_not_leave_colour_on() {
        let cut = truncate("\x1b[32mgreenish text", 4);
        assert!(cut.ends_with("\x1b[0m"), "colour left bleeding: {cut:?}");
    }

    fn visible_len(s: &str) -> usize {
        let mut n = 0;
        let mut chars = s.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for esc in chars.by_ref() {
                    if esc.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            n += 1;
        }
        n
    }
}
