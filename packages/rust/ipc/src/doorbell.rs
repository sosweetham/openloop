//! A futex-style wakeup word in shared memory (`<os/os_sync_wait_on_address.h>`), and the clock
//! its deadlines are in (docs/ARCHITECTURE.md § Runtime ↔ tetherd).

use std::ffi::{c_int, c_void};
use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// <os/os_sync_wait_on_address.h> (macOS 14.4+) and <os/clock.h>.
const OS_SYNC_WAIT_ON_ADDRESS_SHARED: u32 = 1;
const OS_SYNC_WAKE_BY_ADDRESS_SHARED: u32 = 1;
const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn os_sync_wait_on_address(addr: *mut c_void, value: u64, size: usize, flags: u32) -> c_int;
    fn os_sync_wait_on_address_with_deadline(
        addr: *mut c_void,
        value: u64,
        size: usize,
        flags: u32,
        clockid: u32,
        deadline: u64,
    ) -> c_int;
    fn os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> c_int;
    fn os_sync_wake_by_address_all(addr: *mut c_void, size: usize, flags: u32) -> c_int;
    // <time.h>, macOS 10.12+.
    fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
    // <mach/mach_time.h>.
    fn mach_timebase_info(info: *mut MachTimebase) -> c_int;
}

/// The Mac's monotonic clock in nanoseconds (`CLOCK_UPTIME_RAW`): the `XrTime` domain, and the
/// clock every `*_mac_ns` field in the region is in.
pub fn now_ns() -> i64 {
    // SAFETY: CLOCK_UPTIME_RAW is always available on macOS.
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) as i64 }
}

/// `mach_absolute_time` ticks for a time on [`now_ns`]'s clock, which counts the same instants
/// in nanoseconds (clock_gettime(3)).
fn mach_ticks(ns: i64) -> u64 {
    // `numer << 32 | denom`. Racing first calls store the same value, so a real-time waiter
    // never blocks here the way a once-initializer could.
    static TIMEBASE: AtomicU64 = AtomicU64::new(0);
    let mut packed = TIMEBASE.load(Ordering::Relaxed);
    if packed == 0 {
        let mut timebase = MachTimebase { numer: 1, denom: 1 };
        // SAFETY: a valid out pointer.
        unsafe { mach_timebase_info(&mut timebase) };
        packed = (u64::from(timebase.numer) << 32) | u64::from(timebase.denom.max(1));
        TIMEBASE.store(packed, Ordering::Relaxed);
    }
    let (numer, denom) = ((packed >> 32).max(1), packed & 0xffff_ffff);
    (u128::from(ns.max(0) as u64) * u128::from(denom) / u128::from(numer)) as u64
}

/// A word in shared memory that one side sleeps on until the other rings it.
///
/// Ringing never blocks: it is an atomic add, plus one wake syscall only when someone sleeps.
/// The kernel compares the word and queues the sleeper atomically with respect to wakes, so a
/// ring can never fall between a waiter's last check and its sleep.
#[repr(C, align(128))]
pub struct Doorbell {
    rings: AtomicU32,
    sleepers: AtomicU32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    Rung,
    TimedOut,
}

impl Doorbell {
    /// Pass to [`Doorbell::wait`] as `seen`. Read it before looking for work, so that a ring
    /// arriving after the look still ends the wait.
    pub fn rings(&self) -> u32 {
        self.rings.load(Ordering::SeqCst)
    }

    /// Wakes one sleeper.
    pub fn ring(&self) {
        if self.announce() {
            // SAFETY: the word lives in a MAP_SHARED region; ENOENT (nobody left) is fine.
            unsafe { os_sync_wake_by_address_any(self.word(), 4, OS_SYNC_WAKE_BY_ADDRESS_SHARED) };
        }
    }

    /// Wakes every sleeper.
    pub fn ring_all(&self) {
        if self.announce() {
            // SAFETY: as in `ring`.
            unsafe { os_sync_wake_by_address_all(self.word(), 4, OS_SYNC_WAKE_BY_ADDRESS_SHARED) };
        }
    }

    /// Whether anyone needs waking. SeqCst on both sides: either a waiter sees the new count
    /// before it sleeps, or this sees the waiter's announcement and wakes it.
    fn announce(&self) -> bool {
        self.rings.fetch_add(1, Ordering::SeqCst);
        self.sleepers.load(Ordering::SeqCst) != 0
    }

