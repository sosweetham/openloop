//! Stands in for the Quest on the Mac's loopback port, for automated tests and the e2e run: sends
//! a Quest 3-like profile, streams scripted tracking in its own drifting clock, answers clock
//! pings, checks the video, audio and haptics that come back, and reports the session as JSON.
//! See `docs/ROADMAP.md` § Quality gates.
//!
//! ```no_run
//! use std::time::Duration;
//! use tether_fake_headset::{Config, run};
//!
//! let report = run(Config::new(51234), Duration::from_secs(10));
//! println!("{}", report.to_json());
//! ```

pub mod audio;
pub mod clock;
mod headset;
pub mod report;
pub mod script;
pub mod video;

pub use clock::ClockModel;
pub use headset::{Config, FakeHeadset, quest_3, run};
pub use report::Report;
pub use script::{Action, Cue, Hand, Motion, Script, parse_cue};
