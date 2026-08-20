//! Watching for a keypress while something else is running.
//!
//! `watch` runs until interrupted, and Ctrl-C is a blunt way to say "enough".
//! Reading a single `q` needs the terminal in non-canonical mode, which is a
//! change to shared state that must be undone on every exit path -- including
//! the one where a signal kills the process before any destructor runs.
//!
//! All of that is POSIX. Windows reaches the same end through an entirely
//! different console API, which this project does not link, so there `q` does
//! nothing and Ctrl-C remains the way to stop -- unchanged, since it was
//! never this module's doing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use std::io::{IsTerminal, Read};
#[cfg(unix)]
use std::sync::atomic::AtomicPtr;

/// The terminal settings as we found them, kept where a signal handler can
/// reach them. A handler may not lock or allocate, so this is a raw pointer
/// to a leaked `termios` rather than anything friendlier.
#[cfg(unix)]
static ORIGINAL: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

/// Restore the terminal. Safe to call from a signal handler: one syscall,
/// no allocation, no locks.
#[cfg(unix)]
extern "C" fn restore_and_die(signal: i32) {
    let saved = ORIGINAL.load(Ordering::SeqCst);
    if !saved.is_null() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
    }
    // Re-raise with the default disposition so the exit status is the one
    // the shell expects from a signal, not a plain 0.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Set while the user has asked to stop.
#[derive(Clone, Default)]
pub struct Quit(Arc<AtomicBool>);

impl Quit {
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Only the key-reading thread asks, and that thread is POSIX. On
    /// Windows nothing can request a stop, which is the truth of it: there
    /// is no key reader there to press `q` at.
    #[cfg(unix)]
    fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Puts the terminal into non-canonical mode for as long as it lives, and
/// watches stdin for a quit key.
///
/// Echo is off so the keypress does not litter the output, but signal
/// generation is deliberately left on: Ctrl-C must keep working exactly as
/// it did, and a user who reaches for it should not have to discover that
/// this tool broke the habit.
pub struct QuitOnKey {
    quit: Quit,
    #[cfg(unix)]
    restore: Option<libc::termios>,
}

impl QuitOnKey {
    /// Start watching. Without a terminal there is nobody to press a key, so
    /// this does nothing at all and leaves the process untouched.
    #[cfg(unix)]
    pub fn start() -> Self {
        let quit = Quit::default();
        if !std::io::stdin().is_terminal() {
            return Self {
                quit,
                restore: None,
            };
        }

        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Self {
                quit,
                restore: None,
            };
        }

        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Self {
                quit,
                restore: None,
            };
        }

        // Hand the original to the signal handlers before arming them.
        ORIGINAL.store(Box::into_raw(Box::new(original)), Ordering::SeqCst);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let handler: extern "C" fn(i32) = restore_and_die;
            unsafe { libc::signal(signal, handler as *const () as libc::sighandler_t) };
        }

        let flag = quit.clone();
        std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            let mut stdin = std::io::stdin();
            while stdin.read_exact(&mut byte).is_ok() {
                // `q` to leave, and Escape because the habit is widespread
                // enough that people try it.
                if matches!(byte[0], b'q' | b'Q' | 0x1b) {
                    flag.request();
                    return;
                }
            }
        });

        Self {
            quit,
            restore: Some(original),
        }
    }

    /// Nothing to start: reading one key without waiting for Enter needs the
    /// Windows console API, which this project does not link. Ctrl-C still
    /// stops the program, exactly as it did before this type existed.
    #[cfg(not(unix))]
    pub fn start() -> Self {
        Self {
            quit: Quit::default(),
        }
    }

    pub fn quit(&self) -> Quit {
        self.quit.clone()
    }
}

#[cfg(unix)]
impl Drop for QuitOnKey {
    fn drop(&mut self) {
        if let Some(original) = self.restore {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
        }
    }
}

// Every test here is about a POSIX terminal: raw mode, and a flag that only
// a key reader can set. Windows has neither, so the module is not compiled
// there rather than left empty.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn without_a_terminal_nothing_is_touched_and_nothing_quits() {
        // Under a test harness stdin is not a tty, which is also the shape
        // of `watch` in a pipe: it must not change terminal state it does
        // not own, and must never decide on its own to stop.
        let watcher = QuitOnKey::start();
        assert!(
            watcher.restore.is_none(),
            "terminal settings were changed with no terminal"
        );
        assert!(!watcher.quit().requested());
    }

    #[test]
    fn the_flag_is_shared_between_clones() {
        let quit = Quit::default();
        let other = quit.clone();
        assert!(!other.requested());
        quit.request();
        assert!(other.requested(), "a clone must see the request");
    }
}
