//! Encodes on this Mac's real media engine at the headset's frame rate, and holds the
//! encode-latency quality gate from `docs/ROADMAP.md`. The ignored matrix prints how codec,
//! input format and rate control trade latency.
#![cfg(target_os = "macos")]

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_metal::{
    MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor,
    MTLTextureType, MTLTextureUsage,
};
use tether_stream::compositor::{Compositor, EyeSource, iosurface_pixel_buffer};
use tether_stream::encoder::{Codec, Encoder, EncoderConfig, Pending};
use tether_stream::time::now_ns;

/// Quest 3's recommended per-eye render size.
const APP_EYE: (u32, u32) = (1680, 1760);
const FPS: f32 = 90.0;
const FRAMES: usize = 180;
/// The encode scale `tetherd` uses by default.
const SCALE: f32 = 1.0;

fn percentile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * q) as usize]
}

fn even(v: f32) -> u32 {
    (v as u32) & !1
}

struct Run {
    p50: f64,
    p95: f64,
    skipped: usize,
    keyframes: Vec<usize>,
}

/// The production path: app eye texture → 4:2:0 compositor → HEVC, paced at the headset rate.
fn compose_and_encode(scale: f32) -> Run {
    let device = MTLCreateSystemDefaultDevice().expect("a Metal device");
    let queue = device.newCommandQueue().expect("a command queue");
    let descriptor = MTLTextureDescriptor::new();
    descriptor.setTextureType(MTLTextureType::Type2DArray);
    descriptor.setPixelFormat(MTLPixelFormat::RGBA8Unorm_sRGB);
    // SAFETY: plain descriptor configuration.
    unsafe {
        descriptor.setWidth(APP_EYE.0 as usize);
        descriptor.setHeight(APP_EYE.1 as usize);
        descriptor.setArrayLength(2);
    }
    descriptor.setUsage(
        MTLTextureUsage::RenderTarget
            | MTLTextureUsage::ShaderRead
            | MTLTextureUsage::PixelFormatView,
    );
    descriptor.setStorageMode(MTLStorageMode::Private);
    let eyes = device
        .newTextureWithDescriptor(&descriptor)
        .expect("eye texture");

    let encode_eye = (
        even(APP_EYE.0 as f32 * scale),
        even(APP_EYE.1 as f32 * scale),
    );
    let compositor = Compositor::new(&device, encode_eye.0, encode_eye.1).expect("compositor");
    let (tx, rx) = mpsc::channel();
    let config = EncoderConfig {
        codec: Codec::Hevc,
        width: encode_eye.0 * 2,
        height: encode_eye.1,
        fps: FPS,
        bitrate: 60_000_000,
        low_latency: false,
    };
    let encoder = Arc::new(
        Encoder::<(usize, i64)>::new(config, move |out| {
            let _ = tx.send(out.map(|e| (e.meta, e.keyframe, now_ns())));
        })
        .expect("encoder"),
    );

    let period = Duration::from_secs_f32(1.0 / FPS);
    let start = Instant::now();
    let (mut dropped, mut skipped) = (0, 0);
    for frame in 0..FRAMES {
        std::thread::sleep(
            (start + period * frame as u32).saturating_duration_since(Instant::now()),
        );
        let submitted = now_ns();
        let encoder = encoder.clone();
        let source = |slice| EyeSource {
            texture: &eyes,
            slice,
            rect: [0, 0, APP_EYE.0 as i32, APP_EYE.1 as i32],
        };
        let composed = compositor.compose(&queue, [source(0), source(1)], move |slot| {
            let release = slot.clone();
            let pending = Pending {
                meta: (frame, submitted),
                release: Box::new(move || release.release()),
            };
            encoder.encode(&slot.pixel_buffer, submitted, frame == FRAMES / 2, pending);
        });
        if !composed {
            dropped += 1;
            // Warm-up frames can find every slot busy while the session spins up.
            skipped += usize::from(frame >= 10);
        }
    }

    let (mut totals, mut keyframes) = (Vec::new(), Vec::new());
    for _ in 0..FRAMES - dropped {
        let ((frame, submitted), keyframe, done) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("frame")
            .expect("encoded");
        if keyframe {
            keyframes.push(frame);
        }
        // The first frames warm the encoder up; the gate is about steady state.
        if frame >= 10 {
            totals.push((done - submitted) as f64 / 1e6);
        }
    }
    println!(
        "compose+encode {}x{} (scale {scale}) @ {FPS} Hz",
        encode_eye.0 * 2,
        encode_eye.1
    );
    Run {
        p50: percentile(&mut totals, 0.5),
        p95: percentile(&mut totals, 0.95),
        skipped,
        keyframes,
    }
}

