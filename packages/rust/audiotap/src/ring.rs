//! The capture ring: interleaved stereo `f32` frames plus one stamp per delivered chunk. One
//! producer at a time writes it (the IOProc, the synthetic source, or the control thread once it
//! has retired them) and the [`Packetizer`](crate::Packetizer) drains it. Neither side ever waits
//! for the other: a full ring drops the producer's chunk and counts it.

use std::cell::UnsafeCell;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Channels in every frame the ring holds: the tap is a stereo mixdown.
pub const CHANNELS: usize = 2;

/// Where and when one producer chunk was captured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Stamp {
    /// The producer that wrote it; a capture that restarts gets a new one.
    pub epoch: u32,
    pub frames: u32,
    /// Index of the chunk's first frame in the ring's frame sequence.
    pub position: u64,
    pub sample_rate: u32,
    /// Frames the producer dropped, ring full, since its previous stamp.
    pub dropped_before: u32,
    /// `XrTime` of the chunk's first frame, when the source gave a valid host time.
    pub host_ns: Option<i64>,
    /// The source's sample counter at the chunk's first frame, when valid.
    pub sample_time: Option<f64>,
    /// `XrTime` at which the chunk reached the producer.
    pub arrival_ns: i64,
}

/// What a producer says about a chunk; the ring adds where it goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Chunk {
    pub epoch: u32,
    pub sample_rate: u32,
    pub host_ns: Option<i64>,
    pub sample_time: Option<f64>,
    pub arrival_ns: i64,
}

/// Counters a producer keeps; readable from any thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    pub chunks: u64,
    pub frames: u64,
    /// Frames dropped because the consumer fell a whole ring behind.
    pub dropped_frames: u64,
}

#[repr(align(128))]
struct Line<T>(T);

pub(crate) struct CaptureRing {
    samples: Box<[UnsafeCell<f32>]>,
    frame_mask: u64,
    stamps: Box<[UnsafeCell<Stamp>]>,
    stamp_mask: u64,
    /// Frames and stamps ever published; written only by the producer.
    frames_in: Line<AtomicU64>,
    stamps_in: Line<AtomicU64>,
    /// Frames and stamps ever released; written only by the consumer.
    frames_out: Line<AtomicU64>,
    stamps_out: Line<AtomicU64>,
    pending_drop: AtomicU64,
    chunks: AtomicU64,
    frames: AtomicU64,
    dropped: AtomicU64,
    closed_epoch: AtomicU32,
    bell: Doorbell,
}

// SAFETY: the one producer writes only slots no reader can see (at or past `frames_in` and
// `stamps_in`, inside the space the consumer has released), and the one consumer reads only
// slots below the counts it loaded with Acquire, which the producer published with Release.
unsafe impl Sync for CaptureRing {}

impl CaptureRing {
    /// `frames` is rounded up to a power of two; there is a stamp for every 16 frames, enough for
    /// the smallest IO buffer the HAL allows (15 frames).
    pub fn new(frames: usize) -> CaptureRing {
        let frames = frames.clamp(1024, 1 << 24).next_power_of_two();
        let stamps = frames / 16;
        CaptureRing {
            samples: (0..frames * CHANNELS)
                .map(|_| UnsafeCell::new(0.0))
                .collect(),
            frame_mask: frames as u64 - 1,
            stamps: (0..stamps).map(|_| UnsafeCell::new(EMPTY_STAMP)).collect(),
            stamp_mask: stamps as u64 - 1,
            frames_in: Line(AtomicU64::new(0)),
            stamps_in: Line(AtomicU64::new(0)),
            frames_out: Line(AtomicU64::new(0)),
            stamps_out: Line(AtomicU64::new(0)),
            pending_drop: AtomicU64::new(0),
            chunks: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            closed_epoch: AtomicU32::new(0),
            bell: Doorbell::new(),
        }
    }

    pub fn capacity_frames(&self) -> usize {
        self.frame_mask as usize + 1
    }

