use assert_cmd::Command;
use predicates::prelude::*;

mod support {
    use std::path::PathBuf;

    /// Write a config pointing at a simulator, with the endpoint pre-cached so
    /// the CLI never touches mDNS during tests.
    pub fn config_with(addr: std::net::SocketAddr, public_key: &str, token: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("d3home-cli-{}", std::process::id()));
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
fn status_reports_machine_readable_state() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    let out = Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--json", "kettle", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&out).expect("stdout is valid json");
    assert!(value.get("current_temperature").is_some());

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
        .stderr(predicate::str::contains("35"));

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
