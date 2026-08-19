//! Turning domain values into text on stdout. Nothing in this module
//! decides *when* to print or what to do about an error -- it only knows
//! how to render a [`DeviceState`], an [`Event`], a device list, or a
//! discovery result, in either human or JSON form. Keeping that decision
//! out of `commands::kettle` is what lets `watch` stay a stream: the
//! callback handed to `Client::watch` calls straight into `print_event`
//! per event, with no buffering or batching in between.

use serde_json::json;
use syncleo::client::DeviceState;
use syncleo::codec::command::{Event, PowerMode, decode_diagnostic};
use syncleo::discovery::Found;

use crate::config::{Config, Device, hex_encode};
use crate::style::Style;

fn mode_str(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Off => "off",
        PowerMode::On => "on",
        PowerMode::Custom => "custom",
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn fmt_temperature(t: Option<u8>) -> String {
    // A space before the unit, as the SI writes it: "34 °C", not "34°C".
    t.map_or_else(|| "unknown".to_string(), |v| format!("{v} \u{b0}C"))
}

fn fmt_flag(b: Option<bool>) -> &'static str {
    match b {
        Some(v) => yes_no(v),
        None => "unknown",
    }
}

/// Write one line to stdout and flush it, without panicking if the write
/// itself fails.
///
/// `println!` (and `writeln!` on the same handle) panics when the
/// underlying write fails. Rust ignores `SIGPIPE` on startup (`SIG_IGN`)
/// specifically so a broken pipe surfaces as a normal `io::Error` instead
/// of killing the process outright -- but the standard printing macros then
/// turn that `Err` right back into a panic, which exits 101, a code outside
/// this program's documented 0-6 contract. `watch` is explicitly meant to
/// be piped (into a notifier, a log, `jq`), so a downstream reader that
/// goes away early (`| head -1`, a killed notifier, a closed terminal) must
/// not crash this process; it means "stop watching," not "something broke."
fn write_line(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()
}

/// Print everything a [`crate::commands::kettle`] status check learned
/// about the device, either as one JSON object or as human-readable lines.
/// A temperature for the rich view. A target of zero is the device saying
/// it has none set, not a request to chill the water to freezing, so it
/// reads as an em dash rather than a number.
fn fmt_rich_temperature(t: Option<u8>, zero_is_none: bool) -> String {
    match t {
        Some(0) if zero_is_none => "\u{2014}".to_string(),
        Some(v) => format!("{v} \u{00b0}C"),
        None => "unknown".to_string(),
    }
}

/// The status block a capable terminal gets: a heading naming the device
/// and what it is doing, then the readings, indented so the eye can find
/// the numbers without reading the labels.
fn state_rich(state: &DeviceState, device: &Device, style: Style) -> String {
    let mode = state.mode.map(mode_str).unwrap_or("unknown");
    let dot = match state.mode {
        Some(PowerMode::Off) | None => style.dim("\u{25cf}"),
        Some(_) if state.error == Some(true) => style.red("\u{25cf}"),
        Some(_) => style.green("\u{25cf}"),
    };

    let mut out = String::from("\n  ");
    out.push_str(&style.bold(&device.name));
    // Model and driver, so a registry with several devices in it stays
    // legible: which kettle is this, and what speaks to it.
    if let Some(model) = &device.model {
        out.push_str(&style.dim(&format!("  {model}")));
    }
    out.push_str(&style.dim(&format!("  [{}]", device.driver)));
    out.push_str("   ");
    out.push_str(&dot);
    out.push(' ');
    out.push_str(mode);
    if let Some(target) = state.target_temperature.filter(|_| state.mode != Some(PowerMode::Off)) {
        out.push_str(&style.dim(&format!(" \u{2192} {target} \u{00b0}C")));
    }
    out.push_str("\n\n");

    let rows: [(&str, String); 4] = [
        ("Current temperature", fmt_rich_temperature(state.current_temperature, false)),
        ("Target temperature", fmt_rich_temperature(state.target_temperature, true)),
        ("Child lock", fmt_flag(state.child_lock).to_string()),
        (
            "Error",
            match state.error {
                Some(true) => style.red("yes"),
                other => fmt_flag(other).to_string(),
            },
        ),
    ];
    for (label, value) in rows {
        // Pad before painting: escape sequences have no width on screen but
        // every byte counts to `{:<21}`, so colouring first would push the
        // values out of line by exactly the length of the escape.
        out.push_str(&format!("  {}{}\n", style.dim(&format!("{label:<21}")), value));
    }

    if let Some(current) = state.current_temperature {
        let heating = matches!(state.mode, Some(PowerMode::On) | Some(PowerMode::Custom));
        out.push('\n');
        out.push_str(&temperature_bar(current, state.target_temperature, heating, style));
        out.push('\n');
    }
    out.pop();
    out
}

