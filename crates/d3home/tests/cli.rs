use assert_cmd::Command;
use predicates::prelude::*;

mod support {
    use std::path::PathBuf;

    /// Write a config pointing at a simulator, with the endpoint pre-cached so
    /// the CLI never touches mDNS during tests.
    ///
    /// The directory is named after both the test process's pid and the
    /// simulator's port: pid alone is not enough, because cargo's default
    /// test harness runs every `#[test]` in this file as a thread within
    /// one process, so all of them share a pid. Each `KettleSimulator`
    /// binds `127.0.0.1:0`, so the port is unique per test regardless.
    pub fn config_with(addr: std::net::SocketAddr, public_key: &str, token: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("d3home-cli-{}-{}", std::process::id(), addr.port()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[[devices]]
name = "kettle"
aliases = ["k"]
driver = "syncleo"
mac = "aabbccddeeff"
token = "{token}"

[devices.cached]
address = "{}"
port = {}
public_key = "{public_key}"
"#,
                addr.ip(),
                addr.port()
            ),
        )
        .unwrap();
        path
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const TOKEN: [u8; 16] = [0xA0; 16];

#[test]
fn starts_the_kettle_at_a_chosen_temperature() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "start", "80"])
        .assert()
        .success();

    assert_eq!(handle.state().target, 80);
    handle.shutdown();
}

#[test]
fn an_alias_works_exactly_like_the_device_name() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "k", "start"])
        .assert()
        .success();

    assert_eq!(handle.state().mode, syncleo::codec::command::PowerMode::On);
    handle.shutdown();
}

#[test]
fn off_turns_the_kettle_off() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "start"])
        .assert()
        .success();
    assert_eq!(handle.state().mode, syncleo::codec::command::PowerMode::On);

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "off"])
        .assert()
        .success();

    assert_eq!(handle.state().mode, syncleo::codec::command::PowerMode::Off);
    handle.shutdown();
}

#[test]
fn the_device_flag_is_equivalent_to_the_positional_device_word() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--device", "kettle", "start"])
        .assert()
        .success();

    assert_eq!(handle.state().mode, syncleo::codec::command::PowerMode::On);
    handle.shutdown();
}

#[test]
fn watch_streams_events_as_they_arrive() {
    use assert_cmd::prelude::*;
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    // `watch` runs until the connection drops or the process is killed, so
    // this can't use `assert_cmd`'s `.assert()`, which waits for the
    // process to exit on its own. Spawn the binary directly, read one line
    // off its stdout pipe (with a bounded wait, so a regression that makes
    // `watch` stop streaming fails the test instead of hanging it), then
    // kill it. That's enough to prove events are pushed out as they arrive
    // rather than only buffered until exit.
    let mut child = std::process::Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--json", "kettle", "watch"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("d3home watch should spawn");

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
    });

    let received = rx.recv_timeout(Duration::from_secs(5));

    child.kill().ok();
    let _ = child.wait();
    handle.shutdown();

    let line = received.expect("watch should print an event within 5 seconds");
    assert!(!line.trim().is_empty(), "watch printed an empty line");
    let _: serde_json::Value =
        serde_json::from_str(line.trim()).expect("each watch line is a JSON event");
}

/// Collects lines from `rx` until a short quiet gap follows at least one
/// line, or `overall` elapses with nothing at all. Mirrors the burst's own
/// "many messages close together, then done" shape rather than reading a
/// fixed count, so a slow CI box doesn't turn a timing hiccup into a false
/// failure.
fn collect_burst_lines(
    rx: &std::sync::mpsc::Receiver<String>,
    overall: std::time::Duration,
) -> Vec<String> {
    use std::time::{Duration, Instant};

    let mut lines = Vec::new();
    let deadline = Instant::now() + overall;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(300).min(remaining)) {
            Ok(line) => lines.push(line),
            Err(_) if !lines.is_empty() => break,
            Err(_) => continue,
        }
    }
    lines
}

/// Spawn `d3home watch` against `handle`'s simulator with the given extra
/// flags (e.g. `["--json"]` or `[]`), and return whatever lines it printed
/// during the post-handshake burst. Kills the child and shuts the simulator
/// down before returning.
fn watch_burst_lines(handle: &syncleo::simulator::KettleHandle, json_flag: &[&str]) -> Vec<String> {
    use assert_cmd::prelude::*;
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let config_str = config.to_str().unwrap().to_string();

    let mut args: Vec<&str> = vec!["--config", &config_str];
    args.extend_from_slice(json_flag);
    args.extend_from_slice(&["kettle", "watch"]);

    let mut child = std::process::Command::cargo_bin("d3home")
        .unwrap()
        .args(&args)
        .stdout(Stdio::piped())
        .spawn()
        .expect("d3home watch should spawn");

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let lines = collect_burst_lines(&rx, Duration::from_secs(5));

    child.kill().ok();
    let _ = child.wait();

    lines
}

#[test]
fn human_watch_output_shows_the_diagnostic_decoded() {
    // The simulator's burst includes a diagnostic event (code 145, mirroring
    // what the real device sends unprompted right after the handshake) shaped
    // exactly like a real capture: a 20-byte header then tag/value pairs
    // `udps=1 IDLE=2 Tmr=3 rtT=4` (see `report_state_burst`). The human
    // `watch` view must show it decoded, not hide it and not dump raw bytes.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let lines = watch_burst_lines(&handle, &[]);
    handle.shutdown();

    assert!(!lines.is_empty(), "watch printed no lines at all");
    let joined = lines.join("\n");
    assert!(
        joined.contains("diagnostic: udps=1 IDLE=2 Tmr=3 rtT=4"),
        "expected the decoded diagnostic line, got: {joined}"
    );
    assert!(
        joined.contains("hardware: 1.1.4"),
        "expected the hardware version rendered as 1.1.4, got: {joined}"
    );
}

