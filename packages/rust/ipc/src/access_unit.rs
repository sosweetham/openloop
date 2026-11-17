//! The ring that carries encoded video from the app to `tetherd` while the app still encodes
//! (docs/ROADMAP.md § R2 stages, S1 and S2).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::pod::{Line, WordCell};
use crate::records::AccessUnit;
use crate::ring::PushError;

/// Encoded video access units, app → `tetherd`, in `SLOTS` fixed slots of `SLOT_BYTES`. An
/// access unit takes as many consecutive slots as its bytes need (keyframes are several times a
/// delta frame), wrapping from the last slot to the first. Single producer, single consumer;
/// neither side ever waits.
#[repr(C, align(128))]
pub struct AccessUnitRing<const SLOTS: usize, const SLOT_BYTES: usize> {
    /// Slots ever published; written only by the producer.
    head: Line<AtomicU64>,
    /// Slots ever released; written only by the consumer.
    tail: Line<AtomicU64>,
    /// The header of the access unit that starts at each slot.
    headers: [WordCell<AccessUnit>; SLOTS],
    data: UnsafeCell<[[u8; SLOT_BYTES]; SLOTS]>,
}

// SAFETY: `data` is only written between a successful reserve and its commit, by the single
// producer, in slots the consumer has released; everything else is atomic.
unsafe impl<const SLOTS: usize, const SLOT_BYTES: usize> Sync
    for AccessUnitRing<SLOTS, SLOT_BYTES>
{
}

/// The producer's own copy of `head`, so the consumer cannot steer its writes.
#[derive(Debug)]
pub struct AuProducer {
    head: u64,
}

/// The consumer's own copy of `tail`, so the producer cannot steer its reads.
#[derive(Debug)]
pub struct AuConsumer {
    tail: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopError {
    /// The other process broke the ring's invariants; nothing more from it can be trusted.
    Corrupt,
    /// Nothing was consumed; retry with a buffer of at least `needed` bytes.
    BufferTooSmall { needed: usize },
}

/// Space for one access unit, filled by [`AuWriter::write`] and published by
/// [`AuWriter::commit`]. Dropping it publishes nothing.
pub struct AuWriter<'a, const SLOTS: usize, const SLOT_BYTES: usize> {
    ring: &'a AccessUnitRing<SLOTS, SLOT_BYTES>,
    producer: &'a mut AuProducer,
    reserved: usize,
    written: usize,
}

impl<const SLOTS: usize, const SLOT_BYTES: usize> AccessUnitRing<SLOTS, SLOT_BYTES> {
    const TOTAL: usize = {
        assert!(SLOTS > 0 && SLOT_BYTES > 0);
        assert!(
            SLOTS * SLOT_BYTES <= u32::MAX as usize,
            "AccessUnit::len is a u32"
        );
        SLOTS * SLOT_BYTES
    };

    /// The largest access unit the ring can hold: all of it.
    pub const MAX_BYTES: usize = Self::TOTAL;

    /// Exactly one producer may exist per ring, across both processes.
    pub fn producer(&self) -> AuProducer {
        AuProducer {
            head: self.head.0.load(Ordering::Relaxed),
        }
    }

    /// Exactly one consumer may exist per ring, across both processes.
    pub fn consumer(&self) -> AuConsumer {
        AuConsumer {
            tail: self.tail.0.load(Ordering::Relaxed),
        }
    }

    fn span(len: usize) -> u64 {
        len.div_ceil(SLOT_BYTES).max(1) as u64
    }

