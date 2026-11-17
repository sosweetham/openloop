//! When the output will play the next frame written to it. The output's presentation timestamps
//! are trusted only once two agree, a lone outlier is ignored, and a lasting step is followed
//! once a second timestamp confirms it. Without usable timestamps the callback's own start time
//! plus an estimated latency stands in.

use crate::playout::{OutputTiming, Tuning};
use crate::stats::{Counter, Stats};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum ClockMode {
    /// After the output (re)started, until two timestamps agree; playout is silent.
    #[default]
    Waiting = 0,
    /// From the output's presentation timestamps.
    Timestamped = 1,
    /// No usable timestamps: the callback's start plus the output buffer plus a latency that
    /// was learned from earlier timestamps, or assumed.
    Estimated = 2,
}

impl ClockMode {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Timestamped,
            2 => Self::Estimated,
            _ => Self::Waiting,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Point {
    position: i64,
    time: i64,
}

pub(crate) struct OutputClock {
    rate: f64,
    started: Option<i64>,
    model: Option<Point>,
    confirmed_at: i64,
    candidate: Option<Point>,
    /// Smoothed time from a callback's start to its first frame being heard, beyond what the
    /// output's buffer accounts for.
    learned_beyond_buffer_ns: Option<f64>,
    mode: ClockMode,
}

impl OutputClock {
    pub(crate) fn new() -> Self {
        Self {
            rate: 48_000.0,
            started: None,
            model: None,
            confirmed_at: 0,
            candidate: None,
            learned_beyond_buffer_ns: None,
            mode: ClockMode::Waiting,
        }
    }

    pub(crate) fn mode(&self) -> ClockMode {
        self.mode
    }

    /// For an output that started again: nothing it reported before counts.
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// When the first frame of this callback will be heard; `None` while waiting for
    /// timestamps to agree.
    pub(crate) fn update(
        &mut self,
        timing: &OutputTiming,
        tuning: &Tuning,
        stats: &Stats,
    ) -> Option<i64> {
        self.rate = f64::from(timing.sample_rate.max(1));
        let now = timing.now.0;
        let started = *self.started.get_or_insert(now);
        if let Some(ts) = timing.timestamp {
            self.observe(
                Point {
                    position: ts.frame_position,
                    time: ts.time.0,
                },
                timing.frames_written,
                now,
                tuning,
                stats,
            );
        }
        let buffer_ns = f64::from(timing.buffer_frames) * 1e9 / self.rate;
        if let Some(model) = self.model
            && now - self.confirmed_at <= tuning.timestamp_stale_ns
        {
            let first = self.time_of(model, timing.frames_written);
            let beyond = first - now as f64 - buffer_ns;
            let learned = self.learned_beyond_buffer_ns.get_or_insert(beyond);
            *learned += 0.05 * (beyond - *learned);
            self.mode = ClockMode::Timestamped;
            return Some(first.round() as i64);
        }
        if self.learned_beyond_buffer_ns.is_some() || now - started >= tuning.timestamp_wait_ns {
            let beyond = self
                .learned_beyond_buffer_ns
                .unwrap_or(tuning.assumed_latency_beyond_buffer_ns as f64);
            self.mode = ClockMode::Estimated;
            return Some((now as f64 + buffer_ns + beyond).round() as i64);
        }
        self.mode = ClockMode::Waiting;
        None
    }

    fn time_of(&self, point: Point, position: i64) -> f64 {
        point.time as f64 + (position - point.position) as f64 * 1e9 / self.rate
    }

    fn agree(&self, earlier: Point, later: Point, tolerance_ns: i64) -> bool {
        (later.time as f64 - self.time_of(earlier, later.position)).abs() <= tolerance_ns as f64
    }

    fn observe(&mut self, point: Point, written: i64, now: i64, tuning: &Tuning, stats: &Stats) {
        // A heard frame was written already, and what is written now is heard after now.
        let latency = self.time_of(point, written) - now as f64;
        if point.position < 0
            || point.position > written
            || latency < -(tuning.timestamp_agree_ns as f64)
            || latency > tuning.max_output_latency_ns as f64
        {
            stats.add(Counter::TimestampsRejected, 1);
            return;
        }
        if let Some(model) = self.model {
            if point.position <= model.position {
                return;
            }
            if self.agree(model, point, tuning.timestamp_step_ns) {
                self.accept(point, now);
                return;
            }
        }
        match self.candidate {
            Some(candidate) if point.position <= candidate.position => {}
            Some(candidate) if self.agree(candidate, point, tuning.timestamp_agree_ns) => {
                self.accept(point, now);
            }
            candidate => {
                if candidate.is_some() || self.model.is_some() {
                    stats.add(Counter::TimestampsRejected, 1);
                }
                self.candidate = Some(point);
            }
        }
    }