#[test]
fn json_watch_output_still_carries_the_raw_diagnostic_event() {
    // `--json` is where completeness beats tidiness -- same precedent as
    // `volume` in `status`. The raw hardware array form ([1, 1, 4]) is also
    // pinned here, since only the human view gets the "1.1.4" rendering.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let lines = watch_burst_lines(&handle, &["--json"]);
    handle.shutdown();

    assert!(!lines.is_empty(), "watch printed no lines at all");
    let joined = lines.join("\n");
    assert!(joined.contains("diagnostic"), "json watch output lost the diagnostic event: {joined}");

    let diagnostic_line = lines
        .iter()
        .find(|l| l.contains("\"diagnostic\""))
        .unwrap_or_else(|| panic!("no diagnostic event in: {joined}"));
    let value: serde_json::Value = serde_json::from_str(diagnostic_line).expect("diagnostic line is json");
    assert!(value["diagnostic"].is_array(), "raw diagnostic bytes missing: {value}");
    assert_eq!(
        value["diagnostic_decoded"],
        serde_json::json!([
            {"tag": "udps", "value": 1},
            {"tag": "IDLE", "value": 2},
            {"tag": "Tmr", "value": 3},
            {"tag": "rtT", "value": 4},
        ])
    );

    let hardware_line = lines
        .iter()
        .find(|l| l.contains("hardware"))
        .unwrap_or_else(|| panic!("no hardware event in: {joined}"));
    let value: serde_json::Value = serde_json::from_str(hardware_line).expect("hardware line is json");
    assert_eq!(value["hardware"], serde_json::json!([1, 1, 4]));
}

#[test]
fn status_reports_machine_readable_state() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    let output = Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--json", "kettle", "status"])
        .assert()
        .success()
        .get_output()
        .clone();

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert!(value.get("current_temperature").is_some());

    // `assert_cmd` runs the child with its stderr piped, not a terminal --
    // exactly the case the progress spinner is required to stay silent in.
    // If a spinner ever drew here (or forgot to erase itself), this is
    // where it would show up.
    assert!(
        output.stderr.is_empty(),
        "a successful non-tty run must produce no stderr output at all, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    handle.shutdown();
}

#[test]
fn status_against_a_device_that_reports_no_state_exits_with_the_timeout_code() {
    // The post-handshake state burst is this project's own assumption
    // about how a Syncleo device behaves, not a documented part of the
    // protocol. Before this, a device (or a too-short window) that
    // produced zero events still printed six "unknown" lines and exited
    // 0 -- indistinguishable from a real reading where everything happens
    // to be off/false/absent. A script must be able to tell those apart.
    let handle = syncleo::simulator::KettleSimulator::spawn_silent(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "status"])
        .assert()
        .code(5);

    handle.shutdown();
}

#[test]
fn human_status_output_has_no_volume_row() {
    // Measured against the real device: 0 empty, 0 with a full litre of
    // water, 0 right after a full boil to 98°C. A permanently-zero row
    // under a name we can't justify is noise, so the human view stops
    // showing it (see `output::print_state`). The simulator's default
    // volume byte is 42 (non-zero, non-default-looking), which would show
    // up plainly if this regressed.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    let output = Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "status"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.to_lowercase().contains("volume"), "human status output still mentions volume: {stdout}");

    handle.shutdown();
}

#[test]
fn json_status_output_still_carries_volume() {
    // The field is still real data (`DeviceState::volume`, `Event::Volume`)
    // even though the human view no longer prints it -- `--json` is where
    // completeness matters more than tidiness, and where anyone
    // investigating the protocol will look.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    let output = Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--json", "kettle", "status"])
        .assert()
        .success()
        .get_output()
        .clone();

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(value["volume"], 42, "the simulator's default volume byte should still be reported");

    handle.shutdown();
}

#[test]
fn a_temperature_outside_the_supported_range_is_a_usage_error() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "start", "250"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("30"));

    handle.shutdown();
}

#[test]
fn a_device_nak_exits_with_the_device_error_code() {
    // The design allocates exit code 6 to a device NAK/error, distinct
    // from a timeout (5): the device answered clearly and rejected the
    // command, which a caller reading exit codes should be able to tell
    // apart from "the network is flaky."
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    handle.reject_commands();

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "start", "80"])
        .assert()
        .code(6);

    handle.shutdown();
}

#[test]
fn a_wrong_token_exits_with_its_own_code() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&[0xFF; 16]));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "status"])
        .assert()
        .code(4);

    handle.shutdown();
}

#[test]
fn an_unknown_device_name_is_a_usage_error() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "teapot", "status"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("teapot"));

    handle.shutdown();
}

#[test]
fn alias_add_and_remove_survive_a_reload() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let path = config.to_str().unwrap();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "alias", "add", "чай", "kettle"]).assert().success();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "чай", "start"]).assert().success();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "alias", "rm", "чай"]).assert().success();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "чай", "start"]).assert().code(2);

    handle.shutdown();
}

#[test]
fn refuses_to_create_an_alias_that_shadows_a_builtin() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "alias", "add", "discover", "kettle"])
        .assert()
        .code(2);

    handle.shutdown();
}

#[test]
// The brief's assertion form (`predicate::str::contains(&hex(&TOKEN))`) is
// kept verbatim; clippy would rather see the `&` dropped.
#[allow(clippy::needless_borrows_for_generic_args)]
fn devices_lists_what_is_configured_without_leaking_the_token() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "devices"])
        .assert()
        .success()
        .stdout(predicate::str::contains("kettle").and(predicate::str::contains("k")))
        .stdout(predicate::str::contains(&hex(&TOKEN)).not());

    handle.shutdown();
}