pub fn print_state(state: &DeviceState, device: &Device, json: bool) {
    if json {
        // `volume` (code 9) stays in the JSON form even though, on the
        // evidence gathered so far, it is useless: it read 0 on an empty
        // kettle, 0 with a full litre of water in it, and 0 immediately
        // after a full boil to 98°C. `--json` is where completeness beats
        // tidiness -- it's the shape anyone investigating the protocol
        // will look at -- so the field stays here even though the human
        // view below has stopped showing it. See [`Event::Volume`]'s doc
        // comment for what the byte is believed to be (nothing, on this
        // model).
        let value = json!({
            "current_temperature": state.current_temperature,
            "target_temperature": state.target_temperature,
            "mode": state.mode.map(mode_str),
            "volume": state.volume,
            "error": state.error,
            "child_lock": state.child_lock,
        });
        let _ = write_line(&value.to_string());
    } else {
        // No `volume` row here, deliberately. Three real-device readings
        // (empty, a full litre, and straight after boiling to 98°C) all
        // came back 0 -- the most likely explanation being that this
        // model, part of a Polaris IQ Home range that shares the wire
        // protocol, simply has no sensor behind code 9. A row that is
        // always zero and whose name we cannot justify is noise in a
        // status readout whose whole reason to exist is being less
        // annoying than the vendor app. The data itself is untouched --
        // `DeviceState::volume` and `Event::Volume` still carry it, `watch`
        // still prints it, and `--json` above still includes it.
        let style = Style::detect();
        if style.is_rich() {
            let _ = write_line(&state_rich(state, device, style));
        } else {
            let _ = write_line(&format!("Mode:                {}", state.mode.map(mode_str).unwrap_or("unknown")));
            let _ = write_line(&format!("Current temperature: {}", fmt_temperature(state.current_temperature)));
            let _ = write_line(&format!("Target temperature:  {}", fmt_temperature(state.target_temperature)));
            let _ = write_line(&format!("Child lock:          {}", fmt_flag(state.child_lock)));
            let _ = write_line(&format!("Error:               {}", fmt_flag(state.error)));
        }
    }
}

/// Print a single event as it arrives from [`syncleo::client::Client::watch`].
/// Called once per event, immediately -- `watch` in `commands::kettle` never
/// collects events into a buffer before calling this.
///
/// Returns whatever [`write_line`] returns: `Err` means the write itself
/// failed (most commonly a broken pipe downstream), and the caller -- the
/// `watch` reconnect loop in `commands::kettle` -- is the one that decides
/// what that means for the stream as a whole (stop watching; see that
/// module for why panicking here instead would be the wrong answer).
/// stdout is flushed as part of every write: `watch` is meant to be piped
/// (into a notifier, a log, `jq`, ...), and stdout is block-buffered rather
/// than line-buffered once it isn't a terminal, so without an explicit
/// flush a consumer reading the pipe could stall waiting for output sitting
/// in this process's buffer -- exactly the kind of thing that turns "a
/// stream" into "a stream that only delivers on exit."
pub fn print_event(event: &Event, json: bool) -> std::io::Result<()> {
    let line = if json { event_json(event).to_string() } else { event_human(event) };
    write_line(&line)
}

