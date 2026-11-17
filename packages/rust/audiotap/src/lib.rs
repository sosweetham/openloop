//! Captures one app's sound on the Mac for `tetherd` (R2 S2): a private Core Audio process tap
//! on the app's PID, read by a real-time IOProc into a lock-free ring and cut into 16-bit
//! packets stamped with their `XrTime` capture time. A synthetic tone stands in for the tap in
//! tests. Design: docs/ARCHITECTURE.md § Audio.
#![cfg(target_os = "macos")]

pub mod analysis;
mod capture;
mod control;
mod convert;
mod coreaudio;
mod events;
mod ioproc;
mod latch;
mod packetizer;
pub mod probe;
mod ring;
mod synthetic;
mod time;

pub use capture::{AudioCapture, Config, Source};
pub use convert::f32_to_s16;
pub use events::{BlockReason, EndReason, Event, HalCall, RestartReason, status_text};
pub use latch::{blocked, clear_block, permission_proven};
pub use packetizer::{Item, Packet, Packetizer, PacketizerStats, StreamFormat};
pub use ring::{CHANNELS, CaptureStats};
pub use synthetic::{Beep, Tone};
pub use time::{Timebase, now_ns};
