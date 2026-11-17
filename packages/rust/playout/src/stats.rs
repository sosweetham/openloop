//! What the playout publishes for the session report: counters and gauges written with relaxed
//! atomics from the audio callback and the network thread, read as one snapshot anywhere.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering::Relaxed};

use crate::clock::ClockMode;

#[derive(Clone, Copy)]
pub(crate) enum Counter {
    Packets,
    Lost,
    Reordered,
    Late,
    Stale,
    Evicted,
    Dropped,
    Rejected,
    Overflow,
    Dropouts,
    SilentFrames,
    Resyncs,
    Streams,
    OutputRestarts,
    Callbacks,
    TimestampsRejected,
}

const COUNTERS: usize = Counter::TimestampsRejected as usize + 1;

#[derive(Clone, Copy)]
pub(crate) enum Gauge {
    ErrorNs,
    TrimPpb,
    BufferedNs,
    ExtraDelayNs,
    OutputLatencyNs,
}

const GAUGES: usize = Gauge::OutputLatencyNs as usize + 1;

#[derive(Default)]
pub struct Stats {
    counters: [AtomicU64; COUNTERS],
    gauges: [AtomicI64; GAUGES],
    clock: AtomicU8,
    network_delay_capped: AtomicBool,
}

impl Stats {
    pub(crate) fn add(&self, counter: Counter, n: u64) {
        if n > 0 {
            self.counters[counter as usize].fetch_add(n, Relaxed);
        }
    }

    pub(crate) fn set(&self, gauge: Gauge, value: i64) {
        self.gauges[gauge as usize].store(value, Relaxed);
    }

    pub(crate) fn set_clock(&self, mode: ClockMode) {
        self.clock.store(mode as u8, Relaxed);
    }

    pub(crate) fn set_network_delay_capped(&self, capped: bool) {
        self.network_delay_capped.store(capped, Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let count = |c: Counter| self.counters[c as usize].load(Relaxed);
        let gauge = |g: Gauge| self.gauges[g as usize].load(Relaxed);
        Snapshot {
            packets: count(Counter::Packets),
            lost: count(Counter::Lost),
            reordered: count(Counter::Reordered),
            late: count(Counter::Late),
            stale: count(Counter::Stale),
            evicted: count(Counter::Evicted),
            dropped: count(Counter::Dropped),
            rejected: count(Counter::Rejected),
            overflow: count(Counter::Overflow),
            dropouts: count(Counter::Dropouts),
            silent_frames: count(Counter::SilentFrames),
            resyncs: count(Counter::Resyncs),
            streams: count(Counter::Streams),
            output_restarts: count(Counter::OutputRestarts),
            callbacks: count(Counter::Callbacks),
            timestamps_rejected: count(Counter::TimestampsRejected),
            error_ns: gauge(Gauge::ErrorNs),
            trim_ppb: gauge(Gauge::TrimPpb),
            buffered_ns: gauge(Gauge::BufferedNs),
            extra_delay_ns: gauge(Gauge::ExtraDelayNs),
            output_latency_ns: gauge(Gauge::OutputLatencyNs),
            clock: ClockMode::from_u8(self.clock.load(Relaxed)),
            network_delay_capped: self.network_delay_capped.load(Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Audio packets the callback took from the ring.
    pub packets: u64,
    /// Sequence numbers skipped when first seen.
    pub lost: u64,
    /// Packets that arrived behind a later one of their stream.
    pub reordered: u64,
    /// Packets whose frames were all due before they arrived; dropped, never played late.
    pub late: u64,
    /// Packets that disagreed with the schedule and were not confirmed by the next packet.
    pub stale: u64,
    /// Packets evicted unread from a full ring.
    pub evicted: u64,
    /// Packets dropped because the audio callback was reading the slot they needed.
    pub dropped: u64,
    pub rejected: u64,
    /// Packets too far ahead of the read position to store.
    pub overflow: u64,
    /// Fades for missing audio after the stream had become audible.
    pub dropouts: u64,
    /// Output frames muted for missing audio after the stream had become audible.
    pub silent_frames: u64,
    /// Schedule jumps taken with a crossfade instead of the speed trim.
    pub resyncs: u64,
    /// Schedules started: new streams, and restarts or index jumps within one.
    pub streams: u64,
    /// The output stream stopped and started again (reopened, paused, or its frame count reset).
    pub output_restarts: u64,
    pub callbacks: u64,
    /// Output timestamps ignored as implausible or unconfirmed.
    pub timestamps_rejected: u64,
    /// Scheduled minus actual presentation time of the frame being played, filtered.
    pub error_ns: i64,
    /// Speed trim, parts per billion.
    pub trim_ppb: i64,
    /// How far the newest received frame is ahead of the read position.
    pub buffered_ns: i64,
    /// Added to every packet's `present_at`: how much later than asked the audio is heard,
    /// the A/V offset playout adds. Never hidden: it is the price of not going silent.
    pub extra_delay_ns: i64,
    /// From a callback's start until its first frame is heard.
    pub output_latency_ns: i64,
    pub clock: ClockMode,
    /// Network lateness beyond `Tuning::max_network_delay_ns` is being dropped, not delayed.
    pub network_delay_capped: bool,
}
