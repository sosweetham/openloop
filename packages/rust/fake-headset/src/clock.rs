//! The fake headset's clock: CLOCK_MONOTONIC, as on the Quest, moved by an offset and run fast or
//! slow by a drift, so the Mac's clock mapping has real work to do.

use openloop_protocol::ClientTime;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ClockModel {
    pub offset_ns: i64,
    /// How much faster than CLOCK_MONOTONIC the headset clock runs, in parts per million.
    pub drift_ppm: f64,
}

impl Default for ClockModel {
    /// Far from the Mac's clock and drifting like a real crystal, so an unmapped time shows up.
    fn default() -> Self {
        Self {
            offset_ns: 10_000_000_000,
            drift_ppm: 50.0,
        }
    }
}

/// CLOCK_MONOTONIC in nanoseconds: the base of the headset clock, and the clock the report's
/// intervals are measured in.
pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

#[derive(Clone, Copy, Debug)]
pub struct HeadsetClock {
    model: ClockModel,
    /// Where the drift starts accumulating.
    epoch: i64,
}

impl HeadsetClock {
    pub fn start(model: ClockModel) -> Self {
        Self {
            model,
            epoch: monotonic_ns(),
        }
    }

    pub fn now(&self) -> ClientTime {
        self.at(monotonic_ns())
    }

    /// The headset clock's reading at a CLOCK_MONOTONIC instant.
    pub fn at(&self, monotonic: i64) -> ClientTime {
        let drift = (monotonic - self.epoch) as f64 * self.model.drift_ppm * 1e-6;
        ClientTime(monotonic + self.model.offset_ns + drift.round() as i64)
    }

    /// The CLOCK_MONOTONIC instant at which the headset clock reads `time`.
    pub fn to_monotonic(&self, time: ClientTime) -> i64 {
        let since_epoch = (time.0 - self.model.offset_ns - self.epoch) as f64;
        self.epoch + (since_epoch / (1.0 + self.model.drift_ppm * 1e-6)).round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_and_drift_move_the_headset_clock() {
        let clock = HeadsetClock {
            model: ClockModel {
                offset_ns: 7_000,
                drift_ppm: 100.0,
            },
            epoch: 1_000_000_000,
        };
        assert_eq!(clock.at(1_000_000_000), ClientTime(1_000_007_000));
        // 10 s later the headset clock has gained 1 ms.
        assert_eq!(clock.at(11_000_000_000), ClientTime(11_001_007_000));
        assert_eq!(
            clock.to_monotonic(ClientTime(11_001_007_000)),
            11_000_000_000
        );
    }

    #[test]
    fn the_headset_clock_runs_monotonic() {
        let clock = HeadsetClock::start(ClockModel::default());
        let (a, b) = (clock.now(), clock.now());
        assert!(b >= a);
        assert!(
            (a.0 - monotonic_ns() - 10_000_000_000).abs() < 1_000_000,
            "offset applied"
        );
    }
}