    fn accept(&mut self, point: Point, now: i64) {
        self.model = Some(point);
        self.confirmed_at = now;
        self.candidate = None;
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use tether_protocol::ClientTime;

    use super::*;
    use crate::playout::Timestamp;

    const MS: i64 = 1_000_000;

    /// A 48 kHz output whose frame `p` is heard at `20 ms + p / 48 kHz`, called every 4 ms
    /// with 8 ms of output latency.
    fn timing(callback: i64, timestamp: Option<(i64, i64)>) -> OutputTiming {
        let written = callback * 192;
        OutputTiming {
            sample_rate: 48_000,
            channels: 2,
            now: ClientTime(heard(written) - 8 * MS),
            frames_written: written,
            timestamp: timestamp.map(|(p, t)| Timestamp {
                frame_position: p,
                time: ClientTime(t),
            }),
            buffer_frames: 384,
        }
    }

    fn heard(position: i64) -> i64 {
        20 * MS + position * 1_000_000_000 / 48_000
    }

    /// A true timestamp for the frame heard most recently before callback `n`.
    fn truth(callback: i64) -> Option<(i64, i64)> {
        let p = (callback * 192 - 384).max(0);
        Some((p, heard(p)))
    }

    fn run(
        clock: &mut OutputClock,
        callbacks: std::ops::Range<i64>,
        ts: impl Fn(i64) -> Option<(i64, i64)>,
    ) -> Vec<Option<i64>> {
        let stats = Stats::default();
        callbacks
            .map(|n| clock.update(&timing(n, ts(n)), &Tuning::default(), &stats))
            .collect()
    }

    #[test]
    fn waits_for_two_timestamps_that_agree() {
        let mut clock = OutputClock::new();
        let out = run(&mut clock, 1..5, |n| match n {
            1 => Some((0, heard(0) + 3 * MS)),
            _ => truth(n),
        });
        assert_eq!(
            out[..3],
            [None, None, None],
            "a lone or contradicted timestamp is not enough"
        );
        assert_eq!(out[3], Some(heard(4 * 192)));
        assert_eq!(clock.mode(), ClockMode::Timestamped);
    }

    #[test]
    fn ignores_an_outlier_and_follows_a_confirmed_step() {
        let mut clock = OutputClock::new();
        run(&mut clock, 2..10, truth);
        let outlier = run(&mut clock, 10..12, |n| match n {
            10 => truth(n).map(|(p, t)| (p, t + 15 * MS)),
            _ => truth(n),
        });
        assert_eq!(outlier, [Some(heard(10 * 192)), Some(heard(11 * 192))]);
        let step = run(&mut clock, 12..15, |n| {
            truth(n).map(|(p, t)| (p, t + 10 * MS))
        });
        assert_eq!(
            step[0],
            Some(heard(12 * 192)),
            "one stepped timestamp is not trusted"
        );
        assert_eq!(
            step[1..],
            [
                Some(heard(13 * 192) + 10 * MS),
                Some(heard(14 * 192) + 10 * MS)
            ]
        );
    }

    #[test]
    fn rejects_timestamps_that_cannot_be_true() {
        let mut clock = OutputClock::new();
        let out = run(&mut clock, 2..6, |n| {
            truth(n)
                .map(|(p, t)| (p + 100_000, t))
                .filter(|_| n % 2 == 0)
                .or(Some((0, -5_000 * MS)))
        });
        assert!(out.iter().all(Option::is_none));
    }

    #[test]
    fn falls_back_to_the_callback_clock_when_timestamps_never_come() {
        let mut clock = OutputClock::new();
        let wait = Tuning::default().timestamp_wait_ns / (4 * MS);
        let out = run(&mut clock, 1..wait + 3, |_| None);
        assert!(out[..wait as usize - 1].iter().all(Option::is_none));
        let last = timing(wait + 2, None);
        let assumed = 8 * MS + Tuning::default().assumed_latency_beyond_buffer_ns;
        assert_eq!(*out.last().unwrap(), Some(last.now.0 + assumed));
        assert_eq!(clock.mode(), ClockMode::Estimated);
    }

    #[test]
    fn stale_timestamps_hand_over_to_the_learned_latency() {
        let mut clock = OutputClock::new();
        run(&mut clock, 2..100, truth);
        let stale = Tuning::default().timestamp_stale_ns / (4 * MS);
        let out = run(&mut clock, 100..100 + stale + 5, |_| None);
        assert_eq!(clock.mode(), ClockMode::Estimated);
        let n = 100 + stale + 4;
        let error = out.last().unwrap().unwrap() - heard(n * 192);
        assert!(
            error.abs() < 10_000,
            "{error} ns off after losing timestamps"
        );
    }
}
