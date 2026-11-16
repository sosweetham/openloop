//! The headset-facing half of Tether: transports, tracking, composition and encoding. Runs in
//! the app's process today and moves into `tetherd` unchanged. See `docs/ARCHITECTURE.md`.

#[cfg(target_os = "macos")]
pub mod compositor;
#[cfg(target_os = "macos")]
pub mod encoder;
pub mod headset;
pub mod link;
pub mod pose;
pub mod time;

pub use headset::{Headset, HeadsetState};
pub use link::{Link, Transport, UsbTransport};
