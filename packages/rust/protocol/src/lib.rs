//! The wire protocol between `tetherd` and a headset client, shared by both ends.
//! Messages are transport-agnostic: a stream link frames them with [`StreamDecoder`]; a datagram
//! link would carry one encoded message per datagram. See `docs/ARCHITECTURE.md` § Transports.

mod codec;
mod message;

pub use codec::{DecodeError, StreamDecoder, encode, encode_framed};
pub use message::*;

/// Bumped on any incompatible change; peers with different versions refuse each other.
pub const PROTOCOL_VERSION: u16 = 1;

/// The Mac listens here on loopback; over USB the headset reaches it through `adb reverse`.
pub const USB_PORT: u16 = 47800;
