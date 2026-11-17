//! Where received audio waits to be played: planar samples indexed by frame, each frame's
//! identity kept beside it so a slot reused by a later frame or schedule reads as missing, and
//! the schedule's anchors, each packet's first frame with its presentation time.

use crate::interp::{HALF_TAPS, TAPS};
use crate::packet::SLOT_CHANNELS;

/// Frames held: 1.37 s at 48 kHz.
pub(crate) const CAPACITY: usize = 1 << 16;
const MASK: i64 = CAPACITY as i64 - 1;
const FRAME_BITS: u32 = 48;
const ANCHORS: usize = 512;

pub(crate) struct Frames {
    /// The first `TAPS` frames are mirrored past the end, so every window is contiguous.
    planes: [Box<[f32]>; SLOT_CHANNELS],
    keys: Box<[u64]>,
}

impl Frames {
    pub(crate) fn new() -> Self {
        Self {
            planes: std::array::from_fn(|_| vec![0.0; CAPACITY + TAPS].into_boxed_slice()),
            keys: vec![u64::MAX; CAPACITY].into_boxed_slice(),
        }
    }

    fn key(epoch: u16, frame: i64) -> u64 {
        (u64::from(epoch) << FRAME_BITS) | (frame as u64 & ((1 << FRAME_BITS) - 1))
    }

    pub(crate) fn has(&self, epoch: u16, frame: i64) -> bool {
        frame >= 0 && self.keys[(frame & MASK) as usize] == Self::key(epoch, frame)
    }

    /// `pcm` is interleaved with `channels` (1 or 2) samples per frame; `first >= 0`.
    pub(crate) fn store(&mut self, epoch: u16, first: i64, channels: usize, pcm: &[i16]) {
        for (j, samples) in pcm.chunks_exact(channels).enumerate() {
            let frame = first + j as i64;
            let slot = (frame & MASK) as usize;
            for (c, plane) in self.planes.iter_mut().enumerate() {
                let value = f32::from(samples[c.min(channels - 1)]) * (1.0 / 32768.0);
                plane[slot] = value;
                if slot < TAPS {
                    plane[slot + CAPACITY] = value;
                }
            }
            self.keys[slot] = Self::key(epoch, frame);
        }
    }

    /// The `TAPS` frames of one channel around `base`. Missing frames hold stale samples, so
    /// readers use only windows their gate found complete.
    pub(crate) fn window(&self, channel: usize, base: i64) -> &[f32; TAPS] {
        let start = ((base - (HALF_TAPS as i64 - 1)) & MASK) as usize;
        self.planes[channel][start..]
            .first_chunk::<TAPS>()
            .unwrap_or(&[0.0; TAPS])
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Anchor {
    pub(crate) frame: i64,
    /// When `frame` is due, headset nanoseconds.
    pub(crate) time: i64,
}

/// In frame order.
pub(crate) struct Anchors {
    items: [Anchor; ANCHORS],
    len: usize,
}

impl Anchors {
    pub(crate) fn new() -> Self {
        Self {
            items: [Anchor { frame: 0, time: 0 }; ANCHORS],
            len: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    /// A full set drops its oldest anchor for a newer one.
    pub(crate) fn insert(&mut self, anchor: Anchor) {
        let at = self.items[..self.len].partition_point(|a| a.frame < anchor.frame);
        if at < self.len && self.items[at].frame == anchor.frame {
            return;
        }
        if self.len == ANCHORS {
            if at == 0 {
                return;
            }
            self.items.copy_within(1..at, 0);
            self.items[at - 1] = anchor;
            return;
        }
        self.items.copy_within(at..self.len, at + 1);
        self.items[at] = anchor;
        self.len += 1;
    }

    /// When fractional frame `frame` is due, from the latest anchor at or before it (the first
    /// anchor for a frame before them all).
    pub(crate) fn time_at(&self, frame: f64, rate: f64) -> Option<f64> {
        let anchors = &self.items[..self.len];
        let at = anchors.partition_point(|a| a.frame as f64 <= frame);
        let a = anchors.get(at.saturating_sub(1))?;
        Some(a.time as f64 + (frame - a.frame as f64) * 1e9 / rate)
    }

    /// The fractional frame due at `time`.
    pub(crate) fn frame_at(&self, time: i64, rate: f64) -> Option<f64> {
        let anchors = &self.items[..self.len];
        let at = anchors.partition_point(|a| a.time <= time);
        let a = anchors.get(at.saturating_sub(1))?;
        Some(a.frame as f64 + time.saturating_sub(a.time) as f64 * rate / 1e9)
    }

    /// Drops anchors no reader will look up again, keeping the one covering `frame`.
    pub(crate) fn retire_before(&mut self, frame: f64) {
        let keep_from = self.items[..self.len]
            .partition_point(|a| a.frame as f64 <= frame)
            .saturating_sub(1);
        if keep_from > 0 {
            self.items.copy_within(keep_from..self.len, 0);
            self.len -= keep_from;
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn frames_read_back_only_under_their_own_identity() {
        let mut frames = Frames::new();
        frames.store(3, 10, 2, &[100, -100, 200, -200]);
        assert!(frames.has(3, 10) && frames.has(3, 11));
        assert!(!frames.has(4, 10), "another schedule's frame");
        assert!(
            !frames.has(3, 10 + CAPACITY as i64),
            "a later frame in the same slot"
        );
        assert!(!frames.has(3, 12));
        let window = frames.window(1, 10 + HALF_TAPS as i64 - 1);
        assert_eq!(window[0], -100.0 / 32768.0);
    }

    #[test]
    fn windows_wrap_through_the_mirror() {
        let mut frames = Frames::new();
        let last = CAPACITY as i64 - 4;
        frames.store(0, last, 1, &[1; 8]);
        let window = frames.window(0, last + HALF_TAPS as i64 - 1);
        assert_eq!(window[..8], [1.0 / 32768.0; 8]);
    }

    #[test]
    fn anchors_stay_in_frame_order_and_map_both_ways() {
        let mut anchors = Anchors::new();
        for frame in [480, 0, 240, 240] {
            anchors.insert(Anchor {
                frame,
                time: 1_000_000_000 + frame * 1_000_000_000 / 48_000,
            });
        }
        assert_eq!(anchors.len, 3);
        assert_eq!(anchors.time_at(360.0, 48_000.0), Some(1_007_500_000.0));
        assert_eq!(anchors.frame_at(1_007_500_000, 48_000.0), Some(360.0));
        assert_eq!(anchors.time_at(-48.0, 48_000.0), Some(999_000_000.0));
        anchors.retire_before(300.0);
        assert_eq!(anchors.items[0].frame, 240);
        assert_eq!(anchors.len, 2);
    }

    #[test]
    fn a_full_set_drops_its_oldest() {
        let mut anchors = Anchors::new();
        for frame in 0..ANCHORS as i64 + 10 {
            anchors.insert(Anchor { frame, time: frame });
        }
        assert_eq!(anchors.len, ANCHORS);
        assert_eq!(anchors.items[0].frame, 10);
        assert_eq!(anchors.items[ANCHORS - 1].frame, ANCHORS as i64 + 9);
    }
}
