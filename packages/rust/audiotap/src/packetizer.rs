//! Cuts the capture ring's chunks into packets of a fixed number of frames, converted to signed
//! 16-bit PCM at the source's own rate, each stamped with its first frame's capture time
//! (`XrTime`) for tetherd to turn into a headset presentation time. Frame indices follow the
//! source's sample counter, so a skipped IO cycle shows as a gap in `first_frame` rather than a
//! shifted timestamp; anything that breaks that numbering starts a new stream.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crate::convert::f32_to_s16;
use crate::ring::{CHANNELS, CaptureRing, CaptureStats, Stamp};
use crate::time::frames_to_ns;

/// The longest gap kept inside one stream; a longer one restarts it.
const MAX_GAP_SECONDS: u64 = 1;

/// One continuous run of frames. A new `stream` value means the receiver restarts playout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamFormat {
    pub stream: u16,
    /// The source's rate, which is the tap's and not necessarily the headset's.
    pub sample_rate: u32,
    pub channels: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub format: StreamFormat,
    /// +1 per packet within a stream.
    pub sequence: u32,
    /// Index of the first frame in `samples`, counted from the start of the stream.
    pub first_frame: u64,
    /// `XrTime` at which that frame was captured.
    pub host_time_ns: i64,
    /// `XrTime` at which the chunk holding that frame reached the ring.
    pub arrival_ns: i64,
    /// Signed 16-bit PCM, interleaved, [`CHANNELS`] samples per frame.
    pub samples: &'a [i16],
}

