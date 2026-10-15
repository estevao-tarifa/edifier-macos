//! Frame encoding/decoding: `AA|len|payload|cksum` out, `BB|CC|len|payload|cksum` in.

use thiserror::Error;

/// Seed of the additive 16-bit checksum (`0x2019`).
pub const CHECKSUM_BASE: u16 = 8217;

/// Maximum payload size representable in the one-byte length field.
pub const MAX_PAYLOAD_LEN: usize = u8::MAX as usize;

/// Header byte of a host -> device frame.
pub const HEAD_HOST: u8 = 0xAA;
/// Header byte of a device -> host state/value frame.
pub const HEAD_STATE: u8 = 0xBB;
/// Header byte of a device -> host acknowledgement frame.
pub const HEAD_ACK: u8 = 0xCC;

#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum FrameError {
    #[error("payload of {0} bytes exceeds the 255 byte protocol limit")]
    PayloadTooLong(usize),
    #[error("unexpected header byte 0x{0:02X} (expected 0xAA, 0xBB or 0xCC)")]
    BadHeader(u8),
    #[error("checksum mismatch: computed 0x{computed:04X}, received 0x{received:04X}")]
    ChecksumMismatch { computed: u16, received: u16 },
}

/// Origin of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    /// Sent by the host (`0xAA`).
    Host,
    /// Device state or value response (`0xBB`).
    State,
    /// Device acknowledgement (`0xCC`).
    Ack,
}

impl Head {
    pub fn from_byte(byte: u8) -> Result<Self, FrameError> {
        match byte {
            HEAD_HOST => Ok(Head::Host),
            HEAD_STATE => Ok(Head::State),
            HEAD_ACK => Ok(Head::Ack),
            other => Err(FrameError::BadHeader(other)),
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Head::Host => HEAD_HOST,
            Head::State => HEAD_STATE,
            Head::Ack => HEAD_ACK,
        }
    }
}

/// Additive checksum: `0x2019` plus every byte of the frame so far, as `u16`.
pub fn checksum(bytes: &[u8]) -> u16 {
    bytes
        .iter()
        .fold(CHECKSUM_BASE, |acc, byte| acc.wrapping_add(*byte as u16))
}

/// Encode a payload into a complete host frame (`AA|len|payload|sum_hi|sum_lo`).
pub fn encode(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLong(payload.len()));
    }
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(HEAD_HOST);
    out.push(payload.len() as u8);
    out.extend_from_slice(payload);
    let sum = checksum(&out);
    out.push((sum >> 8) as u8);
    out.push((sum & 0xFF) as u8);
    Ok(out)
}

/// A validated, checksum-checked frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub head: Head,
    /// Payload bytes without header, length or checksum.
    pub payload: Vec<u8>,
}

impl Frame {
    /// Validate a complete frame (including checksum) and split it into header + payload.
    pub fn parse(raw: &[u8]) -> Result<Self, FrameError> {
        if raw.len() < 4 {
            return Err(FrameError::BadHeader(*raw.first().unwrap_or(&0)));
        }
        let head = Head::from_byte(raw[0])?;
        let len = raw[1] as usize;
        if raw.len() != len + 4 {
            return Err(FrameError::BadHeader(raw[0]));
        }
        let body = &raw[..len + 2];
        let expected = checksum(body);
        let received = ((raw[len + 2] as u16) << 8) | raw[len + 3] as u16;
        if expected != received {
            return Err(FrameError::ChecksumMismatch {
                computed: expected,
                received,
            });
        }
        Ok(Frame {
            head,
            payload: raw[2..2 + len].to_vec(),
        })
    }

    /// Encode this frame; `Head::Host` frames are the only ones that are ever sent.
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        if self.payload.len() > MAX_PAYLOAD_LEN {
            return Err(FrameError::PayloadTooLong(self.payload.len()));
        }
        let mut out = Vec::with_capacity(self.payload.len() + 4);
        out.push(self.head.as_byte());
        out.push(self.payload.len() as u8);
        out.extend_from_slice(&self.payload);
        let sum = checksum(&out);
        out.push((sum >> 8) as u8);
        out.push((sum & 0xFF) as u8);
        Ok(out)
    }

    /// Command byte, when the payload is non-empty.
    pub fn command_byte(&self) -> Option<u8> {
        self.payload.first().copied()
    }

    /// Parameter bytes following the command byte.
    pub fn params(&self) -> &[u8] {
        self.payload.get(1..).unwrap_or(&[])
    }
}

/// Incremental decoder for the byte stream delivered by a transport.
///
/// BLE notifications and RFCOMM reads both split frames arbitrarily, and the device drops
/// malformed tails on its own, so the decoder assumes resynchronisation is *host* work:
/// bytes before a valid header are discarded, and a frame whose length field is not yet
/// satisfied is buffered until more data arrives.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed recently received bytes into the decoder.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Drop buffered bytes (5 s of silence resets the frame boundary, matching `comm.cpp`).
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Bytes currently buffered but not yet forming a complete frame.
    pub fn pending(&self) -> &[u8] {
        &self.buf
    }

    /// Render the current buffer for diagnostics.
    pub fn pending_hex(&self) -> String {
        self.buf
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Next complete frame, or `None` when more bytes are required.
    ///
    /// Bytes that cannot belong to a frame are dropped, and a frame whose checksum fails is
    /// rejected one byte at a time so a false header cannot swallow the next valid frame.
    pub fn next_frame(&mut self) -> Option<Result<Frame, FrameError>> {
        while let Some(&first) = self.buf.first() {
            if first == HEAD_HOST || first == HEAD_STATE || first == HEAD_ACK {
                break;
            }
            let bad = self.buf.remove(0);
            tracing::debug!("discarding out-of-frame byte 0x{bad:02X}");
        }
        if self.buf.len() < 2 {
            return None;
        }
        let total = self.buf[1] as usize + 4;
        if self.buf.len() < total {
            return None;
        }
        let candidate: Vec<u8> = self.buf[..total].to_vec();
        match Frame::parse(&candidate) {
            Ok(frame) => {
                self.buf.drain(..total);
                Some(Ok(frame))
            }
            Err(err) => {
                self.buf.drain(..1);
                Some(Err(err))
            }
        }
    }
}
