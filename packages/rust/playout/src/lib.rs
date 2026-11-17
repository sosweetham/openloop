//! The headset's audio playout, free of any platform API so it runs in host tests. The thread
//! that receives packets writes them through a wait-free ring of fixed slots; the output's data
//! callback plays every frame at its packet's presentation time on the output's own clock. The
//! Quest client binds it to AAudio. See `docs/ARCHITECTURE.md` § Audio.

mod clock;
mod interp;
mod packet;
mod playout;
mod ring;
mod stats;
mod store;
mod sync;
mod tuner;

pub use clock::ClockMode;
pub use packet::{PacketWriter, Pushed};
pub use playout::{Config, OutputTiming, Playout, Timestamp, Tuning, new};
pub use stats::{Snapshot, Stats};
pub use tether_protocol::ClientTime;
pub use tuner::BufferTuner;