    /// Room for an access unit of up to `len` bytes.
    pub fn reserve<'a>(
        &'a self,
        producer: &'a mut AuProducer,
        len: usize,
    ) -> Result<AuWriter<'a, SLOTS, SLOT_BYTES>, PushError> {
        if len > Self::TOTAL {
            return Err(PushError::TooLarge);
        }
        let tail = self.tail.0.load(Ordering::Acquire);
        let used = producer.head.wrapping_sub(tail);
        if used > SLOTS as u64 {
            return Err(PushError::Corrupt);
        }
        if SLOTS as u64 - used < Self::span(len) {
            return Err(PushError::Full);
        }
        Ok(AuWriter {
            ring: self,
            producer,
            reserved: len,
            written: 0,
        })
    }

    /// Copies the oldest access unit into `out` and releases its slots.
    pub fn pop_into(
        &self,
        consumer: &mut AuConsumer,
        out: &mut [u8],
    ) -> Result<Option<AccessUnit>, PopError> {
        let head = self.head.0.load(Ordering::Acquire);
        let available = head.wrapping_sub(consumer.tail);
        if available > SLOTS as u64 {
            return Err(PopError::Corrupt);
        }
        if available == 0 {
            return Ok(None);
        }
        // One copy of the header is all this side ever looks at, whatever the producer does next.
        let unit = self.headers[Self::slot(consumer.tail)].load();
        let len = unit.len as usize;
        let span = Self::span(len);
        if len > Self::TOTAL || span > available {
            return Err(PopError::Corrupt);
        }
        if out.len() < len {
            return Err(PopError::BufferTooSmall { needed: len });
        }
        self.copy_out(consumer.tail, &mut out[..len]);
        consumer.tail = consumer.tail.wrapping_add(span);
        // Release: the copy above happens before the producer may reuse these slots.
        self.tail.0.store(consumer.tail, Ordering::Release);
        Ok(Some(unit))
    }

    fn slot(position: u64) -> usize {
        (position % SLOTS as u64) as usize
    }

    fn base(&self) -> *mut u8 {
        self.data.get().cast()
    }

    /// The access unit starting at slot `start` keeps its bytes contiguous modulo the ring.
    fn offset(start: u64, at: usize) -> usize {
        (Self::slot(start) * SLOT_BYTES + at) % Self::TOTAL
    }

    fn copy_in(&self, start: u64, at: usize, bytes: &[u8]) {
        let offset = Self::offset(start, at);
        let first = bytes.len().min(Self::TOTAL - offset);
        // SAFETY: both ranges lie inside `data`, and the slots belong to the producer until
        // commit. The consumer process could still write here only by breaking the protocol,
        // which can garble its own access units but not this process's memory.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base().add(offset), first);
            std::ptr::copy_nonoverlapping(
                bytes[first..].as_ptr(),
                self.base(),
                bytes.len() - first,
            );
        }
    }

    fn copy_out(&self, start: u64, out: &mut [u8]) {
        let offset = Self::offset(start, 0);
        let first = out.len().min(Self::TOTAL - offset);
        let rest = out.len() - first;
        // SAFETY: both ranges lie inside `data`. A producer that breaks the protocol and keeps
        // writing can only change the bytes copied; no address or length here comes from them.
        unsafe {
            std::ptr::copy_nonoverlapping(self.base().add(offset), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(self.base(), out[first..].as_mut_ptr(), rest);
        }
    }
}

