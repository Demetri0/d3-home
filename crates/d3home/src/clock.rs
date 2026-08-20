//! Local wall-clock time, for logs a person reads.
//!
//! A trace of a kettle is compared against what someone remembers doing --
//! "I pressed start around half past" -- so the timestamps have to be local
//! time, not UTC, and not seconds since an epoch.

/// `HH:MM:SS`, local time.
pub fn hms() -> String {
    let (h, m, s, _) = parts();
    format!("{h:02}:{m:02}:{s:02}")
}

/// `HH:MM:SS.mmm`, local time. Milliseconds matter in a trace: the device's
/// post-handshake burst arrives inside a few tens of them, and the ordering
/// within it is the interesting part.
pub fn hms_millis() -> String {
    let (h, m, s, ms) = parts();
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

fn parts() -> (i32, i32, i32, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let tm = local_tm(now.as_secs() as libc::time_t);
    (tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_millis())
}

/// `localtime_r` rather than `localtime`: this is called from a thread that
/// is not the only one running.
#[cfg(unix)]
fn local_tm(secs: libc::time_t) -> libc::tm {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // Safety: both pointers are to live locals, and the call only writes
    // through the second.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    tm
}

/// The same call under the C runtime's own name for it, which takes its
/// arguments the other way round.
#[cfg(windows)]
fn local_tm(secs: libc::time_t) -> libc::tm {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // Safety: as above.
    unsafe { libc::localtime_s(&mut tm, &secs) };
    tm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shapes_are_what_a_log_column_expects() {
        let t = hms();
        assert_eq!(t.len(), 8, "HH:MM:SS, got {t:?}");
        assert_eq!(t.as_bytes()[2], b':');
        assert_eq!(t.as_bytes()[5], b':');

        let t = hms_millis();
        assert_eq!(t.len(), 12, "HH:MM:SS.mmm, got {t:?}");
        assert_eq!(t.as_bytes()[8], b'.');
    }

    #[test]
    fn the_fields_are_in_range() {
        let (h, m, s, ms) = parts();
        assert!((0..24).contains(&h), "hour {h}");
        assert!((0..60).contains(&m), "minute {m}");
        assert!((0..=60).contains(&s), "second {s}, leap seconds allowed");
        assert!(ms < 1000, "millis {ms}");
    }
}