    /// Sleeps until the bell has rung since `seen`, or until `deadline_ns` on [`now_ns`]'s clock.
    pub fn wait(&self, seen: u32, deadline_ns: Option<i64>) -> io::Result<Wake> {
        loop {
            if self.rings.load(Ordering::SeqCst) != seen {
                return Ok(Wake::Rung);
            }
            if deadline_ns.is_some_and(|deadline| now_ns() >= deadline) {
                return Ok(Wake::TimedOut);
            }
            self.sleepers.fetch_add(1, Ordering::SeqCst);
            let result = if self.rings.load(Ordering::SeqCst) != seen {
                0
            } else {
                // SAFETY: the word lives in a MAP_SHARED region, 4-aligned. The kernel compares it
                // with `seen` and only sleeps while they are equal.
                unsafe {
                    match deadline_ns {
                        None => os_sync_wait_on_address(
                            self.word(),
                            u64::from(seen),
                            4,
                            OS_SYNC_WAIT_ON_ADDRESS_SHARED,
                        ),
                        Some(deadline) => os_sync_wait_on_address_with_deadline(
                            self.word(),
                            u64::from(seen),
                            4,
                            OS_SYNC_WAIT_ON_ADDRESS_SHARED,
                            OS_CLOCK_MACH_ABSOLUTE_TIME,
                            mach_ticks(deadline).max(1),
                        ),
                    }
                }
            };
            let error = (result < 0).then(io::Error::last_os_error);
            self.sleepers.fetch_sub(1, Ordering::SeqCst);
            match error.and_then(|e| e.raw_os_error()) {
                None => {}
                Some(libc::ETIMEDOUT) => return Ok(Wake::TimedOut),
                // Early returns the header says to retry after re-reading the word.
                Some(libc::EINTR | libc::EFAULT | libc::ENOMEM) => {}
                Some(code) => return Err(io::Error::from_raw_os_error(code)),
            }
        }
    }

    fn word(&self) -> *mut c_void {
        self.rings.as_ptr().cast()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::region::Zeroed;
    use crate::testing::Stats;

    #[test]
    fn times_out_at_the_deadline() {
        let bell = Zeroed::<Doorbell>::new();
        let start = now_ns();
        let deadline = start + 5_000_000;
        assert_eq!(
            bell.wait(bell.rings(), Some(deadline)).unwrap(),
            Wake::TimedOut
        );
        let late = now_ns() - deadline;
        assert!(late >= 0, "woke {}us early", -late / 1000);
        assert!(late < 50_000_000, "woke {}ms late", late / 1_000_000);
    }

    #[test]
    fn a_ring_before_the_wait_is_not_lost() {
        let bell = Zeroed::<Doorbell>::new();
        let seen = bell.rings();
        bell.ring();
        assert_eq!(bell.wait(seen, None).unwrap(), Wake::Rung);
    }

    /// Ping-pong between two threads through two bells: every ring must wake its waiter, which
    /// the deadline would otherwise expose as a timeout.
    #[test]
    fn wakes_a_sleeper_every_time() {
        const ROUNDS: u64 = 20_000;
        let bells = Arc::new(Zeroed::<[Doorbell; 2]>::new());
        let sent = Arc::new(AtomicU64::new(0));
        // Read before the first ring can happen, or the echo could count it as already seen.
        let mut seen = bells[0].rings();
        let echo = {
            let (bells, sent) = (bells.clone(), sent.clone());
            std::thread::spawn(move || {
                let mut timeouts = 0;
                let mut latency = Stats::with_capacity(ROUNDS as usize);
                for _ in 0..ROUNDS {
                    let deadline = now_ns() + 1_000_000_000;
                    if bells[0].wait(seen, Some(deadline)).unwrap() == Wake::TimedOut {
                        timeouts += 1;
                    }
                    latency.push((now_ns() - sent.load(Ordering::SeqCst) as i64) as u64);
                    seen = bells[0].rings();
                    bells[1].ring();
                }
                (timeouts, latency)
            })
        };
        let mut round_trip = Stats::with_capacity(ROUNDS as usize);
        for _ in 0..ROUNDS {
            let seen = bells[1].rings();
            let start = now_ns();
            sent.store(start as u64, Ordering::SeqCst);
            bells[0].ring();
            let deadline = start + 1_000_000_000;
            assert_eq!(bells[1].wait(seen, Some(deadline)).unwrap(), Wake::Rung);
            round_trip.push((now_ns() - start) as u64);
        }
        let (timeouts, mut wake) = echo.join().unwrap();
        eprintln!("{}", wake.summary("in-process ring -> waiter running"));
        eprintln!("{}", round_trip.summary("in-process ping-pong round trip"));
        assert_eq!(timeouts, 0);
    }

    #[test]
    fn ringing_with_no_sleeper_is_cheap() {
        let bell = Zeroed::<Doorbell>::new();
        let start = now_ns();
        for _ in 0..100_000 {
            bell.ring();
        }
        let per_ring = (now_ns() - start) / 100_000;
        eprintln!("ring with no sleeper: {per_ring} ns");
        assert_eq!(bell.rings(), 100_000);
        assert!(
            per_ring < 2_000,
            "ringing an idle bell took {per_ring} ns; it should not syscall"
        );
    }
}
