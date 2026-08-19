use aes::Aes128;
use cbc::{Decryptor, Encryptor};
use cipher::block_padding::Pkcs7;
use cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};

use super::frame::{Frame, FrameType};
use super::keys::{SessionKeys, rotl};
use crate::error::CodecError;

const BLOCK: usize = 16;

fn outgoing_key_iv(keys: &SessionKeys, seq: u8) -> ([u8; 16], [u8; 16]) {
    (rotl(&keys.outkey, seq & 0x0F), rotl(&keys.inkey, seq >> 4))
}

fn incoming_key_iv(keys: &SessionKeys, seq: u8) -> ([u8; 16], [u8; 16]) {
    (rotl(&keys.inkey, seq & 0x0F), rotl(&keys.outkey, seq >> 4))
}

/// Encrypt one frame. `body` is `[command_type, data..]` for `Cmd` frames and
/// empty for `Ack`/`Nak`, which carry nothing but their sequence number.
pub fn encrypt_frame(keys: &SessionKeys, seq: u8, ty: FrameType, body: &[u8]) -> Frame {
    let mut plain = Vec::with_capacity(1 + body.len());
    plain.push(seq);
    plain.extend_from_slice(body);

    let (key, iv) = outgoing_key_iv(keys, seq);
    let padded_len = (plain.len() / BLOCK + 1) * BLOCK;
    let mut buf = vec![0u8; padded_len];
    buf[..plain.len()].copy_from_slice(&plain);

    let ciphertext = Encryptor::<Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
        .expect("buffer sized for PKCS7")
        .to_vec();

    Frame::new(seq, ty, ciphertext)
}

/// Decrypt one frame, returning the body without its leading sequence byte.
pub fn decrypt_frame(keys: &SessionKeys, frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let seq = frame.head.seq;
    let (key, iv) = incoming_key_iv(keys, seq);

    let mut buf = frame.payload.clone();
    let plain = Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut buf)
        .map_err(|_| CodecError::BadPadding)?;

    let (&first, rest) = plain.split_first().ok_or(CodecError::EmptyBody)?;
    if first != seq {
        return Err(CodecError::SeqMismatch {
            head: seq,
            body: first,
        });
    }
    Ok(rest.to_vec())
}
