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