/// Holds what the pipeline sustains today: no drops, throughput inside a frame, spikes inside
/// two. The R6 gate in `docs/ROADMAP.md` tightens p95 to one frame period. Timing depends on
/// what else the Mac is doing, so it runs on demand on an idle machine.
#[test]
#[ignore = "hardware timing gate; run with --ignored on an idle Mac"]
fn composed_frames_keep_up_with_the_headset() {
    let period = 1000.0 / FPS as f64;
    let r = compose_and_encode(SCALE);
    println!(
        "  p50 {:.2} ms, p95 {:.2} ms, skipped {}",
        r.p50, r.p95, r.skipped
    );
    assert_eq!(r.skipped, 0, "no frame may be dropped for want of a slot");
    assert_eq!(
        r.keyframes.first(),
        Some(&0),
        "the stream starts with a keyframe"
    );
    assert!(
        r.keyframes.contains(&(FRAMES / 2)),
        "a forced keyframe is honoured"
    );
    assert!(
        r.p50 < period,
        "p50 {:.2} ms must fit one frame period",
        r.p50
    );
    assert!(
        r.p95 < 2.0 * period,
        "p95 {:.2} ms must fit two frame periods",
        r.p95
    );
}

/// Encode latency p50/p95 in ms and the number of frames VideoToolbox dropped.
fn encode_only(codec: Codec, format: u32, low_latency: bool) -> (f64, f64, usize) {
    let size = (APP_EYE.0 * 2, APP_EYE.1);
    let buffers: Vec<_> = (0..4)
        .map(|_| iosurface_pixel_buffer(size.0, size.1, format).expect("pixel buffer"))
        .collect();
    let (tx, rx) = mpsc::channel();
    let config = EncoderConfig {
        codec,
        width: size.0,
        height: size.1,
        fps: FPS,
        bitrate: 60_000_000,
        low_latency,
    };
    let encoder = Arc::new(
        Encoder::<i64>::new(config, move |out| {
            let _ = tx.send(out.map(|e| now_ns() - e.meta));
        })
        .expect("encoder"),
    );
    let period = Duration::from_secs_f32(1.0 / FPS);
    let start = Instant::now();
    for frame in 0..FRAMES {
        std::thread::sleep(
            (start + period * frame as u32).saturating_duration_since(Instant::now()),
        );
        let now = now_ns();
        encoder.encode(
            &buffers[frame % buffers.len()],
            now,
            false,
            Pending {
                meta: now,
                release: Box::new(|| {}),
            },
        );
    }
    let mut latencies = Vec::new();
    let mut dropped = 0;
    for _ in 0..FRAMES {
        match rx.recv_timeout(Duration::from_secs(2)).expect("callback") {
            Some(ns) => latencies.push(ns as f64 / 1e6),
            None => dropped += 1,
        }
    }
    (
        percentile(&mut latencies, 0.5),
        percentile(&mut latencies, 0.95),
        dropped,
    )
}

#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn encoder_matrix() {
    for scale in [1.0, 0.9, 0.8, 0.75] {
        let r = compose_and_encode(scale);
        println!(
            "  p50 {:.2} ms, p95 {:.2} ms, skipped {}",
            r.p50, r.p95, r.skipped
        );
    }
    let formats = [
        ("BGRA", kCVPixelFormatType_32BGRA),
        ("420v", kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
    ];
    for codec in [Codec::Hevc, Codec::H264] {
        for (name, format) in formats {
            for low_latency in [false, true] {
                let (p50, p95, dropped) = encode_only(codec, format, low_latency);
                println!(
                    "{codec:?} {name} low_latency={low_latency:<5}: encode p50 {p50:6.2} ms  p95 {p95:6.2} ms  dropped {dropped}"
                );
            }
        }
    }
}
