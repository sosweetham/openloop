//! The wire protocol between `openloopd` and a headset client, shared by both ends.
//! Messages are transport-agnostic: a stream link frames them with [`StreamDecoder`]; a datagram
//! link would carry one encoded message per datagram. See `docs/ARCHITECTURE.md` § Transports.
//!
//! The protocol grows without a version bump, because receivers tolerate what they do not know:
//! - a message kind the receiver does not know is skipped;
//! - fields are only ever appended to a payload: a receiver ignores bytes after the fields it
//!   knows, and gives an appended field the sender did not write its default.
//!
//! Any other change bumps [`PROTOCOL_VERSION`].

mod codec;
mod message;

pub use codec::{
    AudioPacketRef, DecodeError, StreamDecoder, decode, encode, encode_audio_framed, encode_framed,
    encode_video_framed,
};
pub use message::*;

/// Peers refuse each other on any difference: the Mac serves only a [`ClientHello`] of its own
/// version, and a client streams only after a [`ServerHello`] of its own.
pub const PROTOCOL_VERSION: u16 = 2;

/// The Mac listens here on loopback; over USB the headset reaches it through `adb reverse`.
pub const USB_PORT: u16 = 47800;