fn event_json(event: &Event) -> serde_json::Value {
    match event {
        Event::Mode(m) => json!({"mode": mode_str(*m)}),
        Event::TargetTemperature(t) => json!({"target_temperature": t}),
        Event::CurrentTemperature(t) => json!({"current_temperature": t}),
        Event::Volume(v) => json!({"volume": v}),
        Event::Error(b) => json!({"error": b}),
        Event::ChildLock(b) => json!({"child_lock": b}),
        Event::Backlight(b) => json!({"backlight": b}),
        Event::AccessControl(b) => json!({"access_control": b}),
        Event::Hardware(h) => json!({"hardware": h}),
        Event::Diagnostic(d) => {
            // The raw bytes are the field of record -- unconditionally
            // present, exactly as before, so nothing that used to be here
            // is lost. `diagnostic_decoded` is added on top, only when the
            // payload has the tag/value shape (see `decode_diagnostic`'s
            // doc comment); a shape that doesn't fit just omits this key
            // rather than guessing.
            let mut value = json!({"diagnostic": d});
            if let Some(pairs) = decode_diagnostic(d) {
                let decoded: Vec<_> =
                    pairs.into_iter().map(|(tag, v)| json!({"tag": tag, "value": v})).collect();
                value["diagnostic_decoded"] = serde_json::Value::Array(decoded);
            }
            value
        }
        Event::Ping => json!({"ping": true}),
        Event::HandshakeResponse { protocol, fw_major, fw_minor, mode } => json!({
            "handshake": {"protocol": protocol, "fw_major": fw_major, "fw_minor": fw_minor, "mode": mode}
        }),
        Event::Unknown { ty, data } => json!({"unknown": {"ty": ty, "data": data}}),
    }
}

/// Render one event as a human line.
fn event_human(event: &Event) -> String {
    match event {
        Event::Mode(m) => format!("mode: {}", mode_str(*m)),
        Event::TargetTemperature(t) => format!("target temperature: {t} \u{b0}C"),
        Event::CurrentTemperature(t) => format!("current temperature: {t} \u{b0}C"),
        Event::Volume(v) => format!("volume: {v}"),
        Event::Error(b) => format!("error: {}", yes_no(*b)),
        Event::ChildLock(b) => format!("child lock: {}", yes_no(*b)),
        Event::Backlight(b) => format!("backlight: {}", yes_no(*b)),
        Event::AccessControl(b) => format!("access control: {}", yes_no(*b)),
        // Confirmed against the real device: its vendor app reports "MCU
        // 1.1.4" for the same three bytes this decodes.
        Event::Hardware([major, minor, patch]) => format!("hardware: {major}.{minor}.{patch}"),
        // Code 145: a vendor diagnostic blob the device sends once per
        // session, right after the state burst -- firmware telemetry meant
        // for the vendor, not kettle state. We still just acknowledge and
        // discard it rather than forward it anywhere (see
        // `commands::kettle::watch`); this only changes how it's *shown*.
        // Every real capture so far decodes cleanly (see
        // `decode_diagnostic`'s doc comment): a 20-byte header followed by
        // 4-byte ASCII tag / 4-byte little-endian value pairs, e.g. `udps=1
        // IDLE=2`. When a payload doesn't fit that shape -- the device also
        // sends a bare one-byte `[0]` diagnostic in the same burst, which
        // never fits -- this falls back to the raw bytes rather than
        // hiding or guessing at it.
        Event::Diagnostic(d) => match decode_diagnostic(d) {
            Some(pairs) => {
                let rendered =
                    pairs.iter().map(|(tag, v)| format!("{tag}={v}")).collect::<Vec<_>>().join(" ");
                format!("diagnostic: {rendered}")
            }
            None => format!("diagnostic: {d:?}"),
        },
        Event::Ping => "ping".to_string(),
        Event::HandshakeResponse { protocol, fw_major, fw_minor, .. } => {
            format!("handshake: protocol {protocol}, firmware {fw_major}.{fw_minor}")
        }
        Event::Unknown { ty, data } => format!("unknown event {ty}: {data:?}"),
    }
}

