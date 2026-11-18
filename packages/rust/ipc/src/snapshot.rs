//! Wait-free single-writer records (docs/ARCHITECTURE.md § Runtime ↔ openloopd).

use std::sync::atomic::{AtomicU64, Ordering, fence};

use crate::pod::{Line, Pod, WordCell};

/// A failed attempt means the writer published `N − 1` newer values and began overwriting the
/// slot during one copy, which takes the reader being descheduled mid-copy. Past this many the
/// reader reports [`ReadError::Contended`] instead of trying again, so a read is bounded.
const READ_ATTEMPTS: u32 = 8;

/// The latest value of a record that one process publishes, in `N` slots written round-robin.
///
/// A seqlock reader retries for as long as its writer keeps writing; here the writer never
/// touches the slot it published last, so a read only fails when `N − 1` publishes land inside
/// one copy. Unlike a classic triple buffer, readers hand nothing back to the writer, so any
/// number of them, in either process, read at once and the writer never looks at them. The
/// slots double as a history of the last `N` values.
#[repr(C, align(128))]
pub struct Snapshots<T: Pod, const N: usize> {
    latest: Line<AtomicU64>,
    slots: [Slot<T>; N],
}

#[repr(C, align(128))]
struct Slot<T: Pod> {
    /// `2·g` once generation `g` is complete here, `2·g − 1` while it is being written.
    seq: AtomicU64,
    value: WordCell<T>,
}

/// The single writer's own count of what it has published. It is never read back from the
/// region, so whatever the other process writes there cannot redirect the writer.
#[derive(Debug)]
pub struct Publisher {
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Versioned<T> {
    /// Counts publishes from 1.
    pub generation: u64,
    pub value: T,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Nothing has been published, or not yet the requested generation.
    Empty,
    /// The requested generation's slot has been reused.
    Overwritten,
    /// The writer overtook every attempt.
    Contended,
}

impl<T: Pod, const N: usize> Snapshots<T, N> {
    const SLOTS: u64 = {
        assert!(N > 0);
        N as u64
    };

    /// The writer's cursor. Exactly one may exist per record, across both processes.
    pub fn publisher(&self) -> Publisher {
        Publisher {
            generation: self.latest.0.load(Ordering::Relaxed),
        }
    }

    /// Wait-free: a fixed sequence of stores that never looks at readers.
    pub fn publish(&self, publisher: &mut Publisher, value: &T) -> u64 {
        let generation = publisher.generation + 1;
        let slot = self.slot(generation);
        slot.seq.store(2 * generation - 1, Ordering::Relaxed);
        // Orders the odd sequence before the payload: a reader that sees any new payload word
        // then sees the slot as being written.
        fence(Ordering::Release);
        slot.value.store(value);
        slot.seq.store(2 * generation, Ordering::Release);
        self.latest.0.store(generation, Ordering::Release);
        publisher.generation = generation;
        generation
    }

    /// The newest generation published, 0 before the first; cheap enough to poll for changes.
    pub fn generation(&self) -> u64 {
        self.latest.0.load(Ordering::Acquire)
    }

    /// Wait-free: at most [`READ_ATTEMPTS`] copies, never a wait on the writer.
    pub fn latest(&self) -> Result<Versioned<T>, ReadError> {
        for _ in 0..READ_ATTEMPTS {
            let generation = self.generation();
            if generation == 0 {
                return Err(ReadError::Empty);
            }
            if let Some(value) = self.read(generation) {
                return Ok(Versioned { generation, value });
            }
        }
        Err(ReadError::Contended)
    }

    /// An older value, while its slot has not been reused.
    pub fn get(&self, generation: u64) -> Result<T, ReadError> {
        let latest = self.generation();
        if generation == 0 || generation > latest {
            return Err(ReadError::Empty);
        }
        if latest - generation >= Self::SLOTS {
            return Err(ReadError::Overwritten);
        }
        // A retry could only find the slot further overwritten, so one attempt decides.
        self.read(generation).ok_or(ReadError::Overwritten)
    }

    fn read(&self, generation: u64) -> Option<T> {
        let slot = self.slot(generation);
        // Wrapping: the generation may come from the other process and be garbage.
        let complete = generation.wrapping_mul(2);
        if slot.seq.load(Ordering::Acquire) != complete {
            return None;
        }
        let value = slot.value.load();
        fence(Ordering::Acquire);
        (slot.seq.load(Ordering::Relaxed) == complete).then_some(value)
    }