    pub fn stats(&self) -> CaptureStats {
        CaptureStats {
            chunks: self.chunks.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped.load(Ordering::Relaxed),
        }
    }

    /// Writes one chunk of `frames` frames. `fill(dst, offset)` writes the chunk's frames from
    /// `offset` on, interleaved, into all of `dst`; it is called once, or twice where the ring
    /// wraps. Returns false, counting the frames as dropped, when the ring has no room.
    /// Real-time safe: no allocation, lock or blocking call.
    ///
    /// # Safety
    /// The caller is the ring's only producer until this returns, and `fill` must not panic.
    pub unsafe fn push(
        &self,
        chunk: Chunk,
        frames: usize,
        mut fill: impl FnMut(&mut [f32], usize),
    ) -> bool {
        if frames == 0 {
            return true;
        }
        let capacity = self.frame_mask + 1;
        let frames_in = self.frames_in.0.load(Ordering::Relaxed);
        let stamps_in = self.stamps_in.0.load(Ordering::Relaxed);
        let used_frames = frames_in.wrapping_sub(self.frames_out.0.load(Ordering::Acquire));
        let used_stamps = stamps_in.wrapping_sub(self.stamps_out.0.load(Ordering::Acquire));
        if frames as u64 > capacity - used_frames.min(capacity) || used_stamps > self.stamp_mask {
            self.pending_drop
                .fetch_add(frames as u64, Ordering::Relaxed);
            self.dropped.fetch_add(frames as u64, Ordering::Relaxed);
            return false;
        }

        let start = (frames_in & self.frame_mask) as usize;
        let first = frames.min(capacity as usize - start);
        // SAFETY: these frames are free: the consumer released them and none is published.
        unsafe {
            fill(self.frames_mut(start, first), 0);
            if first < frames {
                fill(self.frames_mut(0, frames - first), first);
            }
        }
        let dropped_before = self.pending_drop.swap(0, Ordering::Relaxed);
        let stamp = Stamp {
            epoch: chunk.epoch,
            frames: frames as u32,
            position: frames_in,
            sample_rate: chunk.sample_rate,
            dropped_before: dropped_before.min(u64::from(u32::MAX)) as u32,
            host_ns: chunk.host_ns,
            sample_time: chunk.sample_time,
            arrival_ns: chunk.arrival_ns,
        };
        // SAFETY: as above, the slot is past the published count and released by the consumer.
        unsafe { *self.stamps[(stamps_in & self.stamp_mask) as usize].get() = stamp };
        self.frames_in
            .0
            .store(frames_in + frames as u64, Ordering::Release);
        self.stamps_in.0.store(stamps_in + 1, Ordering::Release);
        self.chunks.fetch_add(1, Ordering::Relaxed);
        self.frames.fetch_add(frames as u64, Ordering::Relaxed);
        self.bell.ring();
        true
    }

    /// Says that `epoch`'s producer has retired: nothing more of it will come.
    pub fn close(&self, epoch: u32) {
        self.closed_epoch.store(epoch, Ordering::Release);
        self.bell.ring();
    }

    pub fn closed_epoch(&self) -> u32 {
        self.closed_epoch.load(Ordering::Acquire)
    }

    /// The oldest stamp not yet released.
    ///
    /// # Safety
    /// The caller is the ring's only consumer.
    pub unsafe fn peek(&self) -> Option<Stamp> {
        let out = self.stamps_out.0.load(Ordering::Relaxed);
        if out == self.stamps_in.0.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: published (below stamps_in) and not yet released, so the producer leaves it be.
        Some(unsafe { *self.stamps[(out & self.stamp_mask) as usize].get() })
    }

    /// Releases the stamp [`peek`](Self::peek) returned.
    ///
    /// # Safety
    /// The caller is the ring's only consumer.
    pub unsafe fn release_stamp(&self) {
        let out = self.stamps_out.0.load(Ordering::Relaxed);
        self.stamps_out.0.store(out + 1, Ordering::Release);
    }

