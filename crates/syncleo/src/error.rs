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

#[derive(Debug, Error)]
pub enum Error {
    #[error("device did not respond in time")]
    Timeout,
    #[error("device rejected the handshake; the token is probably wrong")]
    HandshakeRejected,
    #[error("device rejected the command (NAK)")]
    DeviceNak,
    #[error("connection lost: no traffic from the device")]
    Silence,
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    // Confirmed against the real device: this fired once, right after the
    // kettle was set back on its base, and a retry moments later succeeded
    // -- a device still booting, not a device that had genuinely stopped
    // reporting anything. The wording says so, without promising a retry
    // will always help.
    #[error(
        "device reported no state at all within the window; it may not send a state report after \
         the handshake, its report arrived too late, or it is still booting after just being \
         powered on -- if so, trying again in a few seconds may work"
    )]
    NoState,
    #[error("device advertises curve {curve} protocol {protocol}, which this client was not written for")]
    UnsupportedProtocol { curve: u8, protocol: u16 },
    #[error("service record has no usable address (only link-local addresses, or none at all)")]
    NoUsableAddress,
    #[error("malformed mDNS service record: {0}")]
    BadServiceRecord(String),
    #[error(
        "address is link-local IPv6 with no known interface; connecting without a scope id \
         fails at the OS level (EINVAL)"
    )]
    LinkLocalAddressWithoutScope,
}