impl Packet<'_> {
    pub fn frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    /// A stream begins; its packets follow.
    Start(StreamFormat),
    Packet(Packet<'a>),
    /// No more packets of `stream` will come.
    End { stream: u16 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketizerStats {
    pub streams: u64,
    pub packets: u64,
    pub frames: u64,
    /// Gaps bridged inside a stream, and the frames they skipped.
    pub gaps: u64,
    pub gap_frames: u64,
}

/// The capture ring's only consumer. [`poll`](Self::poll) never allocates, locks or blocks.
pub struct Packetizer {
    ring: Arc<CaptureRing>,
    frames_per_packet: usize,
    samples: Box<[i16]>,
    open: Option<Open>,
    stream: Option<Stream>,
    chunk: Option<Cursor>,
    pending: VecDeque<Action>,
    next_stream: u16,
    stats: PacketizerStats,
}

/// The packet being filled.
struct Open {
    format: StreamFormat,
    sequence: u32,
    first_frame: u64,
    host_ns: i64,
    arrival_ns: i64,
    frames: usize,
}

struct Stream {
    format: StreamFormat,
    epoch: u32,
    next_frame: u64,
    /// The source's sample time at frame 0, so stamps are judged against the frame grid and
    /// jitter in one cannot carry into the next.
    origin_sample_time: Option<f64>,
    next_host_ns: Option<i64>,
    sequence: u32,
}

/// The stamp being read.
struct Cursor {
    stamp: Stamp,
    done: usize,
    first_frame: u64,
    host_ns: i64,
}

#[derive(Clone, Copy)]
enum Action {
    Flush,
    End(u16),
    Start(StreamFormat),
}

enum Continuity {
    Contiguous,
    Gap(u64),
    Restart,
}

impl Packetizer {
    pub(crate) fn new(ring: Arc<CaptureRing>, frames_per_packet: usize, first_stream: u16) -> Packetizer {
        let frames_per_packet = frames_per_packet.clamp(1, ring.capacity_frames() / 4);
        Packetizer {
            ring,
            frames_per_packet,
            samples: vec![0; frames_per_packet * CHANNELS].into_boxed_slice(),
            open: None,
            stream: None,
            chunk: None,
            // Never holds more than a flush, an end and a start.
            pending: VecDeque::with_capacity(4),
            next_stream: first_stream,
            stats: PacketizerStats::default(),
        }
    }

    pub fn frames_per_packet(&self) -> usize {
        self.frames_per_packet
    }

    pub fn stats(&self) -> PacketizerStats {
        self.stats
    }

    pub fn capture_stats(&self) -> CaptureStats {
        self.ring.stats()
    }

    /// The next item, or `None` until the producer writes more. A packet is full except the
    /// last of a stream and the one before a gap.
    pub fn poll(&mut self) -> Option<Item<'_>> {
        loop {
            if let Some(action) = self.pending.pop_front() {
                match action {
                    Action::Flush if self.open.is_some() => return self.emit(),
                    Action::Flush => continue,
                    Action::End(stream) => return Some(Item::End { stream }),
                    Action::Start(format) => return Some(Item::Start(format)),
                }
            }
            if self.open.as_ref().is_some_and(|open| open.frames == self.frames_per_packet) {
                return self.emit();
            }
            match self.chunk.as_ref().map(|c| c.stamp.frames as usize - c.done) {
                Some(0) => self.finish_chunk(),
                Some(_) => self.copy(),
                // SAFETY: the packetizer is the ring's only consumer.
                None => match unsafe { self.ring.peek() } {
                    Some(stamp) => self.begin(stamp),
                    None if self.end_closed_stream() => {}
                    None => return None,
                },
            }
        }
    }

    /// Sleeps until the producer may have written more, or `timeout` passes. May return early.
    pub fn wait(&self, timeout: Duration) -> bool {
        let seen = self.ring.rings();
        if self.has_work() {
            return true;
        }
        self.ring.wait(seen, timeout)
    }

    fn has_work(&self) -> bool {
        !self.pending.is_empty()
            || self.chunk.is_some()
            // SAFETY: the packetizer is the ring's only consumer.
            || unsafe { self.ring.peek() }.is_some()
            || self.stream.as_ref().is_some_and(|s| self.ring.closed_epoch() == s.epoch)
    }

    fn emit(&mut self) -> Option<Item<'_>> {
        let open = self.open.take()?;
        self.stats.packets += 1;
        self.stats.frames += open.frames as u64;
        Some(Item::Packet(Packet {
            format: open.format,
            sequence: open.sequence,
            first_frame: open.first_frame,
            host_time_ns: open.host_ns,
            arrival_ns: open.arrival_ns,
            samples: &self.samples[..open.frames * CHANNELS],
        }))
    }

    fn begin(&mut self, stamp: Stamp) {
        let rate = stamp.sample_rate.max(1);
        let continued = match &self.stream {
            Some(s) if s.epoch == stamp.epoch && s.format.sample_rate == rate => match continuity(s, &stamp) {
                Continuity::Contiguous => Some((0, s.next_frame, s.next_host_ns)),
                Continuity::Gap(gap) => {
                    Some((gap, s.next_frame + gap, s.next_host_ns.map(|t| t + frames_to_ns(gap, rate))))
                }
                Continuity::Restart => None,
            },
            _ => None,
        };
        let (first_frame, host_ns) = match continued {
            Some((gap, first_frame, host_ns)) => {
                if gap > 0 {
                    self.pending.push_back(Action::Flush);
                    self.stats.gaps += 1;
                    self.stats.gap_frames += gap;
                }
                (first_frame, host_ns)
            }
            None => {
                self.restart(stamp.epoch, rate);
                (0, None)
            }
        };
        if let Some(stream) = self.stream.as_mut()
            && stream.origin_sample_time.is_none()
        {
            stream.origin_sample_time = stamp.sample_time.map(|t| t - first_frame as f64);
        }
        let host_ns = stamp
            .host_ns
            .or(host_ns)
            .unwrap_or_else(|| stamp.arrival_ns - frames_to_ns(u64::from(stamp.frames), rate));
        self.chunk = Some(Cursor { stamp, done: 0, first_frame, host_ns });
    }

    fn restart(&mut self, epoch: u32, rate: u32) {
        if let Some(old) = self.stream.take() {
            self.pending.push_back(Action::Flush);
            self.pending.push_back(Action::End(old.format.stream));
        }
        let format = StreamFormat { stream: self.next_stream, sample_rate: rate, channels: CHANNELS as u8 };
        self.next_stream = self.next_stream.wrapping_add(1);
        self.pending.push_back(Action::Start(format));
        self.stream = Some(Stream {
            format,
            epoch,
            next_frame: 0,
            origin_sample_time: None,
            next_host_ns: None,
            sequence: 0,
        });
        self.stats.streams += 1;
    }

    fn copy(&mut self) {
        let (Some(cursor), Some(stream)) = (self.chunk.as_mut(), self.stream.as_mut()) else {
            // `begin` gives every chunk a stream; skipping beats spinning on a broken invariant.
            return self.skip_chunk();
        };
        let rate = stream.format.sample_rate;
        let filled = self.open.as_ref().map_or(0, |open| open.frames);
        let take = (cursor.stamp.frames as usize - cursor.done).min(self.frames_per_packet - filled);
        let open = self.open.get_or_insert_with(|| {
            let open = Open {
                format: stream.format,
                sequence: stream.sequence,
                first_frame: cursor.first_frame + cursor.done as u64,
                host_ns: cursor.host_ns + frames_to_ns(cursor.done as u64, rate),
                arrival_ns: cursor.stamp.arrival_ns,
                frames: 0,
            };
            stream.sequence = stream.sequence.wrapping_add(1);
            open
        });
        let out = &mut self.samples[filled * CHANNELS..(filled + take) * CHANNELS];
        // SAFETY: the only consumer, reading inside the stamp it peeked.
        unsafe {
            self.ring.consume(cursor.stamp.position + cursor.done as u64, take, |segment, offset| {
                f32_to_s16(segment, &mut out[offset * CHANNELS..]);
            });
        }
        cursor.done += take;
        open.frames += take;
    }

    fn skip_chunk(&mut self) {
        if let Some(cursor) = self.chunk.as_mut() {
            let left = cursor.stamp.frames as usize - cursor.done;
            // SAFETY: the only consumer, releasing frames inside the stamp it peeked.
            unsafe { self.ring.consume(cursor.stamp.position + cursor.done as u64, left, |_, _| {}) };
            cursor.done += left;
        }
        self.finish_chunk();
    }

    fn finish_chunk(&mut self) {
        let Some(cursor) = self.chunk.take() else {
            return;
        };
        // SAFETY: the only consumer, releasing the stamp it peeked.
        unsafe { self.ring.release_stamp() };
        if let Some(stream) = self.stream.as_mut() {
            let frames = u64::from(cursor.stamp.frames);
            stream.next_frame = cursor.first_frame + frames;
            stream.next_host_ns = Some(cursor.host_ns + frames_to_ns(frames, stream.format.sample_rate));
        }
    }

    /// Ends the stream once its producer has closed it and every stamp it wrote is read.
    /// Returns whether there is more to do.
    fn end_closed_stream(&mut self) -> bool {
        let Some(stream) = &self.stream else {
            return false;
        };
        if self.ring.closed_epoch() != stream.epoch {
            return false;
        }
        // The close was published after the epoch's last stamp, so this peek sees all of them.
        // SAFETY: the packetizer is the ring's only consumer.
        if unsafe { self.ring.peek() }.is_some() {
            return true;
        }
        let id = stream.format.stream;
        self.stream = None;
        self.pending.push_back(Action::Flush);
        self.pending.push_back(Action::End(id));
        true
    }
}

