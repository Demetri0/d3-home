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

use crate::config::Config;

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
        let value = json!({
            "current_temperature": state.current_temperature,
            "target_temperature": state.target_temperature,
            "mode": state.mode.map(mode_str),
            "water_present": state.water_present,
            "error": state.error,
            "child_lock": state.child_lock,
        });
        println!("{value}");
    } else {
        println!("mode:                {}", state.mode.map(mode_str).unwrap_or("unknown"));
        println!("current temperature: {}", fmt_temperature(state.current_temperature));
        println!("target temperature:  {}", fmt_temperature(state.target_temperature));
        println!("water present:       {}", fmt_flag(state.water_present));
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
    } else {
        println!("{}", event_human(event));
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
        Event::WaterPresent(b) => json!({"water_present": b}),
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

fn event_human(event: &Event) -> String {
    match event {
        Event::Mode(m) => format!("mode: {}", mode_str(*m)),
        Event::TargetTemperature(t) => format!("target temperature: {t}\u{b0}C"),
        Event::CurrentTemperature(t) => format!("current temperature: {t}\u{b0}C"),
        Event::WaterPresent(b) => format!("water present: {}", yes_no(*b)),
        Event::Error(b) => format!("error: {}", yes_no(*b)),
        Event::ChildLock(b) => format!("child lock: {}", yes_no(*b)),
        Event::Backlight(b) => format!("backlight: {}", yes_no(*b)),
        Event::AccessControl(b) => format!("access control: {}", yes_no(*b)),
        Event::Hardware(h) => format!("hardware: {h:?}"),
        Event::Diagnostic(d) => format!("diagnostic: {d:?}"),
        Event::Ping => "ping".to_string(),
        Event::HandshakeResponse { protocol, fw_major, fw_minor, .. } => {
            format!("handshake: protocol {protocol}, firmware {fw_major}.{fw_minor}")
        }
        Event::Unknown { ty, data } => format!("unknown event {ty}: {data:?}"),
    }
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

/// Report what `discover` found on the network.
pub fn print_found(found: &[Found], json: bool) {
    if json {
        let list: Vec<_> = found
            .iter()
            .map(|f| json!({"mac": f.mac, "address": f.address.to_string(), "port": f.port}))
            .collect();
        println!("{}", serde_json::Value::Array(list));
    } else if found.is_empty() {
        println!("no devices found");
    } else {
        for f in found {
            println!("{} at {}:{}", f.mac, f.address, f.port);
        }
    }
}