/// Mark, in the printed event stream itself, the boundary between the
/// session that just ended and the one about to begin. `watch` reconnects
/// rather than exiting when the device goes away (see
/// `commands::kettle::watch`), and the device replays its whole
/// post-handshake state burst on every connection -- without a marker,
/// that repeated block of events would read as a glitch (the same values
/// reported twice) rather than what it is: a fresh session after the old
/// one was lost.
/// Same panic-avoidance and error-propagation reasoning as [`print_event`]:
/// `Err` means the write failed (most commonly a broken pipe), and the
/// caller decides what to do about it rather than this panicking on a
/// downstream reader that has simply gone away.
/// The temperature bar, shared by `watch` and `status`.
///
/// The scale is absolute, 0 to 100 °C, so a glance always means the same
/// thing: the dots do not rescale when the target changes, and a kettle at
/// 40 °C looks the same whether it is heading for 60 or for boiling.
pub fn temperature_bar(current: u8, target: Option<u8>, heating: bool, style: Style) -> String {
    const CELLS: usize = 25;
    let cell_of = |t: u8| (usize::from(t) * CELLS).div_ceil(100).min(CELLS).saturating_sub(1);

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

/// Renders a `watch` stream, keeping one live progress line at the bottom
/// on a capable terminal.
///
/// The device sends a temperature reading for every single degree, so a
/// full boil is nearly sixty lines of near-identical text. On a terminal
/// that can redraw, one line that moves is both shorter and easier to read
/// than sixty that scroll. Anywhere else -- a pipe, a log, `--json` -- every
/// reading stays its own line, because something is parsing them.
pub struct WatchView {
    style: Style,
    json: bool,
    target: Option<u8>,
    current: Option<u8>,
    /// Whether the kettle is actually heating. The device keeps its target
    /// while switched off, so a bar drawn from the target alone would
    /// promise a heat that is not happening.
    heating: bool,
    live: bool,
}

impl WatchView {
    pub fn new(json: bool) -> Self {
        Self {
            style: Style::detect(),
            json,
            target: None,
            current: None,
            heating: false,
            live: false,
        }
    }

    fn animated(&self) -> bool {
        self.style.is_rich() && !self.json
    }

    /// Put the bar on screen before anything has arrived, so the user sees
    /// straight away that something is being watched rather than staring at
    /// a blank terminal until the kettle happens to say something.
    pub fn start(&mut self) {
        if self.animated() {
            self.redraw();
        }
    }

    /// Erase the live line so ordinary output can be written without
    /// landing on top of it.
    fn clear_live(&mut self) {
        if self.live {
            self.live = false;
            print!("\r{:width$}\r", "", width = 56);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }

    /// Draw the bar as the last thing on screen.
    fn redraw(&mut self) {
        self.clear_live();
        print!("\r{}", self.status_line());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        self.live = true;
    }

    pub fn event(&mut self, event: &Event) -> std::io::Result<()> {
        match event {
            Event::TargetTemperature(t) => self.target = Some(*t),
            Event::CurrentTemperature(t) => self.current = Some(*t),
            Event::Mode(mode) => self.heating = *mode != PowerMode::Off,
            _ => {}
        }

        if !self.animated() {
            return print_event(event, self.json);
        }

        // Temperature arrives once per degree; on a terminal those belong in
        // the bar, not as sixty near-identical lines. Everything else is news
        // and gets a line of its own, printed above the bar.
        if !matches!(event, Event::CurrentTemperature(_)) {
            self.clear_live();
            print_event(event, self.json)?;
        }
        self.redraw();
        Ok(())
    }

    /// Called when the stream ends, so the bar is not left dangling without
    /// a newline.
    pub fn finish(&mut self) {
        if self.live {
            self.live = false;
            let _ = write_line("");
        }
    }

    /// The bar, or the best summary available so far.
    fn status_line(&self) -> String {
        let current = match self.current {
            Some(current) => current,
            None => return format!("  {}", self.style.dim("waiting for the kettle...")),
        };
        let reading = self.style.bold(&format!("{current} \u{00b0}C"));

        // The bar is drawn whether or not the kettle is heating: an absolute
        // scale is meaningful either way, and the colour says which it is.
        self.bar(current).unwrap_or_else(|| format!("  {reading}"))
    }

    fn bar(&self, current: u8) -> Option<String> {
        Some(temperature_bar(current, self.target, self.heating, self.style))
    }
}

pub fn print_watch_reconnected(json: bool) -> std::io::Result<()> {
    let line = if json { reconnected_json().to_string() } else { RECONNECTED_HUMAN.to_string() };
    write_line(&line)
}

const RECONNECTED_HUMAN: &str = "--- reconnected ---";

fn reconnected_json() -> serde_json::Value {
    json!({"reconnected": true})
}

/// List the configured devices and their aliases -- but never the token,
/// even under `--json`.
pub fn print_devices(config: &Config, json: bool) {
    if json {
        let list: Vec<_> = config
            .devices
            .iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "aliases": d.aliases,
                    "driver": d.driver,
                    "model": d.model,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(list));
    } else if config.devices.is_empty() {
        println!("no devices configured");
    } else {
        for device in &config.devices {
            if device.aliases.is_empty() {
                println!("{}", device.name);
            } else {
                println!("{} ({})", device.name, device.aliases.join(", "));
            }
        }
    }
}

/// Report what `discover` found on the network, including each device's
/// public key: the design's hand-write escape hatch for a config's
/// `[devices.cached]` section (used when mDNS can't reach the device, e.g.
/// a blocked firewall) needs `address`, `port` *and* `public_key`, and this
/// is the only place the public key is ever surfaced to the operator.
pub fn print_found(found: &[Found], json: bool) {
    if json {
        let list: Vec<_> = found.iter().map(found_json).collect();
        println!("{}", serde_json::Value::Array(list));
    } else if found.is_empty() {
        println!("no devices found");
    } else {
        for f in found {
            println!("{}", found_human(f));
        }
    }
}

fn found_json(f: &Found) -> serde_json::Value {
    json!({
        "mac": f.mac,
        // `address` carries the `addr%iface` form for a scoped link-local
        // address -- the same thing `ping` and a hand-filled
        // `[devices.cached]` need -- while `interface` repeats just the
        // name, which is what actually goes in that config's separate
        // `interface` field (its `address` can't hold a `%zone` suffix:
        // that's not part of `IpAddr`'s textual form).
        "address": f.address_display(),
        "interface": f.interface,
        "port": f.port,
        "public_key": hex_encode(&f.public_wire),
    })
}

fn found_human(f: &Found) -> String {
    format!("{} at {}:{} (public key: {})", f.mac, f.address_display(), f.port, hex_encode(&f.public_wire))
}

/// Confirm a heat that the device acknowledged.
pub fn print_heating_started(target: u8, json: bool) {
    if json {
        println!("{{\"action\":\"start\",\"target_temperature\":{target}}}");
    } else {
        println!("heating to {target} \u{00b0}C");
    }
}

/// Confirm a target the device acknowledged, with no mode change.
pub fn print_target_set(target: u8, json: bool) {
    if json {
        println!("{{\"action\":\"set\",\"target_temperature\":{target}}}");
    } else {
        println!("target set to {target} \u{00b0}C, kettle not started");
    }
}

/// Confirm that the kettle was told to stop.
pub fn print_stopped(json: bool) {
    if json {
        println!("{{\"action\":\"off\"}}");
    } else {
        println!("stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_device() -> Device {
        Device {
            name: "kettle".into(),
            aliases: Vec::new(),
            driver: "syncleo".into(),
            model: Some("PWK 1725CGLD".into()),
            mac: "aabbccddeeff".into(),
            token: "0123456789abcdef0123456789abcdef".into(),
            cached: None,
        }
    }

    #[test]
    fn the_bar_is_on_screen_before_the_kettle_has_said_anything() {
        // Staring at a blank terminal until the device happens to speak is
        // indistinguishable from the tool being broken.
        let view = WatchView::new(false);
        let line = view.status_line();
        assert!(line.contains("waiting"), "got {line:?}");
    }

    fn view_at(current: u8, target: Option<u8>, heating: bool) -> WatchView {
        WatchView { style: Style::Rich, json: false, target, current: Some(current), heating, live: false }
    }

    fn count(line: &str, glyph: char) -> usize {
        line.matches(glyph).count()
    }

    #[test]
    fn the_scale_is_absolute_so_the_same_reading_always_looks_the_same() {
        // The dots must not rescale when the target changes: 40 °C is 40 °C
        // whether the kettle is heading for 60 or for boiling.
        let to_sixty = view_at(40, Some(60), true).bar(40).unwrap();
        let to_boil = view_at(40, Some(100), true).bar(40).unwrap();
        assert_eq!(count(&to_sixty, '\u{25cf}'), count(&to_boil, '\u{25cf}'));
    }

    #[test]
    fn the_target_is_marked_with_a_ring_the_eye_can_find() {
        let heating = view_at(40, Some(60), true).bar(40).unwrap();
        assert_eq!(count(&heating, '\u{25c9}'), 1, "exactly one ring: {heating:?}");
        assert!(heating.contains("\u{1b}[93m"), "the ring should stand out: {heating:?}");
    }

    #[test]
    fn colour_says_whether_it_is_heating_not_the_shape() {
        // Same temperature, same dots -- only the colour differs, so a
        // glance at an idle kettle is never mistaken for a heat in progress.
        let hot = view_at(76, Some(100), true).bar(76).unwrap();
        let cold = view_at(76, Some(100), false).bar(76).unwrap();
        assert_eq!(count(&hot, '\u{25cf}'), count(&cold, '\u{25cf}'));
        assert!(hot.contains("\u{1b}[92m"), "heating should be green: {hot:?}");
        assert!(cold.contains("\u{1b}[97m"), "idle should still read as data: {cold:?}");
        assert!(!cold.contains("\u{1b}[92m"));
    }

    #[test]
    fn a_kettle_that_has_not_named_a_target_still_gets_a_bar() {
        let line = view_at(41, None, false).bar(41).unwrap();
        assert_eq!(count(&line, '\u{25c9}'), 0, "no target, no ring: {line:?}");
        assert!(line.contains("41"));
    }

    #[test]
    fn every_event_keeps_the_bar_as_the_last_thing_drawn() {
        // The bar must not vanish between temperature readings, so any
        // event redraws it.
        let mut view = view_at(50, Some(60), true);
        view.event(&Event::Backlight(true)).unwrap();
        assert!(view.live, "an unrelated event left no bar behind");
        view.finish();
    }

    #[test]
    fn json_and_plain_streams_keep_one_line_per_reading() {
        // Something is parsing those, so they must not be collapsed.
        for (style, json) in [(Style::Plain, false), (Style::Rich, true), (Style::Plain, true)] {
            let view = WatchView { style, json, target: Some(60), current: Some(40), heating: true, live: false };
            assert!(!view.animated(), "style {style:?} json {json} should not animate");
        }
        assert!(view_at(40, Some(60), true).animated());
    }

    #[test]
    fn the_rich_status_block_lines_its_values_up() {
        // Escape sequences have no width on screen but plenty in bytes, so
        // padding has to happen before painting or the columns wander.
        let state = DeviceState {
            current_temperature: Some(78),
            target_temperature: Some(0),
            mode: Some(PowerMode::Off),
            volume: Some(0),
            error: Some(false),
            child_lock: Some(false),
        };
        let block = state_rich(&state, &sample_device(), Style::Rich);

        // Only the reading rows, which begin with a dim label. The heading
        // also carries a dim escape (the status dot) and is not a column.
        let columns: Vec<usize> = block
            .lines()
            .filter(|l| l.starts_with("  \u{1b}[2m"))
            .filter_map(|l| l.find("\u{1b}[0m").map(|i| i + "\u{1b}[0m".len()))
            .collect();
        assert!(columns.len() >= 4, "expected the reading rows, got {block:?}");
        assert!(columns.windows(2).all(|w| w[0] == w[1]), "values not aligned: {columns:?}");
        assert!(block.contains("78 \u{00b0}C"));
        assert!(block.contains('\u{2014}'), "a target of 0 should read as a dash: {block:?}");
    }

    #[test]
    fn a_plain_terminal_gets_no_escape_sequences_in_the_status_block() {
        let state = DeviceState {
            current_temperature: Some(78),
            target_temperature: Some(60),
            mode: Some(PowerMode::On),
            volume: Some(0),
            error: Some(true),
            child_lock: Some(false),
        };
        let block = state_rich(&state, &sample_device(), Style::Plain);
        assert!(!block.contains('\u{1b}'), "escape leaked into plain output: {block:?}");
        assert!(block.contains("78"), "the reading itself must survive: {block:?}");
    }
    use std::net::Ipv4Addr;
    use syncleo::codec::command::Event;

    #[test]
    fn human_watch_shows_a_decodable_diagnostic_as_tag_value_pairs() {
        // The same worked example `decode_diagnostic` is golden-tested
        // against, exercised here through the actual rendering path.
        let payload: Vec<u8> = vec![
            255, 2, 0, 0, 172, 56, 0, 0, 192, 111, 65, 4, 0, 0, 0, 0, 157, 47, 54, 3, // header
            117, 100, 112, 115, 186, 236, 7, 3, // udps = 50851002
            114, 116, 84, 0, 249, 9, 4, 0, // rtT\0 = 264697
            112, 112, 84, 0, 52, 211, 11, 0, // ppT\0 = 774964
            84, 109, 114, 32, 218, 9, 5, 0, // "Tmr " = 330202
        ];
        let line = event_human(&Event::Diagnostic(payload));
        assert_eq!(line, "diagnostic: udps=50851002 rtT=264697 ppT=774964 Tmr =330202");
    }

    #[test]
    fn human_watch_falls_back_to_raw_bytes_for_a_diagnostic_that_does_not_decode() {
        // The one-byte diagnostic the real device also sends in the same
        // burst never fits the tag/value shape.
        let line = event_human(&Event::Diagnostic(vec![0]));
        assert_eq!(line, "diagnostic: [0]");
    }

    #[test]
    fn json_watch_keeps_the_raw_diagnostic_bytes_and_adds_the_decoded_form() {
        let payload = vec![0u8; 20]
            .into_iter()
            .chain(*b"IDLE")
            .chain(7u32.to_le_bytes())
            .collect::<Vec<u8>>();
        let value = event_json(&Event::Diagnostic(payload.clone()));
        assert_eq!(value["diagnostic"], json!(payload));
        assert_eq!(value["diagnostic_decoded"], json!([{"tag": "IDLE", "value": 7}]));
    }

    #[test]
    fn json_watch_omits_the_decoded_form_when_the_payload_does_not_decode() {
        let value = event_json(&Event::Diagnostic(vec![0]));
        assert_eq!(value["diagnostic"], json!([0]));
        assert!(value.get("diagnostic_decoded").is_none());
    }

    #[test]
    fn the_json_reconnect_marker_is_a_well_formed_event_line() {
        // Piped `--json` output must stay one-JSON-object-per-line even at
        // the seam between sessions -- a consumer that parses every line
        // (`jq`, a notifier) must not choke on this one.
        let line = reconnected_json().to_string();
        let value: serde_json::Value = serde_json::from_str(&line).expect("must be one JSON object");
        assert_eq!(value["reconnected"], serde_json::json!(true));
    }

    #[test]
    fn the_human_reconnect_marker_says_so_plainly() {
        assert!(RECONNECTED_HUMAN.to_lowercase().contains("reconnect"));
    }

    fn sample_found() -> Found {
        Found {
            mac: "aabbccddeeff".into(),
            address: Ipv4Addr::new(192, 168, 1, 42).into(),
            interface: None,
            port: 8888,
            public_wire: [0xAB; 32],
            curve: 29,
            protocol: 2,
        }
    }

    fn link_local_found() -> Found {
        Found {
            mac: "aabbccddeeff".into(),
            address: "fe80::dead:beef:dead:beef".parse().unwrap(),
            interface: Some("enp8s0".into()),
            port: 8888,
            public_wire: [0xAB; 32],
            curve: 29,
            protocol: 2,
        }
    }

    #[test]
    fn discover_prints_the_public_key_in_human_output() {
        // Without this, an operator whose mDNS is blocked has no way to
        // hand-fill `[devices.cached]`'s `public_key` field, and the cache
        // escape hatch the design describes is unwalkable.
        let line = found_human(&sample_found());
        assert!(line.contains(&hex_encode(&[0xAB; 32])), "public key missing from: {line}");
    }

    #[test]
    fn discover_prints_the_public_key_in_json_output() {
        let value = found_json(&sample_found());
        assert_eq!(value["public_key"], hex_encode(&[0xAB; 32]));
    }

    #[test]
    fn a_scoped_address_is_rendered_in_the_ping_pasteable_form_in_human_output() {
        // This is the exact form confirmed against the real device: `ping
        // fe80::dead:beef:dead:beef%enp8s0` succeeds; the bare address
        // (what this printed before the fix) does not, because the kernel
        // can't tell which link a link-local address lives on.
        let line = found_human(&link_local_found());
        assert!(
            line.contains("fe80::dead:beef:dead:beef%enp8s0"),
            "expected the %iface form in: {line}"
        );
    }

    #[test]
    fn a_scoped_address_is_rendered_in_the_ping_pasteable_form_in_json_output() {
        let value = found_json(&link_local_found());
        assert_eq!(value["address"], "fe80::dead:beef:dead:beef%enp8s0");
        assert_eq!(value["interface"], "enp8s0");
    }

    #[test]
    fn a_global_address_has_no_percent_suffix_in_either_output() {
        let human = found_human(&sample_found());
        assert!(!human.contains('%'), "a global address needs no scope: {human}");

        let value = found_json(&sample_found());
        assert_eq!(value["address"], "192.168.1.42");
        assert!(value["interface"].is_null());
    }
}
