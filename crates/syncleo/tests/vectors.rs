use syncleo::codec::crypt::{decrypt_frame, encrypt_frame};
use syncleo::codec::frame::{Frame, FrameType};
use syncleo::codec::keys::SessionKeys;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn keys() -> SessionKeys {
    SessionKeys {
        inkey: unhex("000102030405060708090a0b0c0d0e0f").try_into().unwrap(),
        outkey: unhex("101112131415161718191a1b1c1d1e1f").try_into().unwrap(),
    }
}

// Every expected value below was produced by the Python reference
// implementation (gch1p/polaris_pwk_1725cgld) with the keys above.

#[test]
fn encrypts_mode_commands_byte_for_byte() {
    // command 1 (mode), payload 1 = on
    let frame = encrypt_frame(&keys(), 0x35, FrameType::Cmd, &[0x01, 0x01]);
    assert_eq!(frame.to_bytes(), unhex("35011000c6ed01166ba072d2baa600574cc0bcc8"));

    // payload 0 = off, at sequence 0 (no rotation at all)
    let frame = encrypt_frame(&keys(), 0x00, FrameType::Cmd, &[0x01, 0x00]);
    assert_eq!(frame.to_bytes(), unhex("000110004c4f50e332b954b25aabec5d9b69b46a"));

    // payload 3 = custom, at sequence 0xFE (both nibbles rotate)
    let frame = encrypt_frame(&keys(), 0xFE, FrameType::Cmd, &[0x01, 0x03]);
    assert_eq!(frame.to_bytes(), unhex("fe011000f184650ca412db228c17f105b574f0bc"));
}

#[test]
fn encrypts_a_target_temperature_command() {
    // command 2 (target temperature), 80 whole degrees, 0 hundredths
    let frame = encrypt_frame(&keys(), 0xC2, FrameType::Cmd, &[0x02, 80, 0]);
    assert_eq!(frame.to_bytes(), unhex("c20110003c1e25f8bcd62e14e9cad115d182ceae"));
}

#[test]
fn decrypts_a_frame_sent_by_the_device() {
    // The device encrypts with the key roles swapped, so this is what we receive.
    let raw = unhex("35011000ac0017868ee3c024d65b3fb78602638a");
    let frame = Frame::parse(&raw).unwrap();

    let body = decrypt_frame(&keys(), &frame).expect("device frame decrypts");

    assert_eq!(body, vec![0x01, 0x01], "mode command, value on");
}

#[test]
fn round_trips_every_body_length_through_a_padding_boundary() {
    // 14 bytes of body plus the sequence byte exactly fills one AES block,
    // which is where PKCS7 bugs hide.
    let device_view = SessionKeys { inkey: keys().outkey, outkey: keys().inkey };

    for len in 0..40usize {
        let body: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let frame = encrypt_frame(&keys(), 0x7B, FrameType::Cmd, &body);
        let back = decrypt_frame(&device_view, &frame).expect("round trip");
        assert_eq!(back, body, "body of length {len} survived the round trip");
    }
}

#[test]
fn rejects_a_frame_whose_sequence_was_tampered_with() {
    let raw = unhex("35011000ac0017868ee3c024d65b3fb78602638a");
    let mut frame = Frame::parse(&raw).unwrap();
    frame.head.seq = 0x36;

    assert!(decrypt_frame(&keys(), &frame).is_err(), "sequence mismatch must be caught");
}

proptest::proptest! {
    #[test]
    fn any_body_at_any_sequence_survives_a_round_trip(
        seq in proptest::prelude::any::<u8>(),
        body in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
    ) {
        let device_view = SessionKeys { inkey: keys().outkey, outkey: keys().inkey };
        let frame = encrypt_frame(&keys(), seq, FrameType::Cmd, &body);

        let parsed = Frame::parse(&frame.to_bytes()).unwrap();
        let back = decrypt_frame(&device_view, &parsed).unwrap();

        proptest::prop_assert_eq!(back, body);
    }
}

// Every expected value below was captured from the Python reference
// implementation (gch1p/polaris_pwk_1725cgld) with the keys above, the same
// way the Cmd vectors at the top of this file were. Unlike the Cmd
// vectors, nothing before this commit exercised an Ack, a Nak, or a Ping
// frame against the reference at all: the simulator's `handle_established`
// returned early on any non-Cmd frame without even decrypting it, so every
// Ack this client ever sent in any test was generated and then discarded
// unread. If the ack framing were subtly wrong here, nothing would have
// noticed until real hardware either retransmitted forever or dropped the
// session.
#[test]
fn encrypts_an_ack_frame_byte_for_byte() {
    // Ack carries no body -- only the sequence byte is encrypted.
    let frame = encrypt_frame(&keys(), 0x42, FrameType::Ack, &[]);
    assert_eq!(frame.to_bytes(), unhex("4200100095eb3dc5eaa61755232adfee881ca3eb"));

    let frame = encrypt_frame(&keys(), 0x00, FrameType::Ack, &[]);
    assert_eq!(frame.to_bytes(), unhex("0000100007ddee3f704c606846d998133e2f5af3"));

    let frame = encrypt_frame(&keys(), 0xFE, FrameType::Ack, &[]);
    assert_eq!(frame.to_bytes(), unhex("fe001000892959f3fb40b33cbef413b00d82e663"));
}

#[test]
fn encrypts_a_nak_frame_with_the_same_ciphertext_as_the_matching_ack() {
    // Ack and Nak carry identical (empty) plaintext for a given sequence,
    // so their ciphertext is identical too -- only the frame type byte in
    // the header tells them apart. Verified against the reference
    // separately anyway, rather than just asserted equal to the Ack
    // vector, so a bug that accidentally made the frame type not matter
    // (e.g. an encryption scheme that folded `ty` into the plaintext) would
    // still be caught.
    let frame = encrypt_frame(&keys(), 0x42, FrameType::Nak, &[]);
    assert_eq!(frame.to_bytes(), unhex("4203100095eb3dc5eaa61755232adfee881ca3eb"));

    let ack = encrypt_frame(&keys(), 0x42, FrameType::Ack, &[]);
    assert_eq!(
        frame.payload, ack.payload,
        "Ack and Nak must share ciphertext for the same sequence; only the type byte differs"
    );
}

#[test]
fn encrypts_a_ping_command_byte_for_byte() {
    use syncleo::codec::command::Command;

    let frame = encrypt_frame(&keys(), 0x07, FrameType::Cmd, &Command::Ping.encode());
    assert_eq!(frame.to_bytes(), unhex("07011000b8a7028c5cce585ee7399f68f882ed33"));
}

#[test]
fn builds_the_reference_handshake_frame() {
    use syncleo::codec::handshake::handshake_frame;

    let our_public: [u8; 32] = core::array::from_fn(|i| i as u8);
    let token: [u8; 16] = unhex("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf").try_into().unwrap();

    let frame = handshake_frame(&keys(), 0x01, &our_public, &token);

    assert_eq!(
        frame.to_bytes(),
        unhex(concat!(
            "01013100",
            "00",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "3a5eb8fc7284be1035c8c6dc6c30aafa",
        )),
    );
}
