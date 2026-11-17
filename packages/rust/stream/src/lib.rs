//! The headset-facing half of Tether: transports, tracking, composition and encoding. `tetherd`
//! runs the link; the app still composites and encodes until R2 S3. See `docs/ARCHITECTURE.md`.

#[cfg(target_os = "macos")]
pub mod compositor;
#[cfg(target_os = "macos")]
pub mod encoder;
pub mod headset;
pub mod link;
pub mod pose;
#[cfg(target_os = "macos")]
pub mod shared;
pub mod stats;
pub mod time;
pub mod video;

pub use headset::{Change, Headset, HeadsetState, Observer, PIPELINE_LATENCY_NS, Tracked};
pub use link::{Link, Transport, UsbTransport};
