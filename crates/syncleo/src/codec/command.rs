use crate::error::CodecError;

pub mod ty {
    pub const HANDSHAKE: u8 = 0;
    pub const MODE: u8 = 1;
    pub const TARGET_TEMPERATURE: u8 = 2;
    pub const ERROR: u8 = 7;
    pub const VOLUME: u8 = 9;
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
    HandshakeResponse {
        protocol: u16,
        fw_major: u8,
        fw_minor: u8,
        mode: u8,
    },
    Mode(PowerMode),
    TargetTemperature(u8),
    CurrentTemperature(u8),
    /// Code 9. The Python reference this protocol was derived from calls
    /// this `volume` and decodes it there as `byte == 1`, the same as
    /// every other one-byte flag -- which is where the inherited (and
    /// wrong) `WaterPresent(bool)` name and decode came from. Tested
    /// against a real device with a full litre of water in it, the byte
    /// it sent did not read as `1`, so that boolean collapse was throwing
    /// away real information and asserting something false. What the byte
    /// actually means -- a level, a volume in some unit, something else
    /// entirely -- is not established; this carries it raw and undecoded
    /// until someone has evidence to say otherwise.
    Volume(u8),
    Error(bool),
    Backlight(bool),
    ChildLock(bool),
    AccessControl(bool),
    Hardware([u8; 3]),
    Diagnostic(Vec<u8>),
    Ping,
    Unknown {
        ty: u8,
        data: Vec<u8>,
    },
}

impl Event {
    /// The protocol command code this event arrived as.
    ///
    /// Kept alongside the decoded meaning so a trace can show both: the
    /// number is what you match against a packet capture or the reference
    /// implementation, and it is the only handle on the codes nobody has
    /// identified yet.
    pub fn code(&self) -> u8 {
        match self {
            Self::HandshakeResponse { .. } => ty::HANDSHAKE,
            Self::Mode(_) => ty::MODE,
            Self::TargetTemperature(_) => ty::TARGET_TEMPERATURE,
            Self::Error(_) => ty::ERROR,
            Self::Volume(_) => ty::VOLUME,
            Self::CurrentTemperature(_) => ty::CURRENT_TEMPERATURE,
            Self::Backlight(_) => ty::BACKLIGHT,
            Self::ChildLock(_) => ty::CHILD_LOCK,
            Self::AccessControl(_) => ty::ACCESS_CONTROL,
            Self::Hardware(_) => ty::HARDWARE,
            Self::Diagnostic(_) => ty::DIAGNOSTIC,
            Self::Ping => ty::PING,
            Self::Unknown { ty, .. } => *ty,
        }
    }

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
        let unknown = || Self::Unknown {
            ty: cmd,
            data: data.to_vec(),
        };
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
            ty::VOLUME => match data {
                [v] => Self::Volume(*v),
                _ => unknown(),
            },
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

/// Decode a code-145 vendor diagnostic payload into its tag/value pairs, if
/// it has the shape confirmed against three real device captures: a 20-byte
/// header (contents unknown; only its length is fixed) followed by one or
/// more 4-byte ASCII tag / 4-byte little-endian `u32` value pairs. Every
/// capture so far was exactly 52 bytes -- 20 + four pairs -- but nothing
/// pins the pair count at four, so any positive number of complete pairs is
/// accepted.
///
/// This is a *separate* function from [`Event::decode`], not folded into
/// it: `Event::Diagnostic` keeps carrying the payload raw (so nothing is
/// ever hidden or lost to a parse the device never asked us to trust), and
/// callers -- currently `d3home`'s human `watch` view -- decide whether and
/// how to apply this on top.
///
/// Returns `None`, never a best-effort guess, for anything that does not
/// cleanly fit: too short to hold the header, a length that leaves a
/// trailing partial pair, or a tag containing a byte outside printable
/// ASCII once trailing NUL padding is trimmed. Callers are expected to fall
/// back to showing the raw bytes in that case.
///
/// Tags are trimmed of *trailing* NUL bytes only (real captures pad short
/// names that way, e.g. `rtT\0`) -- nothing else is normalised, since a tag
/// can genuinely end in a printable space (`Tmr `) and collapsing that would
/// quietly lose information.
pub fn decode_diagnostic(payload: &[u8]) -> Option<Vec<(String, u32)>> {
    const HEADER_LEN: usize = 20;
    const PAIR_LEN: usize = 8;

    if payload.len() <= HEADER_LEN {
        return None;
    }
    let pairs = &payload[HEADER_LEN..];
    if !pairs.len().is_multiple_of(PAIR_LEN) {
        return None;
    }

    pairs
        .chunks_exact(PAIR_LEN)
        .map(|pair| {
            let (tag, value) = pair.split_at(4);
            let tag = trim_trailing_nul(tag);
            if tag.is_empty() || !tag.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
                return None;
            }
            let tag = String::from_utf8(tag.to_vec()).expect("checked printable ASCII above");
            let value = u32::from_le_bytes(
                value
                    .try_into()
                    .expect("chunk is PAIR_LEN, value half is 4 bytes"),
            );
            Some((tag, value))
        })
        .collect()
}

