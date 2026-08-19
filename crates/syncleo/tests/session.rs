use syncleo::codec::command::{Command, Event, PowerMode};
use syncleo::codec::crypt::{decrypt_frame, encrypt_frame};
use syncleo::codec::frame::{Frame, FrameType};
use syncleo::codec::keys::{SessionKeys, derive};
use syncleo::session::{Action, Input, LostReason, Millis, Session};

const OUR_PRIVATE: [u8; 32] = [7; 32];
const DEVICE_PRIVATE: [u8; 32] = [9; 32];
const TOKEN: [u8; 16] = [
    0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF,
];

/// Keys as the device sees them: same secret, roles swapped.
fn device_keys() -> SessionKeys {
    let k = derive(
        &OUR_PRIVATE,
        &syncleo::codec::keys::public_wire(&DEVICE_PRIVATE),
    );
    SessionKeys {
        inkey: k.outkey,
        outkey: k.inkey,
    }
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
    assert!(
        !sent(&actions).is_empty(),
        "device commands must be acknowledged"
    );
}

#[test]
fn acknowledges_incoming_commands_with_the_same_sequence() {
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );

    let actions = session.step(
        Input::Packet(from_device(0x42, FrameType::Cmd, &[20, 93, 0])),
        Millis(20),
    );

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Emit(Event::CurrentTemperature(93)))),
        "temperature must be emitted"
    );
    let ack = Frame::parse(&sent(&actions)[0]).unwrap();
    assert_eq!(ack.head.ty, FrameType::Ack);
    assert_eq!(ack.head.seq, 0x42, "ack carries the sequence it answers");
}

#[test]
fn survives_an_unknown_command() {
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );

    let actions = session.step(
        Input::Packet(from_device(1, FrameType::Cmd, &[77, 9])),
        Millis(20),
    );

    assert!(
        session.is_connected(),
        "an unknown command must not drop the session"
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Emit(Event::Unknown { ty: 77, .. })))
    );
}

#[test]
fn resends_an_unacknowledged_command_then_gives_up() {
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );

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
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );

    session.request(Command::Mode(PowerMode::On), Millis(100));

    let mut now = 100u64;
    let mut actions = Vec::new();
    for _ in 0..5 {
        now += 1000;
        actions = session.step(Input::Tick, Millis(now));
    }

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Lost(LostReason::Unacknowledged))),
        "five unacknowledged attempts must declare the connection lost"
    );
    assert!(!session.is_connected());
}

#[test]
fn pings_every_three_seconds() {
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(0),
    );

    assert!(sent(&session.step(Input::Tick, Millis(2_999))).is_empty());

    let actions = session.step(Input::Tick, Millis(3_000));
    let frame = Frame::parse(&sent(&actions)[0]).unwrap();
    let body = decrypt_frame(&device_keys(), &frame).unwrap();
    assert_eq!(body, vec![255], "ping command");
}

#[test]
fn declares_the_connection_lost_after_fifteen_silent_seconds() {
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(0),
    );

    assert!(
        session
            .step(Input::Tick, Millis(14_999))
            .iter()
            .all(|a| !matches!(a, Action::Lost(_)))
    );

    let actions = session.step(Input::Tick, Millis(15_000));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Lost(LostReason::Silence)))
    );
    assert!(!session.is_connected());
}

#[test]
fn a_nak_matching_the_pending_frame_surfaces_as_nacked_not_a_silent_drop() {
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );
    assert!(session.is_connected());

    let pending = session.request(Command::TargetTemperature(200), Millis(100));
    let seq = Frame::parse(&sent(&pending)[0]).unwrap().head.seq;

    let actions = session.step(
        Input::Packet(from_device(seq, FrameType::Nak, &[])),
        Millis(110),
    );

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Nacked(s) if *s == seq)),
        "a Nak matching the pending frame must surface as Nacked, got {actions:?}"
    );
    assert!(
        session.is_connected(),
        "a device Nak rejects one command, it does not kill the session"
    );
}

#[test]
fn a_nak_that_does_not_match_the_pending_sequence_is_ignored() {
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );

    let pending = session.request(Command::TargetTemperature(80), Millis(100));
    let seq = Frame::parse(&sent(&pending)[0]).unwrap().head.seq;

    // A Nak for some other sequence entirely -- stale, or for a frame this
    // session never sent -- must not be mistaken for an answer to the
    // frame actually pending.
    let actions = session.step(
        Input::Packet(from_device(seq.wrapping_add(1), FrameType::Nak, &[])),
        Millis(110),
    );

    assert!(!actions.iter().any(|a| matches!(a, Action::Nacked(_))));
    assert!(session.is_connected());
}

#[test]
fn treats_a_rejected_handshake_as_a_bad_token() {
    let (mut session, _) = start();

    let actions = session.step(
        Input::Packet(from_device(0, FrameType::Nak, &[])),
        Millis(10),
    );

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Lost(LostReason::HandshakeRejected)))
    );
}

#[test]
fn a_pre_connection_ack_does_not_cancel_the_handshakes_own_resend() {
    // The spec allows the device to answer the handshake with an Ack
    // before its handshake response, in either order. If that Ack cleared
    // the pending handshake frame (as it used to), a handshake response
    // lost after it would leave nothing pending to resend, and the
    // session would sit idle rather than retry every second the way it
    // does for every other pending frame.
    let (mut session, initial) = start();
    let handshake_bytes = sent(&initial)[0].clone();
    let seq = Frame::parse(&handshake_bytes).unwrap().head.seq;

    let ack_actions = session.step(
        Input::Packet(from_device(seq, FrameType::Ack, &[])),
        Millis(10),
    );
    assert!(
        !ack_actions.iter().any(|a| matches!(a, Action::Acked(_))),
        "a pre-connection ack must not surface as Acked, got {ack_actions:?}"
    );
    assert!(!session.is_connected());

    // Past the 1s resend interval with no handshake response ever
    // arriving, the handshake frame must still be resent -- proving it
    // is still tracked as pending, not silently dropped by the Ack above.
    let resend_actions = session.step(Input::Tick, Millis(1_010));
    assert_eq!(
        sent(&resend_actions),
        vec![handshake_bytes],
        "the handshake must still resend after a pre-connection ack"
    );
    assert!(!session.is_connected());
}

