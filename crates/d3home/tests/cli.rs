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