fn trim_trailing_nul(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &bytes[..end]
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn decodes_the_worked_example_from_a_real_capture() {
        // Real device capture, verbatim: 20-byte header then four pairs.
        let payload: Vec<u8> = vec![
            255, 2, 0, 0, 172, 56, 0, 0, 192, 111, 65, 4, 0, 0, 0, 0, 157, 47, 54,
            3, // header
            117, 100, 112, 115, 186, 236, 7, 3, // udps = 50851002
            114, 116, 84, 0, 249, 9, 4, 0, // rtT\0 = 264697
            112, 112, 84, 0, 52, 211, 11, 0, // ppT\0 = 774964
            84, 109, 114, 32, 218, 9, 5, 0, // "Tmr " = 330202
        ];
        assert_eq!(payload.len(), 52);
        let decoded = decode_diagnostic(&payload).expect("worked example should decode");
        assert_eq!(
            decoded,
            vec![
                ("udps".to_string(), 50851002),
                ("rtT".to_string(), 264697),
                ("ppT".to_string(), 774964),
                ("Tmr ".to_string(), 330202),
            ]
        );
    }

    #[test]
    fn trims_nul_padding_but_keeps_a_genuine_trailing_space() {
        let mut payload = vec![0u8; 20];
        payload.extend_from_slice(b"IDLE");
        payload.extend_from_slice(&7u32.to_le_bytes());
        payload.extend_from_slice(b"Tmr ");
        payload.extend_from_slice(&8u32.to_le_bytes());
        let decoded = decode_diagnostic(&payload).unwrap();
        assert_eq!(
            decoded,
            vec![("IDLE".to_string(), 7), ("Tmr ".to_string(), 8)]
        );
    }

    #[test]
    fn a_one_byte_payload_falls_back_instead_of_decoding() {
        // The real device sends this alongside the 52-byte diagnostic in the
        // same burst; it does not fit the tag/value shape at all.
        assert_eq!(decode_diagnostic(&[0]), None);
    }

    #[test]
    fn a_trailing_partial_pair_falls_back_instead_of_decoding() {
        let mut payload = vec![0u8; 20];
        payload.extend_from_slice(b"IDLE");
        payload.extend_from_slice(&7u32.to_le_bytes());
        payload.push(1); // one extra byte: not a full pair
        assert_eq!(decode_diagnostic(&payload), None);
    }

    #[test]
    fn a_non_printable_tag_falls_back_instead_of_decoding() {
        let mut payload = vec![0u8; 20];
        payload.extend_from_slice(&[0xFF, 0x01, 0x02, 0x03]);
        payload.extend_from_slice(&7u32.to_le_bytes());
        assert_eq!(decode_diagnostic(&payload), None);
    }

    #[test]
    fn exactly_the_header_with_no_pairs_falls_back() {
        // 20 bytes and nothing else: no tag/value pair to show, so this is
        // not the shape either -- distinct from "zero-length payload".
        assert_eq!(decode_diagnostic(&[0u8; 20]), None);
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
        assert_eq!(
            Event::decode(&[20, 93, 50]).unwrap(),
            Event::CurrentTemperature(93)
        );
        assert_eq!(
            Event::decode(&[2, 80, 0]).unwrap(),
            Event::TargetTemperature(80)
        );
    }

    #[test]
    fn decodes_boolean_state() {
        assert_eq!(Event::decode(&[7, 0]).unwrap(), Event::Error(false));
        assert_eq!(Event::decode(&[28, 1]).unwrap(), Event::Backlight(true));
        assert_eq!(Event::decode(&[30, 1]).unwrap(), Event::ChildLock(true));
        assert_eq!(
            Event::decode(&[133, 0]).unwrap(),
            Event::AccessControl(false)
        );
    }

    #[test]
    fn decodes_volume_as_a_raw_byte_not_a_boolean() {
        // The old `WaterPresent(bool)` decode collapsed this byte through
        // `== 1`, so any value other than 0 or 1 silently became `false`
        // ("no water") -- exactly what a real device with a full litre in
        // it reported. Pinning a value outside {0, 1} here is what would
        // have caught that: it must survive decoding intact, as a number,
        // not get squashed into a boolean.
        assert_eq!(Event::decode(&[9, 1]).unwrap(), Event::Volume(1));
        assert_eq!(Event::decode(&[9, 0]).unwrap(), Event::Volume(0));
        assert_eq!(Event::decode(&[9, 42]).unwrap(), Event::Volume(42));
    }

    #[test]
    fn decodes_a_handshake_response() {
        // protocol 2 little-endian, firmware 1.4, mode 0, then an echoed token
        let body = [0u8, 0x02, 0x00, 0x01, 0x04, 0x00, 0xAA, 0xBB];
        assert_eq!(
            Event::decode(&body).unwrap(),
            Event::HandshakeResponse {
                protocol: 2,
                fw_major: 1,
                fw_minor: 4,
                mode: 0
            }
        );
    }

    #[test]
    fn decodes_hardware_and_diagnostics() {
        assert_eq!(
            Event::decode(&[143, 1, 1, 1]).unwrap(),
            Event::Hardware([1, 1, 1])
        );
        assert_eq!(
            Event::decode(&[145, 9, 9]).unwrap(),
            Event::Diagnostic(vec![9, 9])
        );
        assert_eq!(Event::decode(&[255]).unwrap(), Event::Ping);
    }

    #[test]
    fn keeps_unknown_commands_instead_of_failing() {
        // The firmware sends messages the reference never identified. Surviving
        // them matters more than understanding them.
        assert_eq!(
            Event::decode(&[77, 1, 2, 3]).unwrap(),
            Event::Unknown {
                ty: 77,
                data: vec![1, 2, 3]
            }
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
        assert_eq!(
            Event::decode(&[20, 93]).unwrap(),
            Event::Unknown {
                ty: 20,
                data: vec![93]
            }
        );
        assert!(matches!(Event::decode(&[]), Err(CodecError::EmptyBody)));
    }
}
