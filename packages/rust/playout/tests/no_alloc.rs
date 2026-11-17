//! The network thread's push and the audio callback's render never allocate, through stream
//! starts and stops, gaps, late packets, a full ring, outlying timestamps, the fallback clock
//! and a reopened output: any allocation while either runs fails this test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tether_playout::{ClientTime, ClockMode, Config, OutputTiming, Timestamp};
use tether_protocol::{AudioFormat, AudioHeader};

struct Counting;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    /// Allocations and frees this thread made while watched.
    static CAUGHT: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    if WATCHING.try_with(Cell::get).unwrap_or(false) {
        let _ = CAUGHT.try_with(|n| n.set(n.get() + 1));
    }
}

// SAFETY: forwards every call to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count();
        // SAFETY: `ptr` came from `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn watched<R>(f: impl FnOnce() -> R) -> R {
    WATCHING.with(|w| w.set(true));
    let result = f();
    WATCHING.with(|w| w.set(false));
    result
}

const MS: i64 = 1_000_000;

#[test]
fn pushing_and_rendering_never_allocate() {
    let (mut writer, playout) = tether_playout::new(Config {
        ring_slots: 16,
        ..Config::default()
    });
    let samples: Vec<i16> = (0..480).map(|i| (i * 37 % 2_000) as i16).collect();
    let mut out = vec![0.0f32; 192 * 2];
    let header = |stream: u16, k: i64| AudioHeader {
        format: AudioFormat {
            stream,
            sample_rate: 48_000,
            channels: 2,
        },
        sequence: k as u32,
        first_frame: (k * 240) as u64,
        present_at: ClientTime(k * 5 * MS + 40 * MS),
    };

    // Packet `k` is captured over `[5k, 5k + 5)` ms and due at `5k + 40` ms; it can be pushed
    // a millisecond after its capture ends. No packet arrives for callbacks 500..560, so the
    // backlog then comes at once: late, and more than the 16-slot ring holds. The stream stops
    // at callback 2_000 and another starts; the output reopens at 3_000. No timestamp comes for
    // the first 600 ms, and every 37th is 15 ms off.
    let (mut written, mut k, mut start) = (0i64, 0i64, 20 * MS);
    for callback in 0..4_000i64 {
        let now = start + written * 1_000_000_000 / 48_000 - 8 * MS;
        let stream = if callback < 2_000 { 1 } else { 2 };
        if callback == 2_000 {
            watched(|| writer.push_stop(1, ClientTime(now)));
        }
        while !(500..560).contains(&callback) && k * 5 * MS + 6 * MS <= now {
            watched(|| writer.push(&header(stream, k), samples.iter().copied(), ClientTime(now)));
            k += 1;
        }
        if callback == 3_000 {
            start = now + 250 * MS;
            written = 0;
            continue;
        }
        let heard = written - 384;
        let outlier = if callback % 37 == 0 { 15 * MS } else { 0 };
        let timestamp = (callback > 150 && heard >= 0).then(|| Timestamp {
            frame_position: heard,
            time: ClientTime(start + heard * 1_000_000_000 / 48_000 + outlier),
        });
        watched(|| {
            playout.render(
                &mut out,
                &OutputTiming {
                    sample_rate: 48_000,
                    channels: 2,
                    now: ClientTime(now),
                    frames_written: written,
                    timestamp,
                    buffer_frames: 384,
                },
            )
        });
        written += 192;
    }

    let stats = playout.stats().snapshot();
    assert_eq!(CAUGHT.with(Cell::get), 0, "{stats:?}");
    assert!(stats.packets > 1_000 && stats.streams >= 2, "{stats:?}");
    assert!(stats.evicted > 0 && stats.late > 0, "{stats:?}");
    assert_eq!(stats.output_restarts, 1);
    assert_eq!(stats.clock, ClockMode::Timestamped);
    assert!(out.iter().any(|&s| s != 0.0), "playing at the end");
}

#[test]
fn the_check_catches_an_allocation() {
    watched(|| drop(std::hint::black_box(Box::new(1u64))));
    assert_eq!(CAUGHT.with(Cell::get), 2, "one allocation and its free");
}