    fn slot(&self, generation: u64) -> &Slot<T> {
        &self.slots[(generation % Self::SLOTS) as usize]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::pod::pod;
    use crate::region::Zeroed;
    use crate::testing::Stats;

    pod! {
        /// Every word carries the generation, so a torn copy shows as words that disagree.
        pub struct Stamp {
            pub words: [u64; 48],
        }
    }

    impl Stamp {
        fn of(generation: u64) -> Stamp {
            let mut words = [0; 48];
            for (index, word) in words.iter_mut().enumerate() {
                *word = generation.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ index as u64;
            }
            Stamp { words }
        }

        fn is_whole(&self) -> bool {
            let generation_mix = self.words[0];
            self.words
                .iter()
                .enumerate()
                .all(|(index, &word)| word ^ index as u64 == generation_mix)
        }
    }

    #[test]
    fn reads_the_latest_and_keeps_history() {
        let record = Zeroed::<Snapshots<Stamp, 4>>::new();
        assert_eq!(record.latest(), Err(ReadError::Empty));
        let mut publisher = record.publisher();
        for generation in 1..=6 {
            assert_eq!(
                record.publish(&mut publisher, &Stamp::of(generation)),
                generation
            );
        }
        let latest = record.latest().unwrap();
        assert_eq!(latest.generation, 6);
        assert_eq!(latest.value, Stamp::of(6));
        assert_eq!(record.get(3), Ok(Stamp::of(3)));
        assert_eq!(record.get(2), Err(ReadError::Overwritten));
        assert_eq!(record.get(7), Err(ReadError::Empty));
        assert_eq!(record.get(0), Err(ReadError::Empty));
    }

    #[test]
    fn a_new_publisher_continues_the_generations() {
        let record = Zeroed::<Snapshots<Stamp, 4>>::new();
        record.publish(&mut record.publisher(), &Stamp::of(1));
        record.publish(&mut record.publisher(), &Stamp::of(2));
        assert_eq!(record.latest().unwrap().generation, 2);
    }

    #[test]
    fn garbage_from_the_other_process_never_panics() {
        let record = Zeroed::<Snapshots<Stamp, 4>>::new();
        record.latest.0.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(record.latest(), Err(ReadError::Contended));
        record.slots[3].seq.store(u64::MAX - 1, Ordering::Relaxed);
        assert!(record.latest().is_ok());
    }

    struct Outcome {
        reads: u64,
        contended: u64,
        torn: u64,
        published: u64,
        latency: Stats,
    }

    /// One thread publishes as fast as it can while this one reads for `run`.
    fn read_under_a_continuous_writer<const N: usize>(run: Duration) -> Outcome {
        let record = Arc::new(Zeroed::<Snapshots<Stamp, N>>::new());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (record, stop) = (record.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut publisher = record.publisher();
                let mut generation = 0;
                while !stop.load(Ordering::Relaxed) {
                    generation = record.publish(&mut publisher, &Stamp::of(generation + 1));
                }
                generation
            })
        };
        while record.generation() == 0 {
            std::hint::spin_loop();
        }
        let mut outcome = Outcome {
            reads: 0,
            contended: 0,
            torn: 0,
            published: 0,
            latency: Stats::with_capacity(1 << 20),
        };
        let started = Instant::now();
        while started.elapsed() < run {
            let t0 = Instant::now();
            let result = record.latest();
            let took = t0.elapsed().as_nanos() as u64;
            outcome.reads += 1;
            if outcome.latency.len() < 1 << 20 {
                outcome.latency.push(took);
            }
            match result {
                Ok(read) if !read.value.is_whole() => outcome.torn += 1,
                Ok(_) => {}
                Err(_) => outcome.contended += 1,
            }
        }
        stop.store(true, Ordering::Relaxed);
        outcome.published = writer.join().unwrap();
        outcome
    }

    /// The verifier's finding: under a writer that never pauses, a seqlock (one slot) reader
    /// keeps being overtaken. With eight slots every read lands on its first attempt.
    #[test]
    fn reads_stay_bounded_under_a_continuous_writer() {
        let run = Duration::from_millis(400);
        let mut slots8 = read_under_a_continuous_writer::<8>(run);
        let mut seqlock = read_under_a_continuous_writer::<1>(run);
        eprintln!(
            "8 slots: {} reads, {} contended, {} torn, {} published; {}",
            slots8.reads,
            slots8.contended,
            slots8.torn,
            slots8.published,
            slots8.latency.summary("read"),
        );
        eprintln!(
            "1 slot (seqlock control): {} reads, {} contended, {} torn, {} published; {}",
            seqlock.reads,
            seqlock.contended,
            seqlock.torn,
            seqlock.published,
            seqlock.latency.summary("read"),
        );
        assert_eq!(
            slots8.torn + seqlock.torn,
            0,
            "a read returned a torn value"
        );
        assert_eq!(
            slots8.contended, 0,
            "an 8-slot read was overtaken {READ_ATTEMPTS} times"
        );
        assert!(
            seqlock.contended * 20 > seqlock.reads,
            "the seqlock control was not overtaken, so the writer is not exercising the reader"
        );
    }
}
