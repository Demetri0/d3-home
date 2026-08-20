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

use crate::bar::temperature_bar;
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
    if let Some(target) = state
        .target_temperature
        .filter(|_| state.mode != Some(PowerMode::Off))
    {
        out.push_str(&style.dim(&format!(" \u{2192} {target} \u{00b0}C")));
    }
    out.push_str("\n\n");

    // Directly under the heading: the bar answers "how hot, and how far to
    // go" at a glance, and the rows below are the detail behind it.
    if let Some(current) = state.current_temperature {
        let heating = matches!(state.mode, Some(PowerMode::On) | Some(PowerMode::Custom));
        out.push_str(&temperature_bar(
            current,
            state.target_temperature,
            heating,
            style,
        ));
        out.push_str("\n\n");
    }

    let rows: [(&str, String); 4] = [
        (
            "Current temperature",
            fmt_rich_temperature(state.current_temperature, false),
        ),
        (
            "Target temperature",
            fmt_rich_temperature(state.target_temperature, true),
        ),
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
        out.push_str(&format!(
            "  {}{}\n",
            style.dim(&format!("{label:<21}")),
            value
        ));
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
            let _ = write_line(&format!(
                "Mode:                {}",
                state.mode.map(mode_str).unwrap_or("unknown")
            ));
            let _ = write_line(&format!(
                "Current temperature: {}",
                fmt_temperature(state.current_temperature)
            ));
            let _ = write_line(&format!(
                "Target temperature:  {}",
                fmt_temperature(state.target_temperature)
            ));
            let _ = write_line(&format!(
                "Child lock:          {}",
                fmt_flag(state.child_lock)
            ));
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
    let line = if json {
        event_json(event).to_string()
    } else {
        event_human(event)
    };
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
                let decoded: Vec<_> = pairs
                    .into_iter()
                    .map(|(tag, v)| json!({"tag": tag, "value": v}))
                    .collect();
                value["diagnostic_decoded"] = serde_json::Value::Array(decoded);
            }
            value
        }
        Event::Ping => json!({"ping": true}),
        Event::HandshakeResponse {
            protocol,
            fw_major,
            fw_minor,
            mode,
        } => json!({
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
                let rendered = pairs
                    .iter()
                    .map(|(tag, v)| format!("{tag}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("diagnostic: {rendered}")
            }
            None => format!("diagnostic: {d:?}"),
        },
        Event::Ping => "ping".to_string(),
        Event::HandshakeResponse {
            protocol,
            fw_major,
            fw_minor,
            ..
        } => {
            format!("handshake: protocol {protocol}, firmware {fw_major}.{fw_minor}")
        }
        Event::Unknown { ty, data } => format!("unknown event {ty}: {data:?}"),
    }
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
    error: Option<bool>,
    child_lock: Option<bool>,
    /// The device reports the same values over and over; a person wants to
    /// know when one of them *changed*. Nothing is announced until there is
    /// something to compare against.
    announced: bool,
    /// The redrawn block: the bar, a blank line, and the last few log lines.
    /// Inactive without a terminal, where every line must be kept.
    block: crate::screen::Block,
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
            error: None,
            child_lock: None,
            announced: false,
            block: crate::screen::Block::new(false),
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
            self.block = crate::screen::Block::new(true);
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
        if self.block.is_active() {
            let line = self.status_line();
            self.block.render(&line);
            return;
        }
        self.clear_live();
        print!("\r{}", self.status_line());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        self.live = true;
    }

    pub fn event(&mut self, event: &Event) -> std::io::Result<()> {
        // `--json` is the machine stream, and it is the same for `watch` and
        // `trace`: everything, unfiltered. The filtering below is only about
        // what a person wants to read.
        if self.json {
            return print_event(event, true);
        }

        if let Some(note) = self.absorb(event) {
            let line = format!("{}  {note}", self.style.dim(&crate::clock::hms()));
            if self.block.is_active() {
                self.block.push(line);
            } else {
                self.clear_live();
                write_line(&line)?;
            }
        }
        if self.animated() {
            self.redraw();
        }
        Ok(())
    }

    /// Fold the event into what we know, and say what a person should be
    /// told about it, if anything.
    fn absorb(&mut self, event: &Event) -> Option<String> {
        match event {
            Event::CurrentTemperature(t) => {
                let first = self.current.is_none();
                self.current = Some(*t);
                // The reading itself lives in the bar; only the first one is
                // worth a line, as "here is what you attached to".
                if first && !self.announced {
                    self.announced = true;
                    return Some(self.connected_note());
                }
                None
            }
            Event::TargetTemperature(t) => {
                let previous = self.target.replace(*t);
                match previous {
                    Some(old) if old != *t && self.heating => {
                        Some(format!("target changed to {t} \u{00b0}C"))
                    }
                    _ => None,
                }
            }
            Event::Mode(mode) => {
                let was = self.heating;
                self.heating = *mode != PowerMode::Off;
                if !self.announced || was == self.heating {
                    return None;
                }
                Some(if self.heating {
                    match self.target {
                        Some(target) => format!("heating to {target} \u{00b0}C"),
                        None => "heating".to_string(),
                    }
                } else {
                    match (self.current, self.target) {
                        // Within a couple of degrees of the target is the
                        // kettle finishing, not somebody stopping it. The
                        // same tolerance decides `boiled` in
                        // `commands::daemon`, and the two must agree.
                        // Saturating, because a device reporting 255 would
                        // otherwise overflow the addition.
                        (Some(current), Some(target)) if current.saturating_add(2) >= target => {
                            format!("reached {current} \u{00b0}C, switched off")
                        }
                        (Some(current), _) => format!("switched off at {current} \u{00b0}C"),
                        _ => "switched off".to_string(),
                    }
                })
            }
            Event::Error(flag) => match self.error.replace(*flag) {
                Some(was) if was != *flag => Some(if *flag {
                    self.style.red("the kettle reports an error")
                } else {
                    "error cleared".to_string()
                }),
                _ => None,
            },
            Event::ChildLock(flag) => match self.child_lock.replace(*flag) {
                Some(was) if was != *flag => {
                    Some(format!("child lock {}", if *flag { "on" } else { "off" }))
                }
                _ => None,
            },
            // Diagnostics, hardware, access control, volume, the unidentified
            // codes: real data, none of it something a person watching a
            // kettle asked to see. `trace` is where all of it goes.
            _ => None,
        }
    }

    fn connected_note(&self) -> String {
        let current = self
            .current
            .map_or_else(|| "unknown".into(), |t| format!("{t} \u{00b0}C"));
        match (self.heating, self.target) {
            (true, Some(target)) => {
                format!("connected \u{2014} {current}, heating to {target} \u{00b0}C")
            }
            (true, None) => format!("connected \u{2014} {current}, heating"),
            (false, _) => format!("connected \u{2014} {current}, idle"),
        }
    }

    /// Called when the stream ends, so the bar is not left dangling without
    /// a newline.
    pub fn finish(&mut self) {
        self.block.finish();
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
        Some(temperature_bar(
            current,
            self.target,
            self.heating,
            self.style,
        ))
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
pub fn print_watch_reconnected(json: bool) -> std::io::Result<()> {
    let line = if json {
        reconnected_json().to_string()
    } else {
        RECONNECTED_HUMAN.to_string()
    };
    write_line(&line)
}

const RECONNECTED_HUMAN: &str = "--- reconnected ---";

fn reconnected_json() -> serde_json::Value {
    json!({"reconnected": true})
}

/// List the configured devices and their aliases -- but never the token,
/// even under `--json`.
/// Human-readable MAC: stored flat, read with separators.
///
/// The MAC is the identifier worth showing, not a slice of the token. It is
/// already broadcast over mDNS so it is in no sense secret, and it is what
/// the router's admin page and the vendor's share link both display -- which
/// makes it the thing that ties a line in this list to a physical object on
/// a worktop. Printing part of a token would be a habit worth not forming.
fn pretty_mac(mac: &str) -> String {
    mac.as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).to_string())
        .collect::<Vec<_>>()
        .join(":")
}

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
                    "vendor": d.vendor,
                    "model": d.model,
                    "mac": d.mac,
                    "endpoint": d.cached.as_ref().map(|c| format!("{}:{}", c.address, c.port)),
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(list));
        return;
    }

    if config.devices.is_empty() {
        println!("no devices configured -- run `d3home add` to register one");
        return;
    }

    let style = Style::detect();
    for (index, device) in config.devices.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let mut heading = style.bold(&device.name);
        if !device.aliases.is_empty() {
            heading.push_str(&style.dim(&format!("  ({})", device.aliases.join(", "))));
        }
        println!("  {heading}");

        let rows = [
            (
                "vendor",
                device.vendor.clone().unwrap_or_else(|| "unknown".into()),
            ),
            (
                "model",
                device.model.clone().unwrap_or_else(|| "unknown".into()),
            ),
            ("driver", device.driver.clone()),
            ("mac", pretty_mac(&device.mac)),
            (
                "endpoint",
                match &device.cached {
                    Some(cached) => format!("{}:{}", cached.address, cached.port),
                    // Not an error: it simply has not been found yet, and the
                    // next command will look for it.
                    None => "not yet discovered".into(),
                },
            ),
        ];
        for (label, value) in rows {
            println!("  {}{}", style.dim(&format!("{label:<10}")), value);
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
    format!(
        "{} at {}:{} (public key: {})",
        f.mac,
        f.address_display(),
        f.port,
        hex_encode(&f.public_wire)
    )
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

/// The raw event log: every report the device makes, in the order it makes
/// them, with the protocol code beside the decoded meaning.
///
/// This is the view for taking the protocol apart rather than for watching a
/// kettle. It is where the two command codes nobody has identified show
/// their bytes, and where the ordering inside the post-handshake burst is
/// visible at all.
pub struct TraceView {
    style: Style,
    json: bool,
}

impl TraceView {
    pub fn new(json: bool) -> Self {
        Self {
            style: Style::detect(),
            json,
        }
    }

    pub fn event(&mut self, event: &Event) -> std::io::Result<()> {
        if self.json {
            return print_event(event, true);
        }
        write_line(&format!(
            "{}  {}  {}",
            self.style.dim(&crate::clock::hms_millis()),
            self.style.yellow(&format!("{:>3}", event.code())),
            event_human(event),
        ))
    }

    /// A session boundary matters in a trace: the whole burst is about to
    /// repeat, and unmarked it reads as duplicated data.
    pub fn reconnected(&mut self) -> std::io::Result<()> {
        if self.json {
            return print_watch_reconnected(true);
        }
        write_line(&self.style.dim("--- reconnected ---"))
    }
}

/// What a streaming command does with the events it receives.
///
/// `watch` and `trace` listen to exactly the same stream and differ only in
/// what they make of it, so the loop that owns the connection is written
/// once and told which of these to feed.
pub trait EventSink {
    /// Called before the first event, for a view that wants something on
    /// screen straight away.
    fn start(&mut self) {}
    fn event(&mut self, event: &Event) -> std::io::Result<()>;
    /// Called when the connection is lost, before the wait to get it back.
    /// A view that only draws has nothing to do here; a sink that reports
    /// the device going away does.
    fn disconnected(&mut self) {}
    fn reconnected(&mut self) -> std::io::Result<()>;
    /// Called when the stream ends, to tidy anything left mid-line.
    fn finish(&mut self) {}
}

impl EventSink for WatchView {
    fn start(&mut self) {
        WatchView::start(self);
    }
    fn event(&mut self, event: &Event) -> std::io::Result<()> {
        WatchView::event(self, event)
    }
    fn reconnected(&mut self) -> std::io::Result<()> {
        self.clear_live();
        print_watch_reconnected(self.json)
    }
    fn finish(&mut self) {
        WatchView::finish(self);
    }
}

impl EventSink for TraceView {
    fn event(&mut self, event: &Event) -> std::io::Result<()> {
        TraceView::event(self, event)
    }
    fn reconnected(&mut self) -> std::io::Result<()> {
        TraceView::reconnected(self)
    }
}

/// Report a failure.
///
/// `--json` changes the shape, not the stream: failures stay on stderr so
/// that stdout carries only the answer and a redirect to a file is never
/// polluted by an error. The exit code says what happened too, but a code
/// alone cannot say *which* device or *what* the device reported.
pub fn print_error(err: &crate::cli::AppError, json: bool) {
    if json {
        let value = json!({
            "error": {
                "kind": err.kind(),
                "exit_code": err.exit_code() as i32,
                "message": err.to_string(),
            }
        });
        eprintln!("{value}");
    } else {
        eprintln!("d3home: {err}");
    }
}

/// Whether this run was asked for machine-readable output.
///
/// A process-wide setting rather than an argument threaded everywhere,
/// because the places that need it are as deep as `Config::load`'s
/// permission check, and making the config layer take a presentation flag
/// would be worse than this. Set once, at startup, in a single-shot CLI.
static JSON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_json(json: bool) {
    JSON.store(json, std::sync::atomic::Ordering::Relaxed);
}

fn json_mode() -> bool {
    JSON.load(std::sync::atomic::Ordering::Relaxed)
}

/// Something worth saying that is not a failure: the command carries on.
/// Always stderr, so it never lands in the middle of the answer.
pub fn print_warning(message: &str) {
    if json_mode() {
        eprintln!("{}", json!({ "warning": { "message": message } }));
    } else {
        eprintln!("d3home: warning: {message}");
    }
}

/// Confirm a device was registered.
pub fn print_added(name: &str, json: bool) {
    if json {
        println!("{}", json!({ "action": "add", "device": name }));
    } else {
        println!("added '{name}'");
    }
}

/// The program and its version, in whichever shape was asked for.
pub fn print_version(json: bool) {
    let version = env!("CARGO_PKG_VERSION");
    if json {
        println!("{}", json!({ "name": "d3home", "version": version }));
    } else {
        println!("d3home {version}");
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
            vendor: Some("polaris".into()),
            icon: None,
            mac: "deadbeefdead".into(),
            token: "deadbeefdeadbeefdeadbeefdeadbeef".into(),
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
        WatchView {
            style: Style::Rich,
            json: false,
            target,
            current: Some(current),
            heating,
            error: None,
            child_lock: None,
            announced: true,
            block: crate::screen::Block::new(false),
            live: false,
        }
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
        for (style, json) in [
            (Style::Plain, false),
            (Style::Rich, true),
            (Style::Plain, true),
        ] {
            let view = WatchView {
                style,
                json,
                target: Some(60),
                current: Some(40),
                heating: true,
                error: None,
                child_lock: None,
                announced: true,
                block: crate::screen::Block::new(false),
                live: false,
            };
            assert!(
                !view.animated(),
                "style {style:?} json {json} should not animate"
            );
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
        assert!(
            columns.len() >= 4,
            "expected the reading rows, got {block:?}"
        );
        assert!(
            columns.windows(2).all(|w| w[0] == w[1]),
            "values not aligned: {columns:?}"
        );
        assert!(block.contains("78 \u{00b0}C"));
        assert!(
            block.contains('\u{2014}'),
            "a target of 0 should read as a dash: {block:?}"
        );
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
        assert!(
            !block.contains('\u{1b}'),
            "escape leaked into plain output: {block:?}"
        );
        assert!(
            block.contains("78"),
            "the reading itself must survive: {block:?}"
        );
    }
    use std::net::Ipv4Addr;
    use syncleo::codec::command::Event;

    #[test]
    fn human_watch_shows_a_decodable_diagnostic_as_tag_value_pairs() {
        // The same worked example `decode_diagnostic` is golden-tested
        // against, exercised here through the actual rendering path.
        let payload: Vec<u8> = vec![
            255, 2, 0, 0, 172, 56, 0, 0, 192, 111, 65, 4, 0, 0, 0, 0, 157, 47, 54,
            3, // header
            117, 100, 112, 115, 186, 236, 7, 3, // udps = 50851002
            114, 116, 84, 0, 249, 9, 4, 0, // rtT\0 = 264697
            112, 112, 84, 0, 52, 211, 11, 0, // ppT\0 = 774964
            84, 109, 114, 32, 218, 9, 5, 0, // "Tmr " = 330202
        ];
        let line = event_human(&Event::Diagnostic(payload));
        assert_eq!(
            line,
            "diagnostic: udps=50851002 rtT=264697 ppT=774964 Tmr =330202"
        );
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
        assert_eq!(
            value["diagnostic_decoded"],
            json!([{"tag": "IDLE", "value": 7}])
        );
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
        let value: serde_json::Value =
            serde_json::from_str(&line).expect("must be one JSON object");
        assert_eq!(value["reconnected"], serde_json::json!(true));
    }

    #[test]
    fn the_human_reconnect_marker_says_so_plainly() {
        assert!(RECONNECTED_HUMAN.to_lowercase().contains("reconnect"));
    }

    fn sample_found() -> Found {
        Found {
            mac: "deadbeefdead".into(),
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
            mac: "deadbeefdead".into(),
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
        assert!(
            line.contains(&hex_encode(&[0xAB; 32])),
            "public key missing from: {line}"
        );
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
        assert!(
            !human.contains('%'),
            "a global address needs no scope: {human}"
        );

        let value = found_json(&sample_found());
        assert_eq!(value["address"], "192.168.1.42");
        assert!(value["interface"].is_null());
    }
}
