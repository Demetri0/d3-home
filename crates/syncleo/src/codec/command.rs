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

fn expect_len(ty: u8, data: &[u8], want: usize) -> Result<(), CodecError> {
    if data.len() == want {
        Ok(())
    } else {
        Err(CodecError::BadCommandLength { ty, len: data.len() })
    }
}

impl Event {
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let (&cmd, data) = body.split_first().ok_or(CodecError::EmptyBody)?;
        let flag = |d: &[u8]| -> Result<bool, CodecError> {
            expect_len(cmd, d, 1)?;
            Ok(d[0] == 1)
        };

        Ok(match cmd {
            ty::HANDSHAKE => {
                if data.len() < 5 {
                    return Err(CodecError::BadCommandLength { ty: cmd, len: data.len() });
                }
                Self::HandshakeResponse {
                    protocol: u16::from_le_bytes([data[0], data[1]]),
                    fw_major: data[2],
                    fw_minor: data[3],
                    mode: data[4],
                }
            }
            ty::MODE => {
                expect_len(cmd, data, 1)?;
                match PowerMode::from_u8(data[0]) {
                    Some(m) => Self::Mode(m),
                    None => Self::Unknown { ty: cmd, data: data.to_vec() },
                }
            }
            ty::TARGET_TEMPERATURE => {
                expect_len(cmd, data, 2)?;
                Self::TargetTemperature(data[0])
            }
            ty::CURRENT_TEMPERATURE => {
                expect_len(cmd, data, 2)?;
                Self::CurrentTemperature(data[0])
            }
            ty::ERROR => Self::Error(flag(data)?),
            ty::WATER => Self::WaterPresent(flag(data)?),
            ty::BACKLIGHT => Self::Backlight(flag(data)?),
            ty::CHILD_LOCK => Self::ChildLock(flag(data)?),
            ty::ACCESS_CONTROL => Self::AccessControl(flag(data)?),
            ty::HARDWARE => {
                expect_len(cmd, data, 3)?;
                Self::Hardware([data[0], data[1], data[2]])
            }
            ty::DIAGNOSTIC => Self::Diagnostic(data.to_vec()),
            ty::PING => {
                expect_len(cmd, data, 0)?;
                Self::Ping
            }
            other => Self::Unknown { ty: other, data: data.to_vec() },
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
    fn rejects_a_known_command_with_the_wrong_length() {
        assert!(matches!(
            Event::decode(&[20, 93]),
            Err(CodecError::BadCommandLength { ty: 20, len: 1 })
        ));
        assert!(matches!(Event::decode(&[]), Err(CodecError::EmptyBody)));
    }
}
