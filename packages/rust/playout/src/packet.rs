//! The network thread's side of the playout: each received packet is copied into a fixed slot
//! of the ring to the audio callback, stamped with when it arrived.

use std::sync::Arc;

use tether_protocol::{
    AudioHeader, AudioPacket, AudioPacketRef, ClientTime, MAX_AUDIO_CHANNELS, MAX_AUDIO_FRAMES,
};

use crate::ring::{Producer, Push};
use crate::stats::{Counter, Stats};

/// Frames one slot holds. A longer packet takes several.
pub(crate) const SLOT_FRAMES: usize = MAX_AUDIO_FRAMES;
/// Channels kept per frame; a wider source contributes its first two.
pub(crate) const SLOT_CHANNELS: usize = MAX_AUDIO_CHANNELS as usize;
const RATES: std::ops::RangeInclusive<u32> = 8_000..=384_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Kind {
    #[default]
    Audio,
    /// No more packets of `stream` will come.
    Stop,
}

#[derive(Clone)]
pub(crate) struct Slot {
    pub(crate) kind: Kind,
    pub(crate) stream: u16,
    pub(crate) sequence: u32,
    pub(crate) first_frame: u64,
    pub(crate) present_at: i64,
    pub(crate) arrived: i64,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u8,
    pub(crate) frames: u16,
    pub(crate) samples: [i16; SLOT_FRAMES * SLOT_CHANNELS],
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            kind: Kind::Audio,
            stream: 0,
            sequence: 0,
            first_frame: 0,
            present_at: 0,
            arrived: 0,
            sample_rate: 0,
            channels: 0,
            frames: 0,
            samples: [0; SLOT_FRAMES * SLOT_CHANNELS],
        }
    }
}

