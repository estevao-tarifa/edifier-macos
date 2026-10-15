//! Wire format for the Edifier headphone control protocol.
//!
//! # Provenance
//!
//! Reverse engineered from the Edifier Connect app traffic captured in
//! [mEDIFIER](https://github.com/wh201906/mEDIFIER) (`doc/KnownCommands/W820NB_Double_Gold.txt`,
//! `Qt/comms/comm.cpp`, `Qt/devices/basedevice.cpp`) and re-verified against the captured
//! send/read pairs in `tests/vectors.rs`.
//!
//! # Framing
//!
//! ```text
//! host -> device:  AA | len | payload[len] | sum_hi | sum_lo
//! device -> host:  BB | len | payload[len] | sum_hi | sum_lo     (state / value response)
//! device -> host:  CC | len | payload[len] | sum_hi | sum_lo     (acknowledgement)
//! ```
//!
//! `len` counts *payload* bytes only. The checksum is a big-endian 16-bit sum seeded with
//! `0x2019` (8217) over every byte preceding it, header and length included:
//!
//! ```text
//! AA 02 C1 01        -> 0x2019 + 0xAA + 0x02 + 0xC1 + 0x01 = 0x2187 -> AA 02 C1 01 21 87
//! ```
//!
//! Payload byte 0 is the command, remaining bytes are parameters. `BB` with `len == 2` carries a
//! single state byte, `BB` with `len > 2` carries a value of variable length, and `CC` frames are
//! acknowledgements (the request is echoed back).

pub mod command;
pub mod event;
pub mod frame;
pub mod model;

pub use command::{AmbientVolume, AncMode, Command, ControlSettings, LdacMode, SoundEffect};
pub use event::{Event, Status};
pub use frame::{CHECKSUM_BASE, Frame, FrameDecoder, FrameError, Head, MAX_PAYLOAD_LEN, checksum};
pub use model::{DeviceModel, Feature};

/// Ambient-sound volume range accepted by the firmware (mirrors mEDIFIER's slider bounds).
pub const AMBIENT_VOLUME_RANGE: std::ops::RangeInclusive<i8> = -3..=3;

/// Firmware offset applied to the ambient-sound volume byte on the wire.
pub const AMBIENT_VOLUME_OFFSET: i8 = 6;

/// Shutdown-timer range in minutes.
pub const SHUTDOWN_TIMER_RANGE: std::ops::RangeInclusive<u8> = 0..=180;

/// Prompt-volume range (0 keeps the voice prompts silent).
pub const PROMPT_VOLUME_RANGE: std::ops::RangeInclusive<u8> = 0..=15;
