use assert_cmd::Command;
use predicates::prelude::*;

mod support {
    use std::os::unix::fs::PermissionsExt;
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
        // Never put a real device's MAC in a fixture. These tests run the actual
        // binary, and if the simulator's cached endpoint fails to answer, the CLI
        // falls back to mDNS discovery by design -- which on a home network would
        // find the real kettle and send it whatever the test was sending, `start`
        // included. A MAC that matches nothing keeps that path harmless.
        config_with_mac(addr, public_key, token, "aabbccddeeff")
    }

    /// Like [`config_with`], but lets a test pick its own MAC.
    ///
    /// Every other test in this file always has its cached endpoint answer
    /// successfully, so mDNS is never actually touched and the fixture MAC
    /// above (which is this project's real reference kettle's MAC) is
    /// inert. A test that deliberately makes the cached endpoint fail --
    /// to exercise `connect`'s fallback to discovery, or a reconnect loop
    /// that goes through it -- is different: on the network this suite was
    /// developed against, that MAC is *actually discoverable*, and a real
    /// mDNS scan would find the real device. Nothing here would go on to
    /// harm it (this project never sends a command a test doesn't mean to),
    /// but the test's outcome must not depend on whether a real kettle
    /// happens to be reachable. A MAC no real device could ever advertise
    /// removes that dependency instead of merely making it unlikely.
    pub fn config_with_mac(addr: std::net::SocketAddr, public_key: &str, token: &str, mac: &str) -> PathBuf {
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
mac = "{mac}"
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
        // A real config file is always 0600 (see `Config::save`). Match
        // that here too: `std::fs::write` leaves the file at whatever the
        // process's umask allows (typically group/other readable), which
        // would otherwise spuriously trip the world-readable-config
        // warning `Config::load` prints (finding 18) in every test in this
        // file, including the ones that assert a clean stderr.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
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
fn a_partial_start_failure_leaves_the_kettle_off_not_heating() {
    // Finding 1: `start N` sends two commands (TargetTemperature then
    // Mode(Custom), see `commands::kettle::send_custom_target`) with no
    // atomicity between them. If the second one's ack never arrives, the
    // CLI must report failure without leaving the kettle heating to a
    // stored target -- the worst outcome this program can produce.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    // Ack the first command (the target) normally, then go silent --
    // simulating the second command's (Mode(Custom)) ack, and every
    // resend of it, being lost.
    handle.ignore_commands_after(1);

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "start", "60"])
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .code(5);

    let state = handle.state();
    assert_eq!(state.target, 60, "the target must still have been set -- that command was acked");
    assert_ne!(
        state.mode,
        syncleo::codec::command::PowerMode::Custom,
        "a failed start must not leave the kettle in Custom mode heating to a stale target"
    );

    handle.shutdown();
}

#[test]
fn watch_exits_cleanly_instead_of_panicking_when_its_output_pipe_is_closed() {
    // Finding 4: `println!` (what `output::print_event` used to use)
    // panics when the write fails -- and Rust ignores SIGPIPE, so a
    // downstream reader going away (`| head -1`, a killed notifier) turns
    // into a broken-pipe write failure, not a signal that kills the
    // process outright. `watch` is explicitly meant to be piped, so this
    // must exit within the documented 0-6 contract, never 101.
    use assert_cmd::prelude::*;
    use std::process::Stdio;
    use std::time::Duration;

    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    // Delay the post-handshake burst so it is guaranteed to still be
    // unsent when the read end below is closed -- every line the child
    // then tries to print hits an already-broken pipe, rather than racing
    // whether the (otherwise near-instant) burst beat this test to it.
    handle.delay_state_burst(Duration::from_millis(500));
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    let mut child = std::process::Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--json", "kettle", "watch"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("d3home watch should spawn");

    // Close our end of stdout -- the read end -- immediately: nothing has
    // been written yet (the burst is delayed), so the pipe has zero
    // readers by the time the child's first `print_event` call fires.
    drop(child.stdout.take().expect("stdout was piped"));

    let output = child
        .wait_with_output()
        .expect("the process must exit on its own, not hang, once its output pipe is closed");

    assert!(
        matches!(output.status.code(), Some(0) | Some(5)),
        "expected an exit code within the documented 0-6 contract, got {:?} (stderr: {})",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "watch must not panic on a broken output pipe: {stderr}");

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
fn status_against_an_absent_device_fails_rather_than_waiting() {
    // `watch` is the only command that waits and retries when a device
    // has gone quiet (see `watch_reconnects_after_the_device_goes_silent_and_returns`
    // below); a one-shot command that waited indefinitely for an absent
    // kettle would be worse, not better. Shutting the simulator down
    // immediately leaves a real, bound UDP port with nothing listening --
    // the same symptom `watch`'s reconnect exists for -- and `status` must
    // still just fail, the same way it always has: the cached address
    // times out, `connect` falls back to a real (here: empty) mDNS scan
    // exactly as it does for any command, and that comes back
    // `NotFound` -- not `watch`-style waiting.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config =
        support::config_with_mac(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN), "d3d3d3d3d3d3");
    handle.shutdown();

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "status"])
        .timeout(std::time::Duration::from_secs(20))
        .assert()
        .code(3);
}

