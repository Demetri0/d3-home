use crate::error::CodecError;

pub const HEAD_LEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Ack,
    Cmd,
    Aux,
    Nak,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ack),
            1 => Some(Self::Cmd),
            2 => Some(Self::Aux),
            3 => Some(Self::Nak),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Ack => 0,
            Self::Cmd => 1,
            Self::Aux => 2,
            Self::Nak => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHead {
    pub seq: u8,
    pub ty: FrameType,
    pub len: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub head: FrameHead,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(seq: u8, ty: FrameType, payload: Vec<u8>) -> Self {
        let len = payload.len() as u16;
        Self {
            head: FrameHead { seq, ty, len },
            payload,
        }
    }

    pub fn parse(buf: &[u8]) -> Result<Self, CodecError> {
        if buf.len() < HEAD_LEN {
            return Err(CodecError::ShortFrame);
        }
        let seq = buf[0];
        let ty = FrameType::from_u8(buf[1]).ok_or(CodecError::UnknownFrameType(buf[1]))?;
        let len = u16::from_le_bytes([buf[2], buf[3]]);
        let payload = &buf[HEAD_LEN..];
        if payload.len() != len as usize {
            return Err(CodecError::LengthMismatch {
                declared: len,
                actual: payload.len(),
            });
        }
        Ok(Self {
            head: FrameHead { seq, ty, len },
            payload: payload.to_vec(),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEAD_LEN + self.payload.len());
        out.push(self.head.seq);
        out.push(self.head.ty.as_u8());
        out.extend_from_slice(&self.head.len.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_frame() {
        // seq=0x35, type=1 (Cmd), len=0x0010 little-endian, then 16 bytes
        let mut buf = vec![0x35, 0x01, 0x10, 0x00];
        buf.extend_from_slice(&[0xAA; 16]);

        let frame = Frame::parse(&buf).expect("valid frame");

        assert_eq!(frame.head.seq, 0x35);
        assert_eq!(frame.head.ty, FrameType::Cmd);
        assert_eq!(frame.head.len, 16);
        assert_eq!(frame.payload, vec![0xAA; 16]);
    }

    #[test]
    fn round_trips_through_bytes() {
        let mut buf = vec![0xC2, 0x00, 0x02, 0x00, 0xDE, 0xAD];
        let frame = Frame::parse(&buf).unwrap();
        assert_eq!(frame.to_bytes(), buf);

        buf.truncate(4);
        assert!(
            Frame::parse(&buf).is_err(),
            "declared length must be honoured"
        );
    }

    #[test]
    fn rejects_a_frame_shorter_than_its_header() {
        assert!(matches!(
            Frame::parse(&[0x01, 0x02, 0x03]),
            Err(CodecError::ShortFrame)
        ));
    }

    #[test]
    fn rejects_a_length_that_disagrees_with_the_buffer() {
        let buf = vec![0x00, 0x01, 0xFF, 0x00, 0x01, 0x02];
        assert!(matches!(
            Frame::parse(&buf),
            Err(CodecError::LengthMismatch {
                declared: 255,
                actual: 2
            })
        ));
    }

    #[test]
    fn rejects_an_unknown_frame_type() {
        let buf = vec![0x00, 0x09, 0x00, 0x00];
        assert!(matches!(
            Frame::parse(&buf),
            Err(CodecError::UnknownFrameType(9))
        ));
    }
}
