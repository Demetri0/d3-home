//! The temperature bar.
//!
//! Its own component because three places draw it -- `status`, `watch`, and
//! whatever comes next -- and a bar that drifts between them would quietly
//! teach the eye two different scales.

use crate::style::Style;

/// Render the bar.
///
/// The scale is absolute, 0 to 100 °C, so a glance always means the same
/// thing: the dots do not rescale when the target changes, and a kettle at
/// 40 °C looks the same whether it is heading for 60 or for boiling.
pub fn temperature_bar(current: u8, target: Option<u8>, heating: bool, style: Style) -> String {
    const CELLS: usize = 25;
    let cell_of = |t: u8| {
        (usize::from(t) * CELLS)
            .div_ceil(100)
            .min(CELLS)
            .saturating_sub(1)
    };

    let filled = cell_of(current);
    let target_cell = target.map(cell_of);

    let mut track = String::new();
    for i in 0..CELLS {
        if Some(i) == target_cell {
            // The one thing worth finding at a glance.
            track.push_str(&style.yellow("\u{25c9}"));
        } else if i <= filled {
            let dot = "\u{25cf}";
            track.push_str(&if heating {
                style.bright_green(dot)
            } else {
                // Idle, but the reading is still real -- bright enough to
                // read as data rather than as disabled chrome.
                style.bright_white(dot)
            });
        } else {
            track.push_str(&style.dim("\u{00b7}"));
        }
    }

    let mut out = format!("  {}  {track}", style.bold(&format!("{current} \u{00b0}C")));
    if let Some(target) = target {
        out.push_str(&format!("  {}", style.dim(&format!("{target} \u{00b0}C"))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(line: &str, glyph: char) -> usize {
        line.matches(glyph).count()
    }

    #[test]
    fn the_scale_is_absolute_so_the_same_reading_always_looks_the_same() {
        // The dots must not rescale when the target changes: 40 °C is 40 °C
        // whether the kettle is heading for 60 or for boiling.
        let to_sixty = temperature_bar(40, Some(60), true, Style::Rich);
        let to_boil = temperature_bar(40, Some(100), true, Style::Rich);
        assert_eq!(count(&to_sixty, '\u{25cf}'), count(&to_boil, '\u{25cf}'));
    }

    #[test]
    fn the_target_is_marked_with_a_ring_the_eye_can_find() {
        let line = temperature_bar(40, Some(60), true, Style::Rich);
        assert_eq!(count(&line, '\u{25c9}'), 1, "exactly one ring: {line:?}");
        assert!(
            line.contains("\u{1b}[93m"),
            "the ring should stand out: {line:?}"
        );
    }

    #[test]
    fn colour_says_whether_it_is_heating_not_the_shape() {
        let hot = temperature_bar(76, Some(100), true, Style::Rich);
        let cold = temperature_bar(76, Some(100), false, Style::Rich);
        assert_eq!(count(&hot, '\u{25cf}'), count(&cold, '\u{25cf}'));
        assert!(
            hot.contains("\u{1b}[92m"),
            "heating should be green: {hot:?}"
        );
        assert!(
            cold.contains("\u{1b}[97m"),
            "idle should still read as data: {cold:?}"
        );
        assert!(!cold.contains("\u{1b}[92m"));
    }

    #[test]
    fn without_a_target_there_is_no_ring() {
        let line = temperature_bar(41, None, false, Style::Rich);
        assert_eq!(count(&line, '\u{25c9}'), 0, "no target, no ring: {line:?}");
        assert!(line.contains("41"));
    }

    #[test]
    fn a_plain_terminal_gets_the_bar_without_escape_sequences() {
        let line = temperature_bar(40, Some(60), true, Style::Plain);
        assert!(
            !line.contains('\u{1b}'),
            "escape leaked into plain output: {line:?}"
        );
        assert!(line.contains("40") && line.contains("60"));
    }
}
