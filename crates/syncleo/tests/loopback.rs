use std::time::Duration;
use syncleo::client::Client;
use syncleo::codec::command::{Command, PowerMode};
use syncleo::simulator::KettleSimulator;
use syncleo::transport::UdpTransport;

const OUR_PRIVATE: [u8; 32] = [11; 32];
const TOKEN: [u8; 16] = [0xA0; 16];

fn connect(handle: &syncleo::simulator::KettleHandle, token: [u8; 16]) -> Result<Client, syncleo::Error> {
    let transport = UdpTransport::connect(handle.addr).unwrap();
    Client::connect(
        Box::new(transport),
        OUR_PRIVATE,
        handle.public_wire,
        token,
        Duration::from_secs(3),
    )
}

#[test]
fn drives_the_simulated_kettle_over_real_udp() {
    let handle = KettleSimulator::spawn(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).expect("handshake succeeds");

    client.send(Command::Mode(PowerMode::Custom)).unwrap();
    client.send(Command::TargetTemperature(80)).unwrap();

    let state = handle.state();
    assert_eq!(state.mode, PowerMode::Custom);
    assert_eq!(state.target, 80);

    handle.shutdown();
}

#[test]
fn reads_state_back_from_the_device() {
    let handle = KettleSimulator::spawn(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).unwrap();

    let state = client.collect_state(Duration::from_millis(500)).unwrap();

    assert!(state.current_temperature.is_some(), "device reports its temperature");
    assert!(state.water_present.is_some(), "device reports whether it holds water");

    handle.shutdown();
}

#[test]
fn the_devices_own_acks_are_decrypted_and_validated_by_the_simulator() {
    // The simulator used to return early on any non-Cmd frame from an
    // established peer without even decrypting it, so every Ack the
    // client ever sent -- acknowledging the handshake response and each
    // state-burst report -- was generated and then silently discarded.
    // This is the only test that proves those acks are actually well
    // formed rather than merely produced.
    let handle = KettleSimulator::spawn(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).expect("handshake succeeds");

    // The handshake response and the four state-burst reports are each a
    // Cmd frame the client must ack; collect_state's window is long enough
    // for all of them to round-trip.
    client.collect_state(Duration::from_millis(500)).unwrap();

    assert!(
        handle.valid_acks() >= 1,
        "the simulator must have decrypted and validated at least one Ack from the client"
    );

    handle.shutdown();
}

#[test]
fn a_device_that_sends_no_state_at_all_is_reported_as_an_error_not_a_hollow_success() {
    // The post-handshake state burst is this project's own assumption
    // about how a Syncleo device behaves, not a documented part of the
    // protocol -- there is no "query state" command. A real device that
    // doesn't send one must not look identical to "everything is really
    // false/zero/off": both would otherwise print as six `unknown` lines
    // with exit code 0, and a script has no way to tell them apart.
    let handle = KettleSimulator::spawn_silent(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).expect("handshake succeeds even with no state burst");

    let err = client.collect_state(Duration::from_millis(200)).expect_err("no events arrived at all");
    assert!(matches!(err, syncleo::Error::NoState), "got {err:?}");

    handle.shutdown();
}

#[test]
fn a_wrong_token_is_rejected() {
    let handle = KettleSimulator::spawn(TOKEN).unwrap();

    let err = connect(&handle, [0xFF; 16]).expect_err("the device must refuse a bad token");

    assert!(
        matches!(err, syncleo::Error::HandshakeRejected),
        "a bad token must be distinguishable from a timeout, got {err:?}"
    );

    handle.shutdown();
}

#[test]
fn a_command_the_device_never_acknowledges_is_reported_as_an_error() {
    let handle = KettleSimulator::spawn(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).expect("handshake succeeds");

    handle.ignore_commands();

    let err = client.send(Command::TargetTemperature(80)).expect_err("must not report success");
    assert!(
        matches!(err, syncleo::Error::Timeout),
        "an exhausted resend must surface as the session's own give-up, got {err:?}"
    );

    handle.shutdown();
}

#[test]
fn a_command_the_device_naks_is_reported_distinctly_from_a_timeout() {
    // Scenario from hardware testing: probing the real temperature bounds,
    // the kettle NAKs an out-of-range `start`. Before this, session.rs
    // only ever acted on a Nak while still unconnected (a rejected
    // handshake); a post-handshake Nak was silently dropped, the pending
    // frame kept resending until it exhausted its attempts, and the CLI
    // reported a plain timeout -- an operator reading a network fault
    // where the device had actually given a clear answer.
    let handle = KettleSimulator::spawn(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).expect("handshake succeeds");

    handle.reject_commands();

    let err = client.send(Command::TargetTemperature(80)).expect_err("must not report success");
    assert!(
        matches!(err, syncleo::Error::DeviceNak),
        "a device Nak must be distinguishable from a timeout, got {err:?}"
    );

    handle.shutdown();
}

#[test]
fn an_unreachable_device_times_out() {
    // Port 1 on loopback: nothing listens there.
    let transport = UdpTransport::connect("127.0.0.1:1".parse().unwrap()).unwrap();
    let err = Client::connect(
        Box::new(transport),
        OUR_PRIVATE,
        [0; 32],
        TOKEN,
        Duration::from_millis(300),
    )
    .expect_err("must not hang");

    assert!(matches!(err, syncleo::Error::Timeout), "got {err:?}");
}
