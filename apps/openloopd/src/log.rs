//! Timestamped lines on stderr, which launchd sends to `~/Library/Logs/OpenLoop/openloopd.log`.

use std::fmt;

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(format_args!($($arg)*)) };
}

pub fn line(args: fmt::Arguments<'_>) {
    eprintln!("{} {args}", local_time());
}

/// `2026-11-17 12:34:56.789` in the Mac's time zone.
pub fn local_time() -> String {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid out pointer; CLOCK_REALTIME always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
    // SAFETY: `tm` is plain data that localtime_r fills.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers; localtime_r is the reentrant form.
    unsafe { libc::localtime_r(&now.tv_sec, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.tv_nsec / 1_000_000
    )
}

/// Seconds since 1970, for reports.
pub fn unix_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// `20261117-123456`, for file names.
pub fn file_stamp() -> String {
    let time = local_time();
    let digits: String = time.chars().filter(char::is_ascii_digit).collect();
    format!("{}-{}", &digits[..8], &digits[8..14])
}
