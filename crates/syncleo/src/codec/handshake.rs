use aes::Aes128;
use cbc::Encryptor;
use cipher::{BlockModeEncrypt, KeyIvInit};

use super::frame::{Frame, FrameType};
use super::keys::SessionKeys;

/// Build the opening frame of a session.
///
/// The handshake body is assembled by hand and deliberately bypasses the frame
/// encryption used everywhere else: the token is a single AES block encrypted
/// with the unrotated keys, and no padding is applied. The device authenticates
/// us by decrypting it successfully.
pub fn handshake_frame(
    keys: &SessionKeys,
    seq: u8,
    our_public_wire: &[u8; 32],
    token: &[u8; 16],
) -> Frame {
    let mut block = *token;
    Encryptor::<Aes128>::new(&keys.outkey.into(), &keys.inkey.into())
        .encrypt_blocks(core::slice::from_mut((&mut block).into()));

    let mut payload = Vec::with_capacity(1 + 32 + 16);
    payload.push(0x00);
    payload.extend_from_slice(our_public_wire);
    payload.extend_from_slice(&block);

    Frame::new(seq, FrameType::Cmd, payload)
}
