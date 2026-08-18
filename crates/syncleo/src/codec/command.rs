use crate::error::CodecError;

pub mod ty {
    pub const HANDSHAKE: u8 = 0;
    pub const MODE: u8 = 1;
    pub const TARGET_TEMPERATURE: u8 = 2;
    pub const ERROR: u8 = 7;
    pub const WATER: u8 = 9;
    pub const CURRENT_TEMPERATURE: u8 = 20;
    pub const BACKLIGHT: u8 = 28;
    pub const CHILD_LOCK: u8 = 30;
    pub const ACCESS_CONTROL: u8 = 133;
    pub const HARDWARE: u8 = 143;
    pub const DIAGNOSTIC: u8 = 145;
    pub const PING: u8 = 255;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    Off,
    On,
    Custom,
}

impl PowerMode {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::Custom => 3,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            3 => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Mode(PowerMode),
    TargetTemperature(u8),
    Ping,
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Mode(m) => vec![ty::MODE, m.as_u8()],
            Self::TargetTemperature(t) => vec![ty::TARGET_TEMPERATURE, *t, 0],
            Self::Ping => vec![ty::PING],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    HandshakeResponse { protocol: u16, fw_major: u8, fw_minor: u8, mode: u8 },
    Mode(PowerMode),
    TargetTemperature(u8),
    CurrentTemperature(u8),
    WaterPresent(bool),
    Error(bool),
    Backlight(bool),
    ChildLock(bool),
    AccessControl(bool),
    Hardware([u8; 3]),
    Diagnostic(Vec<u8>),
    Ping,
    Unknown { ty: u8, data: Vec<u8> },
}

impl Event {
    /// Decode a frame body (`[command_type, data...]`) into an [`Event`].
    ///
    /// An empty body is the one thing this refuses outright: there is no
    /// command type byte to even look at, so [`CodecError::EmptyBody`] is
    /// the only error this returns. A *known* command code carrying data of
    /// an unexpected length degrades to [`Self::Unknown`] instead of
    /// failing -- same as a command code this client has never heard of.
    /// The alternative (an `Err` here) meant the whole frame got dropped
    /// with no diagnostic anywhere: `watch` never saw it, and there was no
    /// way for an operator to learn a real device sent something this
    /// client did not expect. `Unknown` surfaces the raw bytes instead.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let (&cmd, data) = body.split_first().ok_or(CodecError::EmptyBody)?;
        let unknown = || Self::Unknown { ty: cmd, data: data.to_vec() };
        // The five boolean-flag commands all decode the same one-byte
        // shape; `make` is which variant to wrap the bit in.
        let flag = |make: fn(bool) -> Self| match data {
            [v] => make(*v == 1),
            _ => unknown(),
        };

        Ok(match cmd {
            ty::HANDSHAKE => {
                if data.len() < 5 {
                    unknown()
                } else {
                    Self::HandshakeResponse {
                        protocol: u16::from_le_bytes([data[0], data[1]]),
                        fw_major: data[2],
                        fw_minor: data[3],
                        mode: data[4],
                    }
                }
            }
            ty::MODE => match data {
                [v] => match PowerMode::from_u8(*v) {
                    Some(m) => Self::Mode(m),
                    None => unknown(),
                },
                _ => unknown(),
            },
            ty::TARGET_TEMPERATURE => match data {
                [whole, _hundredths] => Self::TargetTemperature(*whole),
                _ => unknown(),
            },
            ty::CURRENT_TEMPERATURE => match data {
                [whole, _hundredths] => Self::CurrentTemperature(*whole),
                _ => unknown(),
            },
            ty::ERROR => flag(Self::Error),
            ty::WATER => flag(Self::WaterPresent),
            ty::BACKLIGHT => flag(Self::Backlight),
            ty::CHILD_LOCK => flag(Self::ChildLock),
            ty::ACCESS_CONTROL => flag(Self::AccessControl),
            ty::HARDWARE => match data {
                [a, b, c] => Self::Hardware([*a, *b, *c]),
                _ => unknown(),
            },
            ty::DIAGNOSTIC => Self::Diagnostic(data.to_vec()),
            ty::PING => {
                if data.is_empty() {
                    Self::Ping
                } else {
                    unknown()
                }
            }
            _ => unknown(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_outgoing_commands() {
        assert_eq!(Command::Mode(PowerMode::On).encode(), vec![1, 1]);
        assert_eq!(Command::Mode(PowerMode::Off).encode(), vec![1, 0]);
        assert_eq!(Command::Mode(PowerMode::Custom).encode(), vec![1, 3]);
        assert_eq!(Command::TargetTemperature(80).encode(), vec![2, 80, 0]);
        assert_eq!(Command::Ping.encode(), vec![255]);
    }

    #[test]
    fn decodes_temperatures_discarding_hundredths() {
        assert_eq!(Event::decode(&[20, 93, 50]).unwrap(), Event::CurrentTemperature(93));
        assert_eq!(Event::decode(&[2, 80, 0]).unwrap(), Event::TargetTemperature(80));
    }

    #[test]
    fn decodes_boolean_state() {
        assert_eq!(Event::decode(&[9, 1]).unwrap(), Event::WaterPresent(true));
        assert_eq!(Event::decode(&[7, 0]).unwrap(), Event::Error(false));
        assert_eq!(Event::decode(&[28, 1]).unwrap(), Event::Backlight(true));
        assert_eq!(Event::decode(&[30, 1]).unwrap(), Event::ChildLock(true));
        assert_eq!(Event::decode(&[133, 0]).unwrap(), Event::AccessControl(false));
    }

    #[test]
    fn decodes_a_handshake_response() {
        // protocol 2 little-endian, firmware 1.4, mode 0, then an echoed token
        let body = [0u8, 0x02, 0x00, 0x01, 0x04, 0x00, 0xAA, 0xBB];
        assert_eq!(
            Event::decode(&body).unwrap(),
            Event::HandshakeResponse { protocol: 2, fw_major: 1, fw_minor: 4, mode: 0 }
        );
    }

    #[test]
    fn decodes_hardware_and_diagnostics() {
        assert_eq!(Event::decode(&[143, 1, 1, 1]).unwrap(), Event::Hardware([1, 1, 1]));
        assert_eq!(Event::decode(&[145, 9, 9]).unwrap(), Event::Diagnostic(vec![9, 9]));
        assert_eq!(Event::decode(&[255]).unwrap(), Event::Ping);
    }

    #[test]
    fn keeps_unknown_commands_instead_of_failing() {
        // The firmware sends messages the reference never identified. Surviving
        // them matters more than understanding them.
        assert_eq!(
            Event::decode(&[77, 1, 2, 3]).unwrap(),
            Event::Unknown { ty: 77, data: vec![1, 2, 3] }
        );
    }

    #[test]
    // Changed from asserting `Err(CodecError::BadCommandLength { .. })`: a
    // known command code carrying an unexpected data length used to fail
    // outright, which dropped the whole frame with no diagnostic anywhere
    // -- `watch` never saw it, and there was no way for an operator to
    // learn a real device had sent something unexpected. It now degrades
    // to `Unknown` (same as a command code this client has never heard of
    // at all) instead of failing, so the raw bytes still reach the
    // operator. An empty body is different in kind -- there is no command
    // type byte to even look at -- and stays a hard error.
    fn a_known_command_with_the_wrong_length_degrades_to_unknown_instead_of_failing() {
        assert_eq!(Event::decode(&[20, 93]).unwrap(), Event::Unknown { ty: 20, data: vec![93] });
        assert!(matches!(Event::decode(&[]), Err(CodecError::EmptyBody)));
    }
}
