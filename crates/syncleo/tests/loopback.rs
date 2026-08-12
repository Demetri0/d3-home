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
