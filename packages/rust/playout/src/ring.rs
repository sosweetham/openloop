//! A fixed ring of reusable slots from one producer to one consumer. Both ends are wait-free
//! and never allocate after construction. A full ring makes room by evicting its oldest slot,
//! so a consumer that fell behind resumes with the newest data rather than the stalest.
//!
//! Each slot carries a sequence number (Vyukov's bounded queue): `index` while it is free for
//! the producer's write of `index`, `index + 1` once that write is published. The consumer and
//! the evicting producer both take slots by moving `tail` with a compare-and-swap, so exactly one
//! of them owns any published slot.

use std::marker::PhantomData;

use crate::sync::{Arc, AtomicUsize, Ordering, UnsafeCell};

/// The orderings that hand a slot from one thread to the other. Only a test swaps in another
/// set, to show the model checker catches a broken hand-over.
pub(crate) trait Handover {
    /// Stores a slot's sequence when giving the slot up.
    const RELEASE: Ordering;
    /// Loads a slot's sequence before taking the slot.
    const ACQUIRE: Ordering;
}

pub(crate) struct AcquireRelease;

impl Handover for AcquireRelease {
    const RELEASE: Ordering = Ordering::Release;
    const ACQUIRE: Ordering = Ordering::Acquire;
}

/// Keeps `tail` off the slots' cache lines: 128 bytes on Apple silicon, 64 on the Quest's cores.
#[repr(align(128))]
struct Padded<T>(T);

struct Slot<T> {
    sequence: AtomicUsize,
    value: UnsafeCell<T>,
}

struct Shared<T> {
    slots: Box<[Slot<T>]>,
    /// The oldest index not yet taken by the consumer or evicted by the producer.
    tail: Padded<AtomicUsize>,
}

// SAFETY: a slot's value is touched by one side at a time. The producer writes index `i` only
// after the slot's sequence reads `i` (the consumer released it, or it was never used) or after
// its own compare-and-swap moved `tail` past `i - capacity` (so the consumer can never take that
// index); the consumer reads index `i` only after the sequence reads `i + 1` (published) and its
// compare-and-swap moved `tail` past `i` (so the producer can no longer evict it).
unsafe impl<T: Send> Send for Shared<T> {}
unsafe impl<T: Send> Sync for Shared<T> {}

pub(crate) struct Producer<T, H = AcquireRelease> {
    shared: Arc<Shared<T>>,
    head: usize,
    handover: PhantomData<H>,
}

pub(crate) struct Consumer<T, H = AcquireRelease> {
    shared: Arc<Shared<T>>,
    handover: PhantomData<H>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Push {
    Published,
    /// Published after evicting the oldest unread slot.
    Evicted,
    /// Nothing written: the only slot that could be reused was being read at that instant.
    Busy,
}

/// `capacity` must be a power of two. Every slot is allocated here and never again.
pub(crate) fn ring<T: Default, H: Handover>(capacity: usize) -> (Producer<T, H>, Consumer<T, H>) {
    assert!(
        capacity.is_power_of_two(),
        "ring capacity must be a power of two"
    );
    let slots = (0..capacity)
        .map(|index| Slot {
            sequence: AtomicUsize::new(index),
            value: UnsafeCell::new(T::default()),
        })
        .collect();
    let shared = Arc::new(Shared {
        slots,
        tail: Padded(AtomicUsize::new(0)),
    });
    (
        Producer {
            shared: shared.clone(),
            head: 0,
            handover: PhantomData,
        },
        Consumer {
            shared,
            handover: PhantomData,
        },
    )
}

impl<T, H: Handover> Producer<T, H> {
    /// Fills the next slot in place and publishes it, evicting the oldest unread slot when the
    /// ring is full.
    pub(crate) fn push_with(&mut self, fill: impl FnOnce(&mut T)) -> Push {
        let capacity = self.shared.slots.len();
        let slot = &self.shared.slots[self.head & (capacity - 1)];
        let mut outcome = Push::Published;
        if slot.sequence.load(H::ACQUIRE) != self.head {
            let oldest = self.head.wrapping_sub(capacity);
            let evicted = self
                .shared
                .tail
                .0
                .compare_exchange(
                    oldest,
                    oldest.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok();
            if evicted {
                outcome = Push::Evicted;
            } else if slot.sequence.load(H::ACQUIRE) != self.head {
                return Push::Busy;
            }
        }
        // SAFETY: see `impl Sync for Shared`. Either the sequence read `head` (released by the
        // consumer) or the compare-and-swap took index `head - capacity` away from it.
        slot.value.with_mut(|value| fill(unsafe { &mut *value }));
        slot.sequence.store(self.head.wrapping_add(1), H::RELEASE);
        self.head = self.head.wrapping_add(1);
        outcome
    }
}

impl<T, H: Handover> Consumer<T, H> {
    pub(crate) fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Reads the oldest published slot in place and hands it back; `None` when empty.
    pub(crate) fn pop_with<R>(&mut self, read: impl FnOnce(&T) -> R) -> Option<R> {
        let capacity = self.capacity();
        let mut tail = self.shared.tail.0.load(Ordering::Relaxed);
        // A lost claim means the producer evicted that slot, once per push it makes, so a bound
        // keeps this wait-free; giving up only defers the slot to the next call.
        for _ in 0..capacity {
            let slot = &self.shared.slots[tail & (capacity - 1)];
            if slot.sequence.load(H::ACQUIRE) != tail.wrapping_add(1) {
                let current = self.shared.tail.0.load(Ordering::Relaxed);
                if current == tail {
                    return None;
                }
                tail = current;
                continue;
            }
            match self.shared.tail.0.compare_exchange(
                tail,
                tail.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // SAFETY: see `impl Sync for Shared`; this thread just claimed index `tail`.
                    let result = slot.value.with(|value| read(unsafe { &*value }));
                    slot.sequence.store(tail.wrapping_add(capacity), H::RELEASE);
                    return Some(result);
                }
                Err(current) => tail = current,
            }
        }
        None
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn push(tx: &mut Producer<u32>, value: u32) -> Push {
        tx.push_with(|slot| *slot = value)
    }

