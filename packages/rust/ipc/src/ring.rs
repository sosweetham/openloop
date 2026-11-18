//! Fixed-slot single-producer, single-consumer rings (docs/ARCHITECTURE.md § Runtime ↔ openloopd).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::pod::{Line, Pod, WordCell};

/// A fixed-capacity single-producer, single-consumer queue of `T` in shared memory. Neither
/// side ever waits: a full ring refuses the push and an empty one returns nothing.
#[repr(C, align(128))]
pub struct SpscRing<T: Pod, const N: usize> {
    /// Items ever pushed; written only by the producer.
    head: Line<AtomicU64>,
    /// Items ever popped; written only by the consumer.
    tail: Line<AtomicU64>,
    slots: [WordCell<T>; N],
}

/// The producer's own copy of `head`, so the consumer cannot steer its writes.
#[derive(Debug)]
pub struct Producer {
    head: u64,
}

/// The consumer's own copy of `tail`, so the producer cannot steer its reads.
#[derive(Debug)]
pub struct Consumer {
    tail: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    Full,
    /// More than the ring can ever hold, or more than was reserved.
    TooLarge,
    /// The other process broke the ring's invariants; nothing more from it can be trusted.
    Corrupt,
}

/// The other process broke the ring's invariants; nothing more from it can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Corrupt;

impl<T: Pod, const N: usize> SpscRing<T, N> {
    const CAPACITY: u64 = {
        assert!(N.is_power_of_two(), "ring capacity must be a power of two");
        N as u64
    };

    /// Exactly one producer may exist per ring, across both processes.
    pub fn producer(&self) -> Producer {
        Producer {
            head: self.head.0.load(Ordering::Relaxed),
        }
    }

    /// Exactly one consumer may exist per ring, across both processes.
    pub fn consumer(&self) -> Consumer {
        Consumer {
            tail: self.tail.0.load(Ordering::Relaxed),
        }
    }

    pub fn push(&self, producer: &mut Producer, value: &T) -> Result<(), PushError> {
        let tail = self.tail.0.load(Ordering::Acquire);
        let used = producer.head.wrapping_sub(tail);
        if used > Self::CAPACITY {
            return Err(PushError::Corrupt);
        }
        if used == Self::CAPACITY {
            return Err(PushError::Full);
        }
        self.slots[(producer.head % Self::CAPACITY) as usize].store(value);
        producer.head = producer.head.wrapping_add(1);
        self.head.0.store(producer.head, Ordering::Release);
        Ok(())
    }

    pub fn pop(&self, consumer: &mut Consumer) -> Result<Option<T>, Corrupt> {
        let head = self.head.0.load(Ordering::Acquire);
        let available = head.wrapping_sub(consumer.tail);
        if available > Self::CAPACITY {
            return Err(Corrupt);
        }
        if available == 0 {
            return Ok(None);
        }
        let value = self.slots[(consumer.tail % Self::CAPACITY) as usize].load();
        consumer.tail = consumer.tail.wrapping_add(1);
        // Release: the slot's loads above happen before the producer may reuse it.
        self.tail.0.store(consumer.tail, Ordering::Release);
        Ok(Some(value))
    }

    /// Items waiting, as last seen; only a hint, since the producer keeps pushing.
    pub fn len(&self) -> u64 {
        let head = self.head.0.load(Ordering::Acquire);
        head.wrapping_sub(self.tail.0.load(Ordering::Acquire))
            .min(Self::CAPACITY)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::region::Zeroed;

    #[test]
    fn fills_drains_and_wraps() {
        let ring = Zeroed::<SpscRing<u64, 8>>::new();
        let (mut producer, mut consumer) = (ring.producer(), ring.consumer());
        let mut next_in = 0u64;
        let mut next_out = 0u64;
        for round in 0..100 {
            let batch = 1 + round % 8;
            for _ in 0..batch {
                ring.push(&mut producer, &next_in).unwrap();
                next_in += 1;
            }
            if batch == 8 {
                assert_eq!(ring.push(&mut producer, &next_in), Err(PushError::Full));
            }
            assert_eq!(ring.len(), batch);
            while let Some(value) = ring.pop(&mut consumer).unwrap() {
                assert_eq!(value, next_out);
                next_out += 1;
            }
        }
        assert_eq!(next_out, next_in);
        assert!(next_in > 8 * 50, "the ring wrapped many times");
    }

    #[test]
    fn streams_in_order_between_threads() {
        const ITEMS: u64 = 1_000_000;
        let ring = Arc::new(Zeroed::<SpscRing<[u64; 3], 64>>::new());
        let producer_ring = ring.clone();
        let producer = std::thread::spawn(move || {
            let mut producer = producer_ring.producer();
            let mut index = 0;
            while index < ITEMS {
                match producer_ring.push(&mut producer, &[index, !index, index * 3]) {
                    Ok(()) => index += 1,
                    Err(PushError::Full) => std::hint::spin_loop(),
                    Err(e) => panic!("{e:?}"),
                }
            }
        });
        let mut consumer = ring.consumer();
        let mut expected = 0;
        while expected < ITEMS {
            if let Some(item) = ring.pop(&mut consumer).unwrap() {
                assert_eq!(item, [expected, !expected, expected * 3]);
                expected += 1;
            }
        }
        producer.join().unwrap();
        assert!(ring.is_empty());
    }

    #[test]
    fn detects_a_corrupt_peer() {
        let ring = Zeroed::<SpscRing<u64, 8>>::new();
        let (mut producer, mut consumer) = (ring.producer(), ring.consumer());
        ring.push(&mut producer, &1).unwrap();
        ring.head.0.store(100, Ordering::Relaxed);
        assert_eq!(ring.pop(&mut consumer), Err(Corrupt));
        ring.tail.0.store(50, Ordering::Relaxed);
        assert_eq!(ring.push(&mut producer, &2), Err(PushError::Corrupt));
    }
}