#[test]
fn watch_reconnects_after_the_device_goes_silent_and_returns() {
    // The central behaviour this feature exists for: lifting the kettle
    // off its base cuts its power outright, `watch` must not exit when
    // that happens, and events from the session that follows the kettle
    // coming back must still reach the output.
    use assert_cmd::prelude::*;
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    // A MAC that cannot collide with a real, discoverable device (see
    // `support::config_with_mac`): the timing below is chosen so the
    // reconnect always succeeds via the cached endpoint and mDNS is never
    // actually reached, but if that assumption is ever wrong on a slower
    // or more loaded machine, this must fail as "not found" rather than
    // risk finding and handshaking with a real kettle.
    let config = support::config_with_mac(
        handle.addr,
        &hex(&handle.public_wire),
        &hex(&TOKEN),
        "d3d3d3d3d3d3",
    );

    let mut child = std::process::Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--json", "kettle", "watch"])
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

    // The first session's burst -- proves the initial connection worked.
    let first = collect_burst_lines(&rx, Duration::from_secs(5));
    assert!(!first.is_empty(), "no events from the first session");

    // Simulate the kettle being lifted off its base: it stops answering
    // *anything*, including a fresh handshake, for long enough that the
    // client's own timers are guaranteed to have declared the session
    // lost before it "returns to its base". syncleo::session's fixed ping
    // (3s) and resend (1s x 5 attempts) constants put that at ~8s after
    // connecting; 9.5s clears that with margin while still landing well
    // inside the ~3s connect timeout of the reconnect attempt that starts
    // once the loss is detected, so the cached endpoint always answers
    // again before that attempt gives up -- this test never needs (and
    // must never need) a real mDNS fallback to pass.
    handle.vanish_for(Duration::from_millis(9_500));

    // Generous overall deadline: loss detection (~8s) plus at least one
    // reconnect attempt has to fit comfortably inside it.
    let after = collect_burst_lines(&rx, Duration::from_secs(20));

    child.kill().ok();
    let _ = child.wait();
    handle.shutdown();

    assert!(!after.is_empty(), "no events reached the output after the device came back");
    let joined = after.join("\n");
    assert!(
        joined.contains(r#""reconnected":true"#),
        "no reconnect boundary marker in the output: {joined}"
    );
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
fn a_world_readable_config_prints_a_warning_but_still_works() {
    // Finding 18: `save` is careful about 0600 from creation; `load`
    // checked nothing at all. A config restored from a backup, or copied
    // with plain `cp` (which doesn't preserve mode), could sit readable by
    // every other local user while holding a device token, silently.
    use std::os::unix::fs::PermissionsExt;

    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();

    let output = Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "status"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("chmod"),
        "expected a permission warning naming the fix on stderr, got: {stderr}"
    );

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
fn refuses_to_create_an_alias_that_looks_like_a_flag_or_is_empty() {
    // Finding 12: both of these would otherwise sit in the config forever,
    // permanently unusable -- a flag-shaped alias because it is read as an
    // option wherever it appears, an empty string because it can never be
    // typed as a positional word. `--` is what gets a literal `--json`
    // past option parsing at all, so that is how the validation is reached.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let path = config.to_str().unwrap();

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", path, "alias", "add", "--", "--json", "kettle"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("start with"));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", path, "alias", "add", "", "kettle"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("non-empty"));

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

#[test]
fn a_global_flag_after_the_action_is_honoured_end_to_end() {
    // The motivating bug: `--json` in trailing position was silently
    // dropped, so a script asking for JSON quietly got human text instead.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let path = config.to_str().unwrap();

    for args in [
        vec!["--config", path, "--json", "kettle", "status"],
        vec!["--config", path, "kettle", "status", "--json"],
        vec!["kettle", "status", "--json", "--config", path],
    ] {
        let out = Command::cargo_bin("d3home")
            .unwrap()
            .args(&args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<serde_json::Value>(&out)
            .unwrap_or_else(|_| panic!("not json for {args:?}: {}", String::from_utf8_lossy(&out)));
    }

    handle.shutdown();
}

#[test]
fn help_lists_every_command_that_exists() {
    // Help drifting out of step with the program is silent, so pin it.
    let out = Command::cargo_bin("d3home")
        .unwrap()
        .args(["help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(out).unwrap();

    for word in [
        "status", "start", "set", "off", "watch", "discover", "devices", "alias", "--json",
        "--device", "--config",
    ] {
        assert!(help.contains(word), "help never mentions {word}:\n{help}");
    }
}

#[test]
fn set_changes_the_target_without_starting_the_kettle() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let before = handle.state().mode;

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "set", "60"])
        .assert()
        .success()
        .stdout(predicate::str::contains("60"));

    let after = handle.state();
    assert_eq!(after.target, 60, "target was not applied");
    assert_eq!(after.mode, before, "set must not touch the mode");

    handle.shutdown();
}

#[test]
fn set_rejects_a_temperature_outside_the_supported_range() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let path = config.to_str().unwrap();

    for bad in ["25", "105", "boiling"] {
        Command::cargo_bin("d3home")
            .unwrap()
            .args(["--config", path, "kettle", "set", bad])
            .assert()
            .code(2);
    }

    // A bare `set` has nothing to set and must say so rather than defaulting.
    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", path, "kettle", "set"])
        .assert()
        .code(2);

    handle.shutdown();
}

#[test]
fn start_reports_what_the_kettle_agreed_to_do() {
    // A silent success left the user guessing whether anything happened.
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let path = config.to_str().unwrap();

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", path, "kettle", "start", "70"])
        .assert()
        .success()
        .stdout(predicate::str::contains("70"));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", path, "kettle", "start"])
        .assert()
        .success()
        .stdout(predicate::str::contains("100"));

    handle.shutdown();
}