#[test]
fn a_duplicate_ack_or_one_for_an_unsent_sequence_produces_no_acked() {
    // Finding 13: the Nak equivalents of this are pinned above
    // (`a_nak_matching_the_pending_frame_...`,
    // `a_nak_that_does_not_match_the_pending_sequence_is_ignored`); the Ack
    // arm had no test at all. The guard here (`pending.seq ==
    // frame.head.seq`) is one `&&` away from being deleted, and if it
    // were, `Client::send` would report success on any ack-shaped packet
    // -- turning "the command was never delivered" into a false exit 0.
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );

    let pending = session.request(Command::TargetTemperature(80), Millis(100));
    let seq = Frame::parse(&sent(&pending)[0]).unwrap().head.seq;

    let first = session.step(
        Input::Packet(from_device(seq, FrameType::Ack, &[])),
        Millis(110),
    );
    assert_eq!(
        first
            .iter()
            .filter(|a| matches!(a, Action::Acked(_)))
            .count(),
        1,
        "the genuine ack must surface exactly once: {first:?}"
    );

    // The identical Ack again: the slot is already cleared, nothing was
    // sent a second time, so this must not surface a second `Acked`.
    let duplicate = session.step(
        Input::Packet(from_device(seq, FrameType::Ack, &[])),
        Millis(120),
    );
    assert!(
        !duplicate.iter().any(|a| matches!(a, Action::Acked(_))),
        "a duplicate ack must not surface again: {duplicate:?}"
    );

    // An ack for a sequence this session never used at all.
    let unsent = session.step(
        Input::Packet(from_device(seq.wrapping_add(7), FrameType::Ack, &[])),
        Millis(130),
    );
    assert!(
        !unsent.iter().any(|a| matches!(a, Action::Acked(_))),
        "an ack for a sequence never sent must be ignored: {unsent:?}"
    );
}

#[test]
fn nothing_happens_to_a_session_once_it_has_been_declared_lost() {
    // Finding 14: `dead` short-circuits both `request` and `step`, so a
    // late handshake response, a duplicate Nak, and a burst of ticks all
    // produce nothing -- including no second `Lost`, which a caller like
    // `Client::watch` would otherwise turn into a second, spurious
    // reconnect cycle for a session that already reconnected. Correct
    // today; this pins it.
    let (mut session, _) = start();
    session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(0),
    );

    let lost = session.step(Input::Tick, Millis(15_000));
    assert!(
        lost.iter()
            .any(|a| matches!(a, Action::Lost(LostReason::Silence)))
    );
    assert!(!session.is_connected());

    let late_handshake_response = session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(15_010),
    );
    assert!(
        late_handshake_response.is_empty(),
        "got {late_handshake_response:?}"
    );

    let late_nak = session.step(
        Input::Packet(from_device(0, FrameType::Nak, &[])),
        Millis(15_020),
    );
    assert!(late_nak.is_empty(), "got {late_nak:?}");

    for i in 0..3u64 {
        let tick = session.step(Input::Tick, Millis(15_030 + i * 1_000));
        assert!(
            tick.is_empty(),
            "a tick after Lost must produce nothing, got {tick:?}"
        );
    }

    // request() must also stay quiet: nothing this session could still
    // send matters once it's declared itself dead.
    let requested = session.request(Command::Mode(PowerMode::On), Millis(20_000));
    assert!(requested.is_empty(), "got {requested:?}");
}

#[test]
fn a_duplicate_handshake_response_after_connecting_does_not_disconnect_or_reconnect() {
    // Finding 15: a retransmitted handshake response arriving after the
    // session is already connected takes the `else` branch of the
    // HandshakeResponse check and is acked and re-emitted as a plain
    // event -- it does not flip `connected` back off, and does not
    // produce a second `Action::Connected`. Harmless today (the state
    // burst that follows is idempotent) but worth pinning: it's the one
    // output shape that could be mistaken for a reconnect without being
    // one.
    let (mut session, _) = start();
    let first = session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(10),
    );
    assert_eq!(
        first
            .iter()
            .filter(|a| matches!(a, Action::Connected))
            .count(),
        1
    );
    assert!(session.is_connected());

    let second = session.step(
        Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])),
        Millis(20),
    );
    assert_eq!(
        second
            .iter()
            .filter(|a| matches!(a, Action::Connected))
            .count(),
        0,
        "Connected must fire exactly once, got {second:?}"
    );
    assert!(
        session.is_connected(),
        "a duplicate handshake response must not disconnect the session"
    );
    assert!(
        second
            .iter()
            .any(|a| matches!(a, Action::Emit(Event::HandshakeResponse { .. }))),
        "the event itself is still emitted -- pinning that current, harmless behaviour: {second:?}"
    );
}

#[test]
fn ignores_a_packet_it_cannot_decrypt() {
    let (mut session, _) = start();

    let actions = session.step(
        Input::Packet(vec![0x00, 0x01, 0x04, 0x00, 1, 2, 3, 4]),
        Millis(10),
    );

    assert!(
        actions.is_empty(),
        "garbage on the wire is dropped silently"
    );
    assert!(!session.is_connected());
}