    /// Hands `count` published frames from `position` to `read(segment, offset)`, once or twice
    /// where the ring wraps, then releases them to the producer.
    ///
    /// # Safety
    /// The caller is the ring's only consumer, and `position..position + count` lies inside a
    /// stamp it has peeked and not yet read past.
    pub unsafe fn consume(&self, position: u64, count: usize, mut read: impl FnMut(&[f32], usize)) {
        let start = (position & self.frame_mask) as usize;
        let first = count.min(self.capacity_frames() - start);
        // SAFETY: published by the producer's Release store that the stamp's Acquire observed,
        // and not released yet, so the producer does not write them.
        unsafe {
            read(self.frames_ref(start, first), 0);
            if first < count {
                read(self.frames_ref(0, count - first), first);
            }
        }
        self.frames_out
            .0
            .store(position + count as u64, Ordering::Release);
    }

    pub fn rings(&self) -> u32 {
        self.bell.rings()
    }

    /// Sleeps until a producer rings after `seen`, or `timeout` passes. May return early.
    pub fn wait(&self, seen: u32, timeout: Duration) -> bool {
        self.bell.wait(seen, timeout)
    }

    /// # Safety
    /// No reference to these frames may exist, and nobody else may write them meanwhile.
    #[allow(clippy::mut_from_ref)]
    unsafe fn frames_mut(&self, frame: usize, count: usize) -> &mut [f32] {
        // SAFETY: UnsafeCell<f32> has f32's layout and permits writes through a shared borrow;
        // the range lies inside the buffer.
        unsafe {
            let base = UnsafeCell::raw_get(self.samples.as_ptr());
            std::slice::from_raw_parts_mut(base.add(frame * CHANNELS), count * CHANNELS)
        }
    }

    /// # Safety
    /// Nobody may write these frames while the borrow lives.
    unsafe fn frames_ref(&self, frame: usize, count: usize) -> &[f32] {
        // SAFETY: as in frames_mut.
        unsafe {
            let base = UnsafeCell::raw_get(self.samples.as_ptr());
            std::slice::from_raw_parts(base.add(frame * CHANNELS), count * CHANNELS)
        }
    }
}

const EMPTY_STAMP: Stamp = Stamp {
    epoch: 0,
    frames: 0,
    position: 0,
    sample_rate: 0,
    dropped_before: 0,
    host_ns: None,
    sample_time: None,
    arrival_ns: 0,
};

// <os/os_sync_wait_on_address.h> (macOS 14.4+) and <os/clock.h>.
const OS_SYNC_NONE: u32 = 0;
const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;

unsafe extern "C" {
    fn os_sync_wait_on_address_with_timeout(
        addr: *mut c_void,
        value: u64,
        size: usize,
        flags: u32,
        clockid: u32,
        timeout_ns: u64,
    ) -> c_int;
    fn os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> c_int;
}

/// A word the consumer sleeps on. Ringing is an atomic add, plus a wake syscall that never
/// blocks, made only while someone sleeps; the kernel compares the word and queues the sleeper
/// atomically with respect to wakes, so no ring falls between the last check and the sleep.
struct Doorbell {
    rings: AtomicU32,
    sleepers: AtomicU32,
}

impl Doorbell {
    fn new() -> Doorbell {
        Doorbell {
            rings: AtomicU32::new(0),
            sleepers: AtomicU32::new(0),
        }
    }

    fn rings(&self) -> u32 {
        self.rings.load(Ordering::SeqCst)
    }

    /// SeqCst on both sides: either the sleeper sees the new count before it sleeps, or this
    /// sees the sleeper and wakes it.
    fn ring(&self) {
        self.rings.fetch_add(1, Ordering::SeqCst);
        if self.sleepers.load(Ordering::SeqCst) != 0 {
            // SAFETY: a live, 4-byte-aligned word; ENOENT (nobody waiting any more) is fine.
            unsafe { os_sync_wake_by_address_any(self.word(), 4, OS_SYNC_NONE) };
        }
    }