impl<const SLOTS: usize, const SLOT_BYTES: usize> AuWriter<'_, SLOTS, SLOT_BYTES> {
    /// Appends to the access unit; fails with `TooLarge` past what was reserved.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), PushError> {
        if bytes.len() > self.reserved - self.written {
            return Err(PushError::TooLarge);
        }
        self.ring.copy_in(self.producer.head, self.written, bytes);
        self.written += bytes.len();
        Ok(())
    }

    pub fn written(&self) -> usize {
        self.written
    }

    /// Publishes the bytes written so far with `unit` as their header; `unit.len` is set to the
    /// number of bytes written.
    pub fn commit(self, unit: &AccessUnit) {
        let ring = self.ring;
        let head = self.producer.head;
        let unit = AccessUnit {
            len: self.written as u32,
            ..*unit
        };
        ring.headers[AccessUnitRing::<SLOTS, SLOT_BYTES>::slot(head)].store(&unit);
        let next = head.wrapping_add(AccessUnitRing::<SLOTS, SLOT_BYTES>::span(self.written));
        self.producer.head = next;
        ring.head.0.store(next, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::Pod;
    use crate::region::Zeroed;
    use crate::testing::{au_bytes_match, fill_au_bytes};

    type Small = AccessUnitRing<4, 64>;

    fn unit(frame_index: u64) -> AccessUnit {
        AccessUnit {
            frame_index,
            flags: AccessUnit::KEYFRAME,
            ..AccessUnit::zeroed()
        }
    }

    fn push(
        ring: &Small,
        producer: &mut AuProducer,
        frame: u64,
        len: usize,
    ) -> Result<(), PushError> {
        let mut bytes = vec![0; len];
        fill_au_bytes(frame, &mut bytes);
        let mut writer = ring.reserve(producer, len)?;
        let (a, b) = bytes.split_at(len / 3);
        writer.write(a)?;
        writer.write(b)?;
        writer.commit(&unit(frame));
        Ok(())
    }

    fn pop(ring: &Small, consumer: &mut AuConsumer) -> Option<(u64, Vec<u8>)> {
        let mut out = vec![0; Small::MAX_BYTES];
        let unit = ring.pop_into(consumer, &mut out).unwrap()?;
        out.truncate(unit.len as usize);
        Some((unit.frame_index, out))
    }

    #[test]
    fn spans_slots_and_wraps() {
        let ring = Zeroed::<Small>::new();
        let (mut producer, mut consumer) = (ring.producer(), ring.consumer());
        let sizes = [1, 64, 65, 200, 256, 0, 129, 63, 128, 192, 17, 256, 100];
        for (frame, &len) in sizes.iter().enumerate() {
            push(&ring, &mut producer, frame as u64, len).unwrap();
            let (popped, bytes) = pop(&ring, &mut consumer).unwrap();
            assert_eq!(popped, frame as u64);
            assert_eq!(bytes.len(), len);
            assert!(
                au_bytes_match(popped, &bytes),
                "frame {frame} ({len} bytes) came back wrong"
            );
        }
        assert!(pop(&ring, &mut consumer).is_none());
    }

    #[test]
    fn refuses_what_does_not_fit() {
        let ring = Zeroed::<Small>::new();
        let mut producer = ring.producer();
        assert_eq!(
            ring.reserve(&mut producer, 257).err(),
            Some(PushError::TooLarge)
        );
        push(&ring, &mut producer, 0, 130).unwrap();
        assert_eq!(
            ring.reserve(&mut producer, 129).err(),
            Some(PushError::Full)
        );
        let mut writer = ring.reserve(&mut producer, 10).unwrap();
        assert_eq!(writer.write(&[0; 11]), Err(PushError::TooLarge));
    }

    #[test]
    fn an_uncommitted_writer_publishes_nothing() {
        let ring = Zeroed::<Small>::new();
        let (mut producer, mut consumer) = (ring.producer(), ring.consumer());
        {
            let mut writer = ring.reserve(&mut producer, 100).unwrap();
            writer.write(&[1; 100]).unwrap();
        }
        assert!(pop(&ring, &mut consumer).is_none());
        push(&ring, &mut producer, 9, 100).unwrap();
        assert_eq!(pop(&ring, &mut consumer).unwrap().0, 9);
    }

    #[test]
    fn a_short_buffer_consumes_nothing() {
        let ring = Zeroed::<Small>::new();
        let (mut producer, mut consumer) = (ring.producer(), ring.consumer());
        push(&ring, &mut producer, 4, 100).unwrap();
        let mut small = [0; 99];
        assert_eq!(
            ring.pop_into(&mut consumer, &mut small),
            Err(PopError::BufferTooSmall { needed: 100 })
        );
        assert_eq!(pop(&ring, &mut consumer).unwrap().0, 4);
    }

    #[test]
    fn detects_a_corrupt_peer() {
        let ring = Zeroed::<Small>::new();
        let (mut producer, mut consumer) = (ring.producer(), ring.consumer());
        push(&ring, &mut producer, 1, 10).unwrap();
        ring.headers[0].store(&AccessUnit {
            len: 200,
            ..unit(1)
        });
        assert_eq!(
            ring.pop_into(&mut consumer, &mut [0; 256]),
            Err(PopError::Corrupt)
        );
        ring.headers[0].store(&AccessUnit {
            len: u32::MAX,
            ..unit(1)
        });
        assert_eq!(
            ring.pop_into(&mut consumer, &mut [0; 256]),
            Err(PopError::Corrupt)
        );
        ring.head.0.store(9, Ordering::Relaxed);
        assert_eq!(
            ring.pop_into(&mut consumer, &mut [0; 256]),
            Err(PopError::Corrupt)
        );
    }

    #[test]
    fn streams_between_threads() {
        const FRAMES: u64 = 20_000;
        let ring = Arc::new(Zeroed::<Small>::new());
        let producer_ring = ring.clone();
        let producer = std::thread::spawn(move || {
            let mut producer = producer_ring.producer();
            let mut frame = 0;
            while frame < FRAMES {
                let len = (frame as usize * 37) % 257;
                match push(&producer_ring, &mut producer, frame, len) {
                    Ok(()) => frame += 1,
                    Err(PushError::Full) => std::thread::yield_now(),
                    Err(e) => panic!("{e:?}"),
                }
            }
        });
        let mut consumer = ring.consumer();
        let mut expected = 0;
        while expected < FRAMES {
            match pop(&ring, &mut consumer) {
                Some((frame, bytes)) => {
                    assert_eq!(frame, expected);
                    assert_eq!(bytes.len(), (frame as usize * 37) % 257);
                    assert!(au_bytes_match(frame, &bytes));
                    expected += 1;
                }
                None => std::thread::yield_now(),
            }
        }
        producer.join().unwrap();
    }
}