    fn drain(rx: &mut Consumer<u32>) -> Vec<u32> {
        std::iter::from_fn(|| rx.pop_with(|slot| *slot)).collect()
    }

    #[test]
    fn a_full_ring_evicts_its_oldest() {
        let (mut tx, mut rx) = ring::<u32, AcquireRelease>(4);
        for i in 0..4 {
            assert_eq!(push(&mut tx, i), Push::Published);
        }
        assert_eq!(push(&mut tx, 4), Push::Evicted);
        assert_eq!(push(&mut tx, 5), Push::Evicted);
        assert_eq!(drain(&mut rx), [2, 3, 4, 5]);
        assert_eq!(push(&mut tx, 6), Push::Published);
        assert_eq!(rx.pop_with(|slot| *slot), Some(6));
        assert_eq!(rx.pop_with(|slot| *slot), None);
    }

    #[test]
    fn eviction_wraps_many_times() {
        let (mut tx, mut rx) = ring::<u32, AcquireRelease>(2);
        for i in 0..1000 {
            push(&mut tx, i);
            if i % 7 == 0 {
                rx.pop_with(|_| ());
            }
        }
        assert_eq!(drain(&mut rx), [998, 999]);
    }

    #[test]
    fn hands_slots_over_in_order_across_threads_with_evictions() {
        const N: u64 = 2_000_000;
        let (mut tx, mut rx) = ring::<[u64; 8], AcquireRelease>(64);
        let producer = std::thread::spawn(move || {
            let mut evicted = 0u64;
            for next in 0..N {
                loop {
                    match tx.push_with(|slot| *slot = [next; 8]) {
                        Push::Published => break,
                        Push::Evicted => {
                            evicted += 1;
                            break;
                        }
                        Push::Busy => std::hint::spin_loop(),
                    }
                }
            }
            evicted
        });
        let (mut last, mut received) = (None::<u64>, 0u64);
        while last != Some(N - 1) {
            match rx.pop_with(|slot| *slot) {
                Some(slot) => {
                    assert!(slot.iter().all(|&v| v == slot[0]), "torn slot {slot:?}");
                    assert!(last.is_none_or(|l| slot[0] > l), "out of order");
                    last = Some(slot[0]);
                    received += 1;
                }
                None => std::hint::spin_loop(),
            }
        }
        let evicted = producer.join().unwrap();
        assert_eq!(
            received + evicted,
            N,
            "every value is read or evicted, once"
        );
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;

    /// The producer pushes three values into two slots, evicting when full; the consumer must
    /// see whole values, in order, and always the last one. Explores every interleaving (no
    /// preemption bound) and returns how many there were.
    fn hand_over<H: Handover + 'static>() -> usize {
        let explored = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = explored.clone();
        loom::model(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (mut tx, mut rx) = ring::<[u32; 2], H>(2);
            let producer = loom::thread::spawn(move || {
                for i in 1..=3u32 {
                    while tx.push_with(|slot| *slot = [i, i]) == Push::Busy {
                        loom::thread::yield_now();
                    }
                }
            });
            let mut last = 0u32;
            while last < 3 {
                match rx.pop_with(|slot| *slot) {
                    Some([a, b]) => {
                        assert_eq!(a, b, "torn slot");
                        assert!(a > last, "out of order");
                        last = a;
                    }
                    None => loom::thread::yield_now(),
                }
            }
            producer.join().unwrap();
        });
        explored.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn slots_hand_over_without_races() {
        let explored = hand_over::<AcquireRelease>();
        println!("loom explored {explored} interleavings");
        assert!(explored > 10_000, "{explored}");
    }

    /// Negative control: publishing with Relaxed stores lets the consumer read a slot the
    /// producer may still be writing, and the model checker must say so.
    struct RelaxedRelease;

    impl Handover for RelaxedRelease {
        const RELEASE: Ordering = Ordering::Relaxed;
        const ACQUIRE: Ordering = Ordering::Acquire;
    }

    #[test]
    #[should_panic(expected = "Causality violation")]
    fn a_relaxed_hand_over_is_caught() {
        hand_over::<RelaxedRelease>();
    }
}