    fn wait(&self, seen: u32, timeout: Duration) -> bool {
        if self.rings() != seen {
            return true;
        }
        self.sleepers.fetch_add(1, Ordering::SeqCst);
        if self.rings() == seen {
            let timeout_ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX).max(1);
            // SAFETY: a live, 4-byte-aligned word. The kernel sleeps only while it still holds
            // `seen`; timeouts, spurious wakes and EINTR all just return.
            unsafe {
                os_sync_wait_on_address_with_timeout(
                    self.word(),
                    u64::from(seen),
                    4,
                    OS_SYNC_NONE,
                    OS_CLOCK_MACH_ABSOLUTE_TIME,
                    timeout_ns,
                )
            };
        }
        self.sleepers.fetch_sub(1, Ordering::SeqCst);
        self.rings() != seen
    }

    fn word(&self) -> *mut c_void {
        self.rings.as_ptr().cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn chunk(epoch: u32) -> Chunk {
        Chunk {
            epoch,
            sample_rate: 48_000,
            host_ns: Some(7),
            sample_time: Some(0.0),
            arrival_ns: 9,
        }
    }

    /// Frame `n` of a test sequence: left = n, right = -n.
    fn push_counting(ring: &CaptureRing, first: u64, frames: usize) -> bool {
        // SAFETY: tests are single-producer.
        unsafe {
            ring.push(chunk(1), frames, |dst, offset| {
                for (i, frame) in dst.chunks_exact_mut(CHANNELS).enumerate() {
                    let n = (first + (offset + i) as u64) as f32;
                    frame[0] = n;
                    frame[1] = -n;
                }
            })
        }
    }

    fn drain(ring: &CaptureRing) -> Vec<(Stamp, Vec<f32>)> {
        let mut out = Vec::new();
        // SAFETY: tests are single-consumer.
        unsafe {
            while let Some(stamp) = ring.peek() {
                let mut frames = vec![0.0; stamp.frames as usize * CHANNELS];
                ring.consume(stamp.position, stamp.frames as usize, |segment, offset| {
                    frames[offset * CHANNELS..offset * CHANNELS + segment.len()]
                        .copy_from_slice(segment);
                });
                ring.release_stamp();
                out.push((stamp, frames));
            }
        }
        out
    }

    #[test]
    fn round_trips_chunks_across_the_wrap() {
        let ring = CaptureRing::new(1024);
        assert_eq!(ring.capacity_frames(), 1024);
        let mut next = 0u64;
        for round in 0..10 {
            let sizes = [300usize, 512, 200];
            for &size in &sizes {
                assert!(push_counting(&ring, next, size), "round {round}");
                next += size as u64;
            }
            let drained = drain(&ring);
            assert_eq!(drained.len(), sizes.len());
            let mut expected = next - sizes.iter().sum::<usize>() as u64;
            for ((stamp, frames), &size) in drained.iter().zip(&sizes) {
                assert_eq!(stamp.frames as usize, size);
                assert_eq!(stamp.position, expected);
                assert_eq!(stamp.host_ns, Some(7));
                for (i, frame) in frames.chunks_exact(CHANNELS).enumerate() {
                    let n = (expected + i as u64) as f32;
                    assert_eq!(frame, [n, -n]);
                }
                expected += size as u64;
            }
        }
    }

    #[test]
    fn a_full_ring_drops_the_newest_chunk_and_says_so() {
        let ring = CaptureRing::new(1024);
        assert!(push_counting(&ring, 0, 1000));
        assert!(!push_counting(&ring, 1000, 100), "only 24 frames free");
        assert!(!push_counting(&ring, 1100, 50));
        assert_eq!(ring.stats().dropped_frames, 150);
        let first = drain(&ring);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0.dropped_before, 0);

        assert!(push_counting(&ring, 1150, 10));
        let second = drain(&ring);
        assert_eq!(
            second[0].0.dropped_before, 150,
            "the next stamp carries the drop"
        );
        assert_eq!(second[0].0.position, 1000);
        assert_eq!(
            ring.stats(),
            CaptureStats {
                chunks: 2,
                frames: 1010,
                dropped_frames: 150
            }
        );
    }

    #[test]
    fn running_out_of_stamps_drops_too() {
        let ring = CaptureRing::new(1024);
        let stamps = 1024 / 16;
        for i in 0..stamps {
            assert!(push_counting(&ring, i as u64, 1));
        }
        assert!(!push_counting(&ring, stamps as u64, 1));
        assert_eq!(drain(&ring).len(), stamps);
        assert!(push_counting(&ring, 0, 1));
    }

    #[test]
    fn chunks_larger_than_the_ring_are_dropped() {
        let ring = CaptureRing::new(1024);
        assert!(!push_counting(&ring, 0, 1025));
        assert!(push_counting(&ring, 0, 1024));
    }

    #[test]
    fn close_marks_the_epoch_and_rings() {
        let ring = CaptureRing::new(1024);
        let seen = ring.rings();
        assert_eq!(ring.closed_epoch(), 0);
        ring.close(3);
        assert_eq!(ring.closed_epoch(), 3);
        assert_ne!(ring.rings(), seen);
    }

    #[test]
    fn concurrent_producer_and_consumer_lose_nothing_silently() {
        let ring = Arc::new(CaptureRing::new(4096));
        let total_chunks = 20_000u64;
        let producer_ring = ring.clone();
        let producer = std::thread::spawn(move || {
            let mut next = 0u64;
            let mut accepted = 0u64;
            for i in 0..total_chunks {
                let size = 1 + (i * 7919 % 700) as usize;
                if push_counting(&producer_ring, next, size) {
                    accepted += size as u64;
                }
                next += size as u64;
                if i % 64 == 0 {
                    std::thread::yield_now();
                }
            }
            (next, accepted)
        });

        let mut seen_frames = 0u64;
        let mut dropped_seen = 0u64;
        let mut next_value = 0u64;
        let mut ring_position = 0u64;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let finished = producer.is_finished();
            for (stamp, frames) in drain(&ring) {
                assert_eq!(stamp.position, ring_position);
                ring_position += u64::from(stamp.frames);
                dropped_seen += u64::from(stamp.dropped_before);
                next_value += u64::from(stamp.dropped_before);
                for frame in frames.chunks_exact(CHANNELS) {
                    assert_eq!(frame[0], next_value as f32);
                    assert_eq!(frame[1], -(next_value as f32));
                    next_value += 1;
                }
                seen_frames += u64::from(stamp.frames);
            }
            // Read before the drain, so a finished producer's last chunk was in it.
            if finished {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        let (produced, accepted) = producer.join().unwrap();
        assert_eq!(seen_frames, accepted);
        let stats = ring.stats();
        assert_eq!(stats.frames, accepted);
        assert_eq!(stats.dropped_frames, produced - accepted);
        // Drops after the last accepted chunk have no later stamp to carry them.
        assert!(dropped_seen <= stats.dropped_frames);
    }

    #[test]
    fn wait_wakes_on_a_push_from_another_thread() {
        let ring = Arc::new(CaptureRing::new(1024));
        let seen = ring.rings();
        let producer_ring = ring.clone();
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            push_counting(&producer_ring, 0, 10)
        });
        let start = Instant::now();
        let mut woke = false;
        while !woke && start.elapsed() < Duration::from_secs(5) {
            woke = ring.wait(seen, Duration::from_secs(5));
        }
        assert!(woke);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "woken by the ring, not the timeout"
        );
        assert!(producer.join().unwrap());
    }

    #[test]
    fn wait_times_out_when_nothing_rings() {
        let ring = CaptureRing::new(1024);
        let start = Instant::now();
        let seen = ring.rings();
        let mut woke = false;
        while start.elapsed() < Duration::from_millis(30) {
            woke |= ring.wait(seen, Duration::from_millis(30));
        }
        assert!(!woke);
    }
}
