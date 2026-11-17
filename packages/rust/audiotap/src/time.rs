//! The Mac's clocks: `mach_absolute_time` ticks, the unit of `AudioTimeStamp::mHostTime`, and
//! `XrTime`, nanoseconds on `CLOCK_UPTIME_RAW`. clock_gettime(3): CLOCK_UPTIME_RAW "is identical
//! to the result of mach_absolute_time() after the appropriate mach_timebase conversion".

use std::sync::OnceLock;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    // <mach/mach_time.h>
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    // <time.h>, macOS 10.12+.
    fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
}

/// `XrTime` now: nanoseconds on the Mac's monotonic clock.
pub fn now_ns() -> i64 {
    // SAFETY: CLOCK_UPTIME_RAW is always available on macOS.
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) as i64 }
}

pub(crate) fn host_ticks() -> u64 {
    // SAFETY: reads the commpage clock; no preconditions.
    unsafe { mach_absolute_time() }
}

/// Converts host ticks to `XrTime` nanoseconds. Copy it into real-time contexts rather than
/// calling [`Timebase::host`] there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    pub fn host() -> Timebase {
        static HOST: OnceLock<Timebase> = OnceLock::new();
        *HOST.get_or_init(|| {
            let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
            // SAFETY: a valid out pointer.
            let status = unsafe { mach_timebase_info(&mut info) };
            assert!(status == 0 && info.denom != 0, "mach_timebase_info failed: {status}");
            Timebase { numer: info.numer, denom: info.denom }
        })
    }

    /// `denom` must not be 0.
    pub const fn new(numer: u32, denom: u32) -> Timebase {
        Timebase { numer, denom }
    }

    /// Truncates like the kernel's own conversion, so the result equals what `CLOCK_UPTIME_RAW`
    /// reported at that tick.
    pub fn ticks_to_ns(self, ticks: u64) -> i64 {
        let ns = u128::from(ticks) * u128::from(self.numer) / u128::from(self.denom);
        i64::try_from(ns).unwrap_or(i64::MAX)
    }
}

/// How long `frames` last at `rate` frames per second.
pub(crate) fn frames_to_ns(frames: u64, rate: u32) -> i64 {
    let ns = i128::from(frames) * 1_000_000_000 / i128::from(rate.max(1));
    i64::try_from(ns).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_silicon_timebase_truncates() {
        let tb = Timebase::new(125, 3);
        assert_eq!(tb.ticks_to_ns(0), 0);
        assert_eq!(tb.ticks_to_ns(3), 125);
        assert_eq!(tb.ticks_to_ns(1), 41);
        assert_eq!(tb.ticks_to_ns(24_000_000), 1_000_000_000);
    }

    #[test]
    fn huge_tick_counts_do_not_overflow() {
        let tb = Timebase::new(125, 3);
        // About 12 years of uptime at 24 MHz.
        let ticks = 24_000_000u64 * 86_400 * 365 * 12;
        assert_eq!(tb.ticks_to_ns(ticks), (ticks as i128 * 125 / 3) as i64);
        assert_eq!(tb.ticks_to_ns(u64::MAX), i64::MAX);
    }

    #[test]
    fn host_ticks_land_on_uptime_raw() {
        let tb = Timebase::host();
        for _ in 0..1000 {
            let before = now_ns();
            let ticks = host_ticks();
            let after = now_ns();
            let ns = tb.ticks_to_ns(ticks);
            assert!(before <= ns && ns <= after, "{before} <= {ns} <= {after}");
        }
    }

    #[test]
    fn frame_durations() {
        assert_eq!(frames_to_ns(48_000, 48_000), 1_000_000_000);
        assert_eq!(frames_to_ns(240, 48_000), 5_000_000);
        assert_eq!(frames_to_ns(1, 44_100), 22_675);
        assert_eq!(frames_to_ns(5, 0), 5_000_000_000);
    }
}
