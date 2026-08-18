//! Turning domain values into text on stdout. Nothing in this module
//! decides *when* to print or what to do about an error -- it only knows
//! how to render a [`DeviceState`], an [`Event`], a device list, or a
//! discovery result, in either human or JSON form. Keeping that decision
//! out of `commands::kettle` is what lets `watch` stay a stream: the
//! callback handed to `Client::watch` calls straight into `print_event`
//! per event, with no buffering or batching in between.

use serde_json::json;
use syncleo::client::DeviceState;
use syncleo::codec::command::{Event, PowerMode};
use syncleo::discovery::Found;

use crate::config::{Config, hex_encode};

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
    t.map_or_else(|| "unknown".to_string(), |v| format!("{v}\u{b0}C"))
}

fn fmt_flag(b: Option<bool>) -> &'static str {
    match b {
        Some(v) => yes_no(v),
        None => "unknown",
    }
}

/// Print everything a [`crate::commands::kettle`] status check learned
/// about the device, either as one JSON object or as human-readable lines.
pub fn print_state(state: &DeviceState, json: bool) {
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
        println!("{value}");
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
        println!("mode:                {}", state.mode.map(mode_str).unwrap_or("unknown"));
        println!("current temperature: {}", fmt_temperature(state.current_temperature));
        println!("target temperature:  {}", fmt_temperature(state.target_temperature));
        println!("error:               {}", fmt_flag(state.error));
        println!("child lock:          {}", fmt_flag(state.child_lock));
    }
}

/// Print a single event as it arrives from [`syncleo::client::Client::watch`].
/// Called once per event, immediately -- `watch` in `commands::kettle` never
/// collects events into a buffer before calling this.
pub fn print_event(event: &Event, json: bool) {
    if json {
        println!("{}", event_json(event));
    } else if let Some(line) = event_human(event) {
        println!("{line}");
    } else {
        // `event_human` returning `None` means this event is deliberately
        // not shown in the human view (currently only `Event::Diagnostic`,
        // see its doc comment there) -- nothing was printed, so there is
        // nothing to flush either.
        return;
    }
    // `watch` is meant to be piped (into a notifier, a log, `jq`, ...), and
    // stdout is block-buffered rather than line-buffered once it isn't a
    // terminal. Without an explicit flush here, a consumer reading the pipe
    // could stall waiting for output that is sitting in this process's
    // buffer -- exactly the kind of thing that turns "a stream" into "a
    // stream that only delivers on exit."
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
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
        Event::Diagnostic(d) => json!({"diagnostic": d}),
        Event::Ping => json!({"ping": true}),
        Event::HandshakeResponse { protocol, fw_major, fw_minor, mode } => json!({
            "handshake": {"protocol": protocol, "fw_major": fw_major, "fw_minor": fw_minor, "mode": mode}
        }),
        Event::Unknown { ty, data } => json!({"unknown": {"ty": ty, "data": data}}),
    }
}

/// Render one event as a human line, or `None` when it should not appear in
/// the human `watch` view at all.
fn event_human(event: &Event) -> Option<String> {
    Some(match event {
        Event::Mode(m) => format!("mode: {}", mode_str(*m)),
        Event::TargetTemperature(t) => format!("target temperature: {t}\u{b0}C"),
        Event::CurrentTemperature(t) => format!("current temperature: {t}\u{b0}C"),
        Event::Volume(v) => format!("volume: {v}"),
        Event::Error(b) => format!("error: {}", yes_no(*b)),
        Event::ChildLock(b) => format!("child lock: {}", yes_no(*b)),
        Event::Backlight(b) => format!("backlight: {}", yes_no(*b)),
        Event::AccessControl(b) => format!("access control: {}", yes_no(*b)),
        // Confirmed against the real device: its vendor app reports "MCU
        // 1.1.4" for the same three bytes this decodes.
        Event::Hardware([major, minor, patch]) => format!("hardware: {major}.{minor}.{patch}"),
        // Code 145: a 52-byte vendor diagnostic blob the device sends once
        // per session, right after the state burst. Decoded (see the design
        // spec's code-145 row): a 20-byte header followed by four 4-byte
        // ASCII tag / 4-byte little-endian value pairs -- firmware
        // telemetry meant for the vendor, not the kettle's state. We already
        // acknowledge and discard it rather than forward it (see
        // `commands::kettle::watch`); a session-opening dump of 52 numbers
        // is pure noise in the view whose whole reason to exist is being
        // less annoying than the vendor app, so it stops showing up here.
        // `--json` still carries it in full -- same treatment `volume`
        // already got.
        Event::Diagnostic(_) => return None,
        Event::Ping => "ping".to_string(),
        Event::HandshakeResponse { protocol, fw_major, fw_minor, .. } => {
            format!("handshake: protocol {protocol}, firmware {fw_major}.{fw_minor}")
        }
        Event::Unknown { ty, data } => format!("unknown event {ty}: {data:?}"),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
