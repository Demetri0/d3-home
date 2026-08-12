use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("frame is shorter than its 4-byte header")]
    ShortFrame,
    #[error("declared payload length {declared} does not match {actual} bytes present")]
    LengthMismatch { declared: u16, actual: usize },
    #[error("unknown frame type {0}")]
    UnknownFrameType(u8),
    #[error("decrypted sequence {body} does not match header sequence {head}")]
    SeqMismatch { head: u8, body: u8 },
    #[error("invalid PKCS7 padding")]
    BadPadding,
    #[error("decrypted body is empty")]
    EmptyBody,
    #[error("command {ty} carries {len} bytes, which is not a valid length")]
    BadCommandLength { ty: u8, len: usize },
}
