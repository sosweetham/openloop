//! What runs once per frame or per audio packet never allocates once warmed up: `openloopd`
//! sending video and sound to the headset, and the runtime reading a session's records. Any
//! allocation on the watched thread while they run fails this test.

#![cfg(target_os = "macos")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Read;
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use openloop_ipc::SessionRegion;
use openloop_ipc::records::TrackingSample;
use openloop_protocol::{AudioFormat, AudioHeader, ClientTime, Message, VideoFrame, VideoHeader};
use openloop_stream::{Headset, UsbTransport, shared};

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

#[test]
fn sending_frames_and_sound_and_reading_records_never_allocate() {
    let headset = Headset::detached();
    let transport = UsbTransport::bind_port(0).unwrap();
    let port = transport.port();
    headset.accept(Box::new(transport));
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    thread::spawn(move || {
        let mut sink = vec![0u8; 1 << 20];
        while client.read(&mut sink).is_ok_and(|n| n > 0) {}
    });

    let keyframe = vec![7u8; 2 << 20];
    let header = |frame_index| VideoHeader {
        frame_index,
        display_time_ns: 1_000,
        keyframe: frame_index == 0,
        ..VideoHeader::default()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !headset.send_video(&header(0), &keyframe) {
        assert!(Instant::now() < deadline, "the link never came up");
        thread::sleep(Duration::from_millis(5));
    }
    let samples = vec![3i16; 480];
    let audio = |sequence| AudioHeader {
        format: AudioFormat {
            stream: 1,
            sample_rate: 48_000,
            channels: 2,
        },
        sequence,
        first_frame: u64::from(sequence) * 240,
        present_at: ClientTime(5),
    };
    assert!(headset.send_audio(&audio(0), &samples));

    let region = SessionRegion::create(1, 2, 3).unwrap();
    let mut publisher = region.tracking.publisher();
    region
        .tracking
        .publish(&mut publisher, &TrackingSample::default());

    let caught_before = CAUGHT.with(Cell::get);
    let mut sent = 0;
    watched(|| {
        for index in 1..=180u64 {
            let size = (index as usize * 7_919) % keyframe.len();
            sent += usize::from(headset.send_video(&header(index), &keyframe[..size]));
            sent += usize::from(headset.send_audio(&audio(index as u32), &samples));
            let sample = region.tracking.latest().unwrap().value;
            let tracked = shared::tracked(&sample, true);
            std::hint::black_box(tracked.head_at(index as i64));
            std::hint::black_box(region.heartbeat_mac_ns());
        }
    });
    assert_eq!(sent, 360, "the link stayed up");
    assert_eq!(
        CAUGHT.with(Cell::get) - caught_before,
        0,
        "allocations while sending and reading"
    );

    let caught_before = CAUGHT.with(Cell::get);
    watched(|| {
        let owned = Message::VideoFrame(VideoFrame {
            frame_index: 181,
            display_time_ns: 1_000,
            keyframe: false,
            views: Default::default(),
            data: keyframe[..1_000].to_vec(),
        });
        assert!(headset.send(&owned));
    });
    assert!(
        CAUGHT.with(Cell::get) > caught_before,
        "the control, a frame copied into a message, is caught"
    );
}