fn continuity(stream: &Stream, stamp: &Stamp) -> Continuity {
    let gap = match (stamp.sample_time, stream.origin_sample_time) {
        (Some(actual), Some(origin)) => {
            // Sample counters move in whole frames; less than one either way is jitter.
            let delta = actual - (origin + stream.next_frame as f64);
            if !delta.is_finite() || delta <= -1.0 {
                return Continuity::Restart;
            }
            if delta < 1.0 {
                return Continuity::Contiguous;
            }
            delta.round() as u64
        }
        _ => u64::from(stamp.dropped_before),
    };
    match gap {
        0 => Continuity::Contiguous,
        gap if gap > u64::from(stream.format.sample_rate) * MAX_GAP_SECONDS => Continuity::Restart,
        gap => Continuity::Gap(gap),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ring::Chunk;

    /// An owned copy of an [`Item`].
    #[derive(Clone, Debug, PartialEq)]
    pub(crate) enum Owned {
        Start(StreamFormat),
        Packet { stream: u16, sequence: u32, first_frame: u64, host_ns: i64, samples: Vec<i16> },
        End(u16),
    }

    pub(crate) fn drain(packetizer: &mut Packetizer) -> Vec<Owned> {
        let mut out = Vec::new();
        while let Some(item) = packetizer.poll() {
            out.push(match item {
                Item::Start(format) => Owned::Start(format),
                Item::Packet(p) => Owned::Packet {
                    stream: p.format.stream,
                    sequence: p.sequence,
                    first_frame: p.first_frame,
                    host_ns: p.host_time_ns,
                    samples: p.samples.to_vec(),
                },
                Item::End { stream } => Owned::End(stream),
            });
        }
        out
    }

    /// Frame `n` of a test source: left = n / 32768, right = -n / 32768, which the
    /// packetizer turns into n and -n.
    fn push(ring: &CaptureRing, epoch: u32, value: i64, frames: usize, host_ns: Option<i64>, sample_time: Option<f64>) {
        push_at_rate(ring, epoch, 48_000, value, frames, host_ns, sample_time);
    }

    fn push_at_rate(
        ring: &CaptureRing,
        epoch: u32,
        sample_rate: u32,
        value: i64,
        frames: usize,
        host_ns: Option<i64>,
        sample_time: Option<f64>,
    ) {
        let chunk = Chunk { epoch, sample_rate, host_ns, sample_time, arrival_ns: 1_000 };
        // SAFETY: tests are single-producer.
        let pushed = unsafe {
            ring.push(chunk, frames, |dst, offset| {
                for (i, frame) in dst.chunks_exact_mut(CHANNELS).enumerate() {
                    let n = (value + (offset + i) as i64) as f32 / 32768.0;
                    frame[0] = n;
                    frame[1] = -n;
                }
            })
        };
        assert!(pushed);
    }

    fn setup(frames_per_packet: usize) -> (Arc<CaptureRing>, Packetizer) {
        let ring = Arc::new(CaptureRing::new(1 << 14));
        let packetizer = Packetizer::new(ring.clone(), frames_per_packet, 5);
        (ring, packetizer)
    }

    fn packets(items: &[Owned]) -> Vec<(u16, u32, u64, i64, usize)> {
        items
            .iter()
            .filter_map(|item| match item {
                Owned::Packet { stream, sequence, first_frame, host_ns, samples } => {
                    Some((*stream, *sequence, *first_frame, *host_ns, samples.len() / CHANNELS))
                }
                _ => None,
            })
            .collect()
    }

    fn ns(frames: u64) -> i64 {
        frames_to_ns(frames, 48_000)
    }

    #[test]
    fn cuts_odd_chunks_into_exact_packets_with_their_own_times() {
        let (ring, mut p) = setup(240);
        let base = 1_000_000_000;
        let mut frame = 0u64;
        for size in [512usize, 512, 100, 700] {
            push(&ring, 1, frame as i64, size, Some(base + ns(frame)), Some(10_000.0 + frame as f64));
            frame += size as u64;
        }
        let items = drain(&mut p);
        let format = StreamFormat { stream: 5, sample_rate: 48_000, channels: 2 };
        assert_eq!(items[0], Owned::Start(format));
        let got = packets(&items);
        assert_eq!(got.len(), 7, "1824 frames make 7 whole packets; 144 wait for more");
        for (i, &(stream, sequence, first_frame, host_ns, frames)) in got.iter().enumerate() {
            assert_eq!((stream, sequence, frames), (5, i as u32, 240));
            assert_eq!(first_frame, i as u64 * 240);
            assert_near(host_ns, base + ns(first_frame));
        }
        let Owned::Packet { samples, .. } = &items[3] else { panic!() };
        for (i, frame) in samples.chunks_exact(CHANNELS).enumerate() {
            let n = (480 + i) as i16;
            assert_eq!(frame, [n, -n]);
        }
        assert!(p.poll().is_none());

        push(&ring, 1, frame as i64, 96, Some(base + ns(frame)), Some(10_000.0 + frame as f64));
        let more = packets(&drain(&mut p));
        assert_eq!(more.len(), 1);
        assert_eq!((more[0].0, more[0].1, more[0].2, more[0].4), (5, 7, 1680, 240));
        assert_near(more[0].3, base + ns(1680));
        assert_eq!(p.stats().packets, 8);
    }

    /// Host times built from two truncated conversions may differ by a nanosecond.
    fn assert_near(actual: i64, expected: i64) {
        assert!((actual - expected).abs() <= 2, "{actual} vs {expected}");
    }

    #[test]
    fn a_gap_flushes_and_skips_frame_indices_inside_the_stream() {
        let (ring, mut p) = setup(240);
        push(&ring, 1, 0, 300, Some(0), Some(0.0));
        // The source skipped one 512-frame cycle.
        push(&ring, 1, 812, 300, Some(ns(812)), Some(812.0));
        let items = drain(&mut p);
        assert_eq!(
            packets(&items),
            vec![(5, 0, 0, 0, 240), (5, 1, 240, ns(240), 60), (5, 2, 812, ns(812), 240)]
        );
        assert!(!items.iter().any(|i| matches!(i, Owned::End(_))));
        assert_eq!((p.stats().gaps, p.stats().gap_frames), (1, 512));
    }

    #[test]
    fn sample_time_jitter_under_a_frame_is_not_a_gap() {
        let (ring, mut p) = setup(100);
        push(&ring, 1, 0, 100, Some(0), Some(0.0));
        push(&ring, 1, 100, 100, Some(ns(100)), Some(100.4));
        push(&ring, 1, 200, 100, Some(ns(200)), Some(199.7));
        let got = packets(&drain(&mut p));
        assert_eq!(got.iter().map(|p| p.2).collect::<Vec<_>>(), vec![0, 100, 200]);
        assert_eq!(p.stats().gaps, 0);
    }

    #[test]
    fn jitter_never_accumulates_over_a_long_stream() {
        let (ring, mut p) = setup(512);
        let mut got = Vec::new();
        for i in 0..2_000u64 {
            let jitter = [0.3, -0.3, 0.45, -0.45][i as usize % 4];
            push(&ring, 1, (i * 512) as i64 % 30_000, 512, Some(ns(i * 512)), Some(7.0 + (i * 512) as f64 + jitter));
            got.extend(packets(&drain(&mut p)));
        }
        assert_eq!(got.len(), 2_000);
        assert!(got.iter().enumerate().all(|(i, p)| p.0 == 5 && p.2 == i as u64 * 512));
        assert_eq!(p.stats().gaps, 0);
    }

    #[test]
    fn going_backwards_ends_the_stream_and_starts_another() {
        let (ring, mut p) = setup(240);
        push(&ring, 1, 0, 300, Some(0), Some(5000.0));
        push(&ring, 1, 0, 240, Some(ns(400)), Some(10.0));
        let items = drain(&mut p);
        let second = StreamFormat { stream: 6, sample_rate: 48_000, channels: 2 };
        assert_eq!(
            items.iter().map(|i| match i { Owned::Packet { .. } => Owned::End(999), other => other.clone() }).collect::<Vec<_>>(),
            vec![
                Owned::Start(StreamFormat { stream: 5, sample_rate: 48_000, channels: 2 }),
                Owned::End(999),
                Owned::End(999),
                Owned::End(5),
                Owned::Start(second),
                Owned::End(999),
            ]
        );
        assert_eq!(packets(&items)[2], (6, 0, 0, ns(400), 240));
    }

    #[test]
    fn a_gap_longer_than_a_second_restarts() {
        let (ring, mut p) = setup(240);
        push(&ring, 1, 0, 240, Some(0), Some(0.0));
        push(&ring, 1, 0, 240, Some(ns(48_241)), Some(48_241.0));
        let items = drain(&mut p);
        assert!(items.contains(&Owned::End(5)));
        assert_eq!(packets(&items).last().unwrap().0, 6);
    }

    #[test]
    fn a_new_epoch_or_rate_is_a_new_stream() {
        let (ring, mut p) = setup(240);
        push(&ring, 1, 0, 240, Some(0), Some(0.0));
        push(&ring, 2, 0, 240, Some(ns(240)), Some(240.0));
        push_at_rate(&ring, 2, 44_100, 0, 240, Some(ns(480)), Some(480.0));
        let items = drain(&mut p);
        let starts: Vec<_> = items.iter().filter_map(|i| match i { Owned::Start(f) => Some(*f), _ => None }).collect();
        assert_eq!(starts.iter().map(|f| (f.stream, f.sample_rate)).collect::<Vec<_>>(), vec![(5, 48_000), (6, 48_000), (7, 44_100)]);
        let ends: Vec<_> = items.iter().filter_map(|i| match i { Owned::End(s) => Some(*s), _ => None }).collect();
        assert_eq!(ends, vec![5, 6]);
    }

    #[test]
    fn closing_the_epoch_flushes_and_ends_once_everything_is_read() {
        let (ring, mut p) = setup(240);
        push(&ring, 1, 0, 300, Some(0), Some(0.0));
        assert_eq!(packets(&drain(&mut p)).len(), 1);
        ring.close(1);
        let items = drain(&mut p);
        assert_eq!(packets(&items), vec![(5, 1, 240, ns(240), 60)]);
        assert_eq!(items.last(), Some(&Owned::End(5)));
        assert!(drain(&mut p).is_empty(), "ended exactly once");

        push(&ring, 2, 0, 240, Some(ns(10_000)), Some(0.0));
        let items = drain(&mut p);
        assert_eq!(items[0], Owned::Start(StreamFormat { stream: 6, sample_rate: 48_000, channels: 2 }));
    }

    #[test]
    fn a_close_raced_by_unread_stamps_still_delivers_them() {
        let (ring, mut p) = setup(240);
        push(&ring, 1, 0, 240, Some(0), Some(0.0));
        push(&ring, 1, 240, 240, Some(ns(240)), Some(240.0));
        ring.close(1);
        let items = drain(&mut p);
        assert_eq!(packets(&items).len(), 2);
        assert_eq!(items.last(), Some(&Owned::End(5)));
    }

    #[test]
    fn without_sample_times_a_full_ring_shows_as_a_gap_of_the_dropped_frames() {
        let ring = Arc::new(CaptureRing::new(1024));
        let mut p = Packetizer::new(ring.clone(), 100, 5);
        push(&ring, 1, 0, 100, Some(0), None);
        push(&ring, 1, 100, 924, Some(ns(100)), None);
        let chunk = Chunk { epoch: 1, sample_rate: 48_000, host_ns: Some(ns(1024)), sample_time: None, arrival_ns: 0 };
        // SAFETY: tests are single-producer.
        assert!(!unsafe { ring.push(chunk, 50, |_, _| {}) }, "the ring is full");
        let before = packets(&drain(&mut p));
        assert_eq!(before.len(), 10);

        push(&ring, 1, 1074, 100, Some(ns(1074)), None);
        let after = packets(&drain(&mut p));
        assert_eq!(after.iter().map(|p| (p.2, p.4)).collect::<Vec<_>>(), vec![(1000, 24), (1074, 100)]);
        assert_eq!((p.stats().gaps, p.stats().gap_frames), (1, 50));
        assert_eq!(p.capture_stats().dropped_frames, 50);
    }

    #[test]
    fn missing_host_times_are_extrapolated_from_the_previous_chunk() {
        let (ring, mut p) = setup(100);
        push(&ring, 1, 0, 100, Some(5_000), Some(0.0));
        push(&ring, 1, 100, 100, None, Some(100.0));
        push(&ring, 1, 300, 100, None, Some(300.0));
        let got = packets(&drain(&mut p));
        assert_eq!(got.len(), 3);
        for (packet, expected) in got.iter().zip([5_000, 5_000 + ns(100), 5_000 + ns(300)]) {
            assert_near(packet.3, expected);
        }
    }

    #[test]
    fn a_first_chunk_without_host_time_is_placed_before_its_arrival() {
        let (ring, mut p) = setup(100);
        push(&ring, 1, 0, 100, None, None);
        let got = packets(&drain(&mut p));
        assert_eq!(got[0].3, 1_000 - ns(100));
    }

    #[test]
    fn stream_ids_wrap() {
        let ring = Arc::new(CaptureRing::new(1 << 12));
        let mut p = Packetizer::new(ring.clone(), 10, u16::MAX);
        push(&ring, 1, 0, 10, Some(0), Some(0.0));
        push(&ring, 2, 0, 10, Some(0), Some(0.0));
        let starts: Vec<_> = drain(&mut p).into_iter().filter_map(|i| match i { Owned::Start(f) => Some(f.stream), _ => None }).collect();
        assert_eq!(starts, vec![u16::MAX, 0]);
    }

    #[test]
    fn wait_reports_pending_work_without_sleeping() {
        let (ring, p) = setup(10);
        push(&ring, 1, 0, 10, Some(0), Some(0.0));
        let start = std::time::Instant::now();
        assert!(p.wait(Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