impl Slot {
    pub(crate) fn pcm(&self) -> &[i16] {
        &self.samples[..usize::from(self.frames) * usize::from(self.channels)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    Queued,
    /// Queued after evicting the oldest queued packet. The ring only fills while the audio
    /// callback is not draining it, so the oldest packet is the one already late.
    QueuedEvictingOldest,
    /// Dropped: the one slot that could be reused was being read by the audio callback.
    Dropped,
    /// Unplayable: no channels, partial frames or an implausible sample rate.
    Rejected,
}

/// Owned by the thread that receives packets: one writer per playout.
pub struct PacketWriter {
    ring: Producer<Slot>,
    stats: Arc<Stats>,
}

impl PacketWriter {
    pub(crate) fn new(ring: Producer<Slot>, stats: Arc<Stats>) -> Self {
        Self { ring, stats }
    }

    /// A packet read in place from the received bytes, so nothing is allocated on the way.
    pub fn push_ref(&mut self, packet: &AudioPacketRef<'_>, arrived: ClientTime) -> Pushed {
        self.push(&packet.header, packet.samples(), arrived)
    }

    pub fn push_packet(&mut self, packet: &AudioPacket, arrived: ClientTime) -> Pushed {
        self.push(&packet.header, packet.samples.iter().copied(), arrived)
    }

    /// `samples` are interleaved, `header.format.channels` per frame. `arrived` is when the
    /// packet was received, on the clock of `present_at`.
    pub fn push(
        &mut self,
        header: &AudioHeader,
        mut samples: impl ExactSizeIterator<Item = i16>,
        arrived: ClientTime,
    ) -> Pushed {
        let channels = usize::from(header.format.channels);
        let rate = header.format.sample_rate;
        if channels == 0
            || samples.len() == 0
            || !samples.len().is_multiple_of(channels)
            || !RATES.contains(&rate)
        {
            self.stats.add(Counter::Rejected, 1);
            return Pushed::Rejected;
        }
        let frames = samples.len() / channels;
        let kept = channels.min(SLOT_CHANNELS);
        let mut result = Pushed::Queued;
        for start in (0..frames).step_by(SLOT_FRAMES) {
            let count = (frames - start).min(SLOT_FRAMES);
            let offset_ns = (start as f64 * 1e9 / f64::from(rate)).round() as i64;
            let outcome = self.ring.push_with(|slot| {
                slot.kind = Kind::Audio;
                slot.stream = header.format.stream;
                slot.sequence = header.sequence;
                slot.first_frame = header.first_frame.wrapping_add(start as u64);
                slot.present_at = header.present_at.0.saturating_add(offset_ns);
                slot.arrived = arrived.0;
                slot.sample_rate = rate;
                slot.channels = kept as u8;
                slot.frames = count as u16;
                for frame in slot.samples[..count * kept].chunks_exact_mut(kept) {
                    for (c, sample) in samples.by_ref().take(channels).enumerate() {
                        if let Some(dst) = frame.get_mut(c) {
                            *dst = sample;
                        }
                    }
                }
            });
            match outcome {
                Push::Published => {}
                Push::Evicted => {
                    self.stats.add(Counter::Evicted, 1);
                    if result == Pushed::Queued {
                        result = Pushed::QueuedEvictingOldest;
                    }
                }
                Push::Busy => {
                    self.stats.add(Counter::Dropped, 1);
                    result = Pushed::Dropped;
                    // The frames this chunk would have taken are skipped, not shifted.
                    samples.by_ref().take(count * channels).for_each(drop);
                }
            }
        }
        result
    }

    /// The sender's `AudioStop`: playout ends the stream after its last frame instead of
    /// counting the silence that follows as a dropout.
    pub fn push_stop(&mut self, stream: u16, arrived: ClientTime) -> Pushed {
        let outcome = self.ring.push_with(|slot| {
            slot.kind = Kind::Stop;
            slot.stream = stream;
            slot.arrived = arrived.0;
            slot.frames = 0;
        });
        match outcome {
            Push::Published => Pushed::Queued,
            Push::Evicted => {
                self.stats.add(Counter::Evicted, 1);
                Pushed::QueuedEvictingOldest
            }
            Push::Busy => {
                self.stats.add(Counter::Dropped, 1);
                Pushed::Dropped
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use tether_protocol::{AudioFormat, Message, encode};

    use super::*;
    use crate::ring::{AcquireRelease, Consumer, ring};

    fn writer(slots: usize) -> (PacketWriter, Consumer<Slot>, Arc<Stats>) {
        let (tx, rx) = ring::<Slot, AcquireRelease>(slots);
        let stats = Arc::new(Stats::default());
        (PacketWriter::new(tx, stats.clone()), rx, stats)
    }

    fn header(channels: u8) -> AudioHeader {
        AudioHeader {
            format: AudioFormat {
                stream: 7,
                sample_rate: 48_000,
                channels,
            },
            sequence: 41,
            first_frame: 1_000,
            present_at: ClientTime(5_000_000),
        }
    }

    #[test]
    fn copies_a_packet_read_in_place_from_the_wire() {
        let (mut writer, mut rx, _) = writer(4);
        let samples: Vec<i16> = (0..480).map(|i| (i * 67 - 16_000) as i16).collect();
        let mut bytes = Vec::new();
        let message = Message::Audio(AudioPacket {
            header: header(2),
            samples: samples.clone(),
        });
        encode(&message, &mut bytes);
        let packet = AudioPacketRef::decode(&bytes).unwrap().unwrap();
        assert_eq!(
            writer.push_ref(&packet, ClientTime(4_000_000)),
            Pushed::Queued
        );
        let slot = rx.pop_with(Slot::clone).unwrap();
        assert_eq!(
            (
                slot.stream,
                slot.sequence,
                slot.first_frame,
                slot.present_at
            ),
            (7, 41, 1_000, 5_000_000)
        );
        assert_eq!(
            (slot.arrived, slot.frames, slot.channels),
            (4_000_000, 240, 2)
        );
        assert_eq!(slot.pcm(), &samples[..]);
    }

    #[test]
    fn splits_long_packets_and_keeps_two_channels() {
        let (mut writer, mut rx, _) = writer(4);
        let frames = SLOT_FRAMES + 100;
        let samples: Vec<i16> = (0..frames * 3).map(|i| i as i16).collect();
        let pushed = writer.push(&header(3), samples.iter().copied(), ClientTime(0));
        assert_eq!(pushed, Pushed::Queued);
        let first = rx.pop_with(Slot::clone).unwrap();
        let second = rx.pop_with(Slot::clone).unwrap();
        assert_eq!((first.frames as usize, first.channels), (SLOT_FRAMES, 2));
        assert_eq!(&first.pcm()[..4], &[0, 1, 3, 4]);
        assert_eq!(second.frames, 100);
        assert_eq!(second.first_frame, 1_000 + SLOT_FRAMES as u64);
        assert_eq!(second.present_at, 5_000_000 + 10_000_000);
        let base = (SLOT_FRAMES * 3) as i16;
        assert_eq!(&second.pcm()[..2], &[base, base + 1]);
    }

    #[test]
    fn rejects_what_cannot_be_played() {
        let (mut writer, mut rx, stats) = writer(4);
        let partial = writer.push(&header(2), [1i16, 2, 3].into_iter(), ClientTime(0));
        let mut silent_rate = header(2);
        silent_rate.format.sample_rate = 0;
        let rate = writer.push(&silent_rate, [0i16; 4].into_iter(), ClientTime(0));
        let empty = writer.push(&header(1), std::iter::empty(), ClientTime(0));
        assert_eq!([partial, rate, empty], [Pushed::Rejected; 3]);
        assert_eq!(stats.snapshot().rejected, 3);
        assert!(rx.pop_with(|_| ()).is_none());
    }

    #[test]
    fn a_full_ring_keeps_the_newest_packets() {
        let (mut writer, mut rx, stats) = writer(2);
        for sequence in 0..5u32 {
            let mut h = header(1);
            h.sequence = sequence;
            writer.push(&h, [0i16; 48].into_iter(), ClientTime(0));
        }
        let kept: Vec<u32> = std::iter::from_fn(|| rx.pop_with(|s| s.sequence)).collect();
        assert_eq!(kept, [3, 4]);
        assert_eq!(stats.snapshot().evicted, 3);
    }
}
