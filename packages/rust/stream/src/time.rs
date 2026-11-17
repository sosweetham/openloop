unsafe extern "C" {
    // <time.h>, macOS 10.12+.
    fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
}

/// The Mac's monotonic clock in nanoseconds; the domain of every `XrTime` Tether hands out.
pub fn now_ns() -> i64 {
    // SAFETY: CLOCK_UPTIME_RAW is always available on macOS.
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) as i64 }
}

/// Maps between the headset's clock and the Mac's from Mac-initiated pings. The estimate from
/// the fastest recent round trip wins, because queueing only ever adds delay.
#[derive(Default)]
pub struct ClockSync {
    best: Option<(i64, i64)>, // (round trip, client - mac)
    samples: u32,
}

impl ClockSync {
    /// A pong for the ping sent at `mac_sent`, stamped `client_at` by the headset and
    /// received back at `mac_received`.
    pub fn observe(&mut self, mac_sent: i64, client_at: i64, mac_received: i64) {
        let round_trip = (mac_received - mac_sent).max(0);
        let offset = client_at - (mac_sent + round_trip / 2);
        // Periodically accept a fresh sample so drift between the clocks is followed.
        self.samples += 1;
        let refresh = self.samples.is_multiple_of(64);
        if refresh || self.best.is_none_or(|(rt, _)| round_trip <= rt) {
            self.best = Some((round_trip, offset));
        }
    }

    pub fn is_synced(&self) -> bool {
        self.best.is_some()
    }

    /// `client - mac`, in nanoseconds; 0 before the first pong.
    pub fn offset_ns(&self) -> i64 {
        self.offset()
    }

    /// The round trip the current offset came from: the offset is good to half of it.
    pub fn round_trip_ns(&self) -> Option<i64> {
        self.best.map(|(round_trip, _)| round_trip)
    }

    pub fn to_mac(&self, client_ns: i64) -> i64 {
        client_ns - self.offset()
    }

    pub fn to_client(&self, mac_ns: i64) -> i64 {
        mac_ns + self.offset()
    }

    fn offset(&self) -> i64 {
        self.best.map_or(0, |(_, offset)| offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_the_fastest_round_trip() {
        let mut sync = ClockSync::default();
        // True relation: client = mac + 1000.
        sync.observe(0, 1000 + 80, 100); // slow and asymmetric
        sync.observe(200, 1000 + 205, 210); // fast
        assert_eq!(sync.to_mac(1500), 500);
        assert_eq!(sync.to_client(500), 1500);
    }
}
