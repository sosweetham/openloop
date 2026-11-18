//! How large the output's buffer should be, decided on the thread that owns the output from
//! its underrun count. Changing the size is left to that thread: playout never calls
//! `AAudioStream_setBufferSizeInFrames` from the data callback.

use openloop_protocol::ClientTime;

pub struct BufferTuner {
    burst: i32,
    limit: i32,
    xruns: i32,
    raised_at: Option<i64>,
}

impl BufferTuner {
    /// The buffer never grows past this many bursts.
    pub const MAX_BURSTS: i32 = 8;
    /// Underruns closer together than this are one glitch and raise the buffer once.
    pub const SETTLE_NS: i64 = 250_000_000;

    /// From the stream as opened: `AAudioStream_getFramesPerBurst` and
    /// `AAudioStream_getBufferCapacityInFrames`.
    pub fn new(frames_per_burst: i32, capacity_frames: i32) -> Self {
        let burst = frames_per_burst.max(1);
        Self {
            burst,
            limit: (burst * Self::MAX_BURSTS).min(capacity_frames.max(burst)),
            xruns: 0,
            raised_at: None,
        }
    }

    /// Double buffering, where Android's low-latency guidance starts; set it after opening,
    /// before starting.
    pub fn initial_size(&self) -> i32 {
        (2 * self.burst).min(self.limit)
    }

    /// Call regularly with `AAudioStream_getXRunCount` and `AAudioStream_getBufferSizeInFrames`;
    /// returns a larger size to request when the output has underrun since the last call.
    pub fn poll(&mut self, now: ClientTime, xruns: i32, current_size: i32) -> Option<i32> {
        if xruns <= self.xruns {
            self.xruns = xruns;
            return None;
        }
        self.xruns = xruns;
        if self
            .raised_at
            .is_some_and(|at| now.0 - at < Self::SETTLE_NS)
        {
            return None;
        }
        let target = (current_size + self.burst).min(self.limit);
        if target <= current_size {
            return None;
        }
        self.raised_at = Some(now.0);
        Some(target)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn grows_one_burst_per_glitch_up_to_the_limit() {
        let mut tuner = BufferTuner::new(192, 4_096);
        assert_eq!(tuner.initial_size(), 384);
        let mut size = tuner.initial_size();
        assert_eq!(tuner.poll(ClientTime(0), 0, size), None);
        size = tuner.poll(ClientTime(MS), 1, size).unwrap();
        assert_eq!(size, 576);
        assert_eq!(
            tuner.poll(ClientTime(2 * MS), 3, size),
            None,
            "underruns of the same glitch"
        );
        let mut t = 300 * MS;
        for xruns in 4..20 {
            if let Some(next) = tuner.poll(ClientTime(t), xruns, size) {
                size = next;
            }
            t += 300 * MS;
        }
        assert_eq!(size, 192 * BufferTuner::MAX_BURSTS);
    }

    #[test]
    fn never_asks_beyond_the_capacity() {
        let mut tuner = BufferTuner::new(960, 1_920);
        assert_eq!(tuner.initial_size(), 1_920);
        assert_eq!(tuner.poll(ClientTime(0), 5, 1_920), None);
    }

    #[test]
    fn a_new_stream_counts_from_zero() {
        let mut tuner = BufferTuner::new(192, 4_096);
        assert!(tuner.poll(ClientTime(0), 7, 384).is_some());
        assert_eq!(tuner.poll(ClientTime(SECOND), 0, 384), None);
        assert_eq!(tuner.poll(ClientTime(2 * SECOND), 1, 384), Some(576));
    }

    const SECOND: i64 = 1_000_000_000;
}
