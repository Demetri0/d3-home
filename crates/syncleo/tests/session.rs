use syncleo::codec::command::{Command, Event, PowerMode};
use syncleo::codec::crypt::{decrypt_frame, encrypt_frame};
use syncleo::codec::frame::{Frame, FrameType};
use syncleo::codec::keys::{SessionKeys, derive};
use syncleo::session::{Action, Input, LostReason, Millis, Session};

const OUR_PRIVATE: [u8; 32] = [7; 32];
const DEVICE_PRIVATE: [u8; 32] = [9; 32];
const TOKEN: [u8; 16] = [0xA0; 16];

/// Keys as the device sees them: same secret, roles swapped.
fn device_keys() -> SessionKeys {
    let k = derive(&OUR_PRIVATE, &syncleo::codec::keys::public_wire(&DEVICE_PRIVATE));
    SessionKeys { inkey: k.outkey, outkey: k.inkey }
}

fn start() -> (Session, Vec<Action>) {
    Session::new(
        OUR_PRIVATE,
        syncleo::codec::keys::public_wire(&DEVICE_PRIVATE),
        TOKEN,
        Millis(0),
    )
}

fn sent(actions: &[Action]) -> Vec<Vec<u8>> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send(bytes) => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

/// Build a packet as the device would send it.
fn from_device(seq: u8, ty: FrameType, body: &[u8]) -> Vec<u8> {
    encrypt_frame(&device_keys(), seq, ty, body).to_bytes()
}

#[test]
fn opens_with_a_handshake() {
    let (_session, actions) = start();
    let packets = sent(&actions);

    assert_eq!(packets.len(), 1, "exactly one packet on open");
    let frame = Frame::parse(&packets[0]).unwrap();
    assert_eq!(frame.head.ty, FrameType::Cmd);
    assert_eq!(frame.payload[0], 0x00, "handshake command");
    assert_eq!(frame.payload.len(), 1 + 32 + 16);
}

#[test]
fn reports_connected_once_the_device_answers() {
    let (mut session, _) = start();

    let response = from_device(0, FrameType::Cmd, &[0, 0x02, 0x00, 1, 4, 0]);
    let actions = session.step(Input::Packet(response), Millis(50));

    assert!(session.is_connected());
    assert!(actions.iter().any(|a| matches!(a, Action::Connected)));
    assert!(!sent(&actions).is_empty(), "device commands must be acknowledged");
}

#[test]
fn acknowledges_incoming_commands_with_the_same_sequence() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(10));

    let actions = session.step(
        Input::Packet(from_device(0x42, FrameType::Cmd, &[20, 93, 0])),
        Millis(20),
    );

    assert!(
        actions.iter().any(|a| matches!(a, Action::Emit(Event::CurrentTemperature(93)))),
        "temperature must be emitted"
    );
    let ack = Frame::parse(&sent(&actions)[0]).unwrap();
    assert_eq!(ack.head.ty, FrameType::Ack);
    assert_eq!(ack.head.seq, 0x42, "ack carries the sequence it answers");
}

#[test]
fn survives_an_unknown_command() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(10));

    let actions = session.step(Input::Packet(from_device(1, FrameType::Cmd, &[77, 9])), Millis(20));

    assert!(session.is_connected(), "an unknown command must not drop the session");
    assert!(actions.iter().any(|a| matches!(a, Action::Emit(Event::Unknown { ty: 77, .. }))));
}

#[test]
fn resends_an_unacknowledged_command_then_gives_up() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(10));

    let first = session.request(Command::Mode(PowerMode::On), Millis(100));
    assert_eq!(sent(&first).len(), 1);

    let mut resends = 0;
    let mut now = 100u64;
    for _ in 0..8 {
        now += 1000;
        resends += sent(&session.step(Input::Tick, Millis(now))).len();
    }

    assert_eq!(resends, 4, "five attempts total means four resends");
}

#[test]
fn declares_the_connection_lost_when_a_command_exhausts_its_attempts() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(10));

    session.request(Command::Mode(PowerMode::On), Millis(100));

    let mut now = 100u64;
    let mut actions = Vec::new();
    for _ in 0..5 {
        now += 1000;
        actions = session.step(Input::Tick, Millis(now));
    }

    assert!(
        actions.iter().any(|a| matches!(a, Action::Lost(LostReason::Unacknowledged))),
        "five unacknowledged attempts must declare the connection lost"
    );
    assert!(!session.is_connected());
}

#[test]
fn pings_every_three_seconds() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(0));

    assert!(sent(&session.step(Input::Tick, Millis(2_999))).is_empty());

    let actions = session.step(Input::Tick, Millis(3_000));
    let frame = Frame::parse(&sent(&actions)[0]).unwrap();
    let body = decrypt_frame(&device_keys(), &frame).unwrap();
    assert_eq!(body, vec![255], "ping command");
}

#[test]
fn declares_the_connection_lost_after_fifteen_silent_seconds() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(0));

    assert!(session.step(Input::Tick, Millis(14_999)).iter().all(|a| !matches!(a, Action::Lost(_))));

    let actions = session.step(Input::Tick, Millis(15_000));
    assert!(actions.iter().any(|a| matches!(a, Action::Lost(LostReason::Silence))));
    assert!(!session.is_connected());
}

#[test]
fn treats_a_rejected_handshake_as_a_bad_token() {
    let (mut session, _) = start();

    let actions = session.step(Input::Packet(from_device(0, FrameType::Nak, &[])), Millis(10));

    assert!(actions.iter().any(|a| matches!(a, Action::Lost(LostReason::HandshakeRejected))));
}

#[test]
fn ignores_a_packet_it_cannot_decrypt() {
    let (mut session, _) = start();

    let actions = session.step(Input::Packet(vec![0x00, 0x01, 0x04, 0x00, 1, 2, 3, 4]), Millis(10));

    assert!(actions.is_empty(), "garbage on the wire is dropped silently");
    assert!(!session.is_connected());
}
