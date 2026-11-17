//! Encodes on this Mac's real media engine at the headset's frame rate, and holds the
//! encode-latency quality gate from `docs/ROADMAP.md`. The ignored matrix prints how codec,
//! input format and rate control trade latency.
#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLStorageMode,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use tether_stream::compositor::{Compositor, EyeSource, iosurface_pixel_buffer};
use tether_stream::encoder::{Codec, Encoder, EncoderConfig, Pending};
use tether_stream::time::now_ns;
use tether_stream::video;

/// Quest 3's recommended per-eye render size.
const QUEST3_EYE: (u32, u32) = (1680, 1760);
const FPS: f32 = 90.0;
/// Long enough to see VideoToolbox's steady state, which differs from its first seconds.
const FRAMES: usize = 1800;

/// A scene that moves every frame: rings, fine stripes and gradients, so motion search and
/// rate control work as they do on an app's frames.
const SCENE: &str = r#"
#include <metal_stdlib>
using namespace metal;
vertex float4 cover(uint id [[vertex_id]]) {
    float2 xy = float2((id << 1) & 2, id & 2);
    return float4(xy * 2.0 - 1.0, 0.0, 1.0);
}
fragment float4 scene(float4 position [[position]], constant float& t [[buffer(0)]]) {
    float2 p = position.xy + float2(6.0 * t, 2.0 * t);
    float rings = 0.5 + 0.5 * sin(length(p - float2(1100.0, 1200.0)) * 0.3);
    float stripes = step(0.5, fract(p.x / 7.0 + p.y / 29.0));
    float ramp = fract((p.x + 2.0 * p.y) / 1500.0);
    return float4(rings, mix(stripes, ramp, 0.4), ramp, 1.0);
}
"#;

type Texture = ProtocolObject<dyn MTLTexture>;

fn percentile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * q) as usize]
}

struct Run {
    /// VideoToolbox alone: from the composed frame to its access unit, ms.
    encode_p50: f64,
    encode_p95: f64,
    /// From the app's submission to the composed frame: its rendering still queued, then the
    /// compositor's, ms.
    compose_p50: f64,
    compose_p95: f64,
    skipped: usize,
    keyframes: Vec<usize>,
}

/// Draws the scene at time `frame` into both slices of `eyes`, as an app renders its views.
struct App {
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    eyes: Retained<Texture>,
}

impl App {
    fn new(device: &ProtocolObject<dyn MTLDevice>, eye: (u32, u32)) -> App {
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SCENE), None)
            .expect("scene shader");
        let function = |name| {
            library
                .newFunctionWithName(&NSString::from_str(name))
                .expect("scene function")
        };
        let descriptor = MTLRenderPipelineDescriptor::new();
        descriptor.setVertexFunction(Some(&function("cover")));
        descriptor.setFragmentFunction(Some(&function("scene")));
        // SAFETY: index 0 always exists.
        unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) }
            .setPixelFormat(MTLPixelFormat::RGBA8Unorm_sRGB);
        let pipeline = device
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .expect("scene pipeline");

        let texture = MTLTextureDescriptor::new();
        texture.setTextureType(MTLTextureType::Type2DArray);
        texture.setPixelFormat(MTLPixelFormat::RGBA8Unorm_sRGB);
        // SAFETY: plain descriptor configuration.
        unsafe {
            texture.setWidth(eye.0 as usize);
            texture.setHeight(eye.1 as usize);
            texture.setArrayLength(2);
        }
        texture.setUsage(
            MTLTextureUsage::RenderTarget
                | MTLTextureUsage::ShaderRead
                | MTLTextureUsage::PixelFormatView,
        );
        texture.setStorageMode(MTLStorageMode::Private);
        let eyes = device.newTextureWithDescriptor(&texture).expect("eyes");
        App { pipeline, eyes }
    }

    fn render(&self, queue: &ProtocolObject<dyn MTLCommandQueue>, frame: usize) {
        let commands = queue.commandBuffer().expect("command buffer");
        let t = frame as f32;
        for slice in 0..2 {
            let pass = MTLRenderPassDescriptor::new();
            // SAFETY: index 0 always exists.
            let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            attachment.setTexture(Some(&self.eyes));
            attachment.setSlice(slice);
            attachment.setLoadAction(MTLLoadAction::DontCare);
            attachment.setStoreAction(MTLStoreAction::Store);
            let encoder = commands
                .renderCommandEncoderWithDescriptor(&pass)
                .expect("render encoder");
            encoder.setRenderPipelineState(&self.pipeline);
            // SAFETY: `t` outlives the call; Metal copies the bytes.
            unsafe {
                encoder.setFragmentBytes_length_atIndex(
                    NonNull::from(&t).cast::<c_void>(),
                    size_of::<f32>(),
                    0,
                );
                encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            }
            encoder.endEncoding();
        }
        commands.commit();
    }
}

/// The production path: the app's supersampled eyes → downscale and 4:2:0 compositor → HEVC at
/// the USB bitrate, paced at the headset rate.
fn compose_and_encode(render_scale: f32) -> Run {
    let device = MTLCreateSystemDefaultDevice().expect("a Metal device");
    let queue = device.newCommandQueue().expect("a command queue");
    let app_eye = video::render_eye(QUEST3_EYE, render_scale);
    let encode_eye = video::encode_eye(app_eye, FPS);
    let app = App::new(&device, app_eye);
    let compositor = Compositor::new(&device, app_eye, encode_eye).expect("compositor");
    let (tx, rx) = mpsc::channel();
    let config = EncoderConfig {
        codec: Codec::Hevc,
        width: encode_eye.0 * 2,
        height: encode_eye.1,
        fps: FPS,
        bitrate: video::USB_BITRATE,
        low_latency: false,
    };
    let encoder = Arc::new(
        Encoder::<(usize, i64, i64)>::new(config, move |out| {
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
        app.render(&queue, frame);
        let submitted = now_ns();
        let encoder = encoder.clone();
        let source = |slice| EyeSource {
            texture: &app.eyes,
            slice,
            rect: [0, 0, app_eye.0 as i32, app_eye.1 as i32],
        };
        let composed = compositor.compose(&queue, [source(0), source(1)], move |slot| {
            let release = slot.clone();
            let pending = Pending {
                meta: (frame, submitted, now_ns()),
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

    let (mut encode, mut compose, mut keyframes) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..FRAMES - dropped {
        let ((frame, submitted, composed), keyframe, done) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("frame")
            .expect("encoded");
        if keyframe {
            keyframes.push(frame);
        }
        // The first frames warm the encoder up; the gate is about steady state.
        if frame >= 10 {
            encode.push((done - composed) as f64 / 1e6);
            compose.push((composed - submitted) as f64 / 1e6);
        }
    }
    println!(
        "compose+encode {}x{} eyes to {}x{} at {} Mbps @ {FPS} Hz",
        app_eye.0,
        app_eye.1,
        encode_eye.0 * 2,
        encode_eye.1,
        video::USB_BITRATE / 1_000_000
    );
    Run {
        encode_p50: percentile(&mut encode, 0.5),
        encode_p95: percentile(&mut encode, 0.95),
        compose_p50: percentile(&mut compose, 0.5),
        compose_p95: percentile(&mut compose, 0.95),
        skipped,
        keyframes,
    }
}

impl Run {
    fn print(&self) {
        println!(
            "  encode p50 {:.2} ms, p95 {:.2} ms; submission to composed p50 {:.2} ms, p95 {:.2} ms; skipped {}",
            self.encode_p50, self.encode_p95, self.compose_p50, self.compose_p95, self.skipped
        );
    }
}

/// Holds the ROADMAP gate on what the pipeline sustains, over 20 s of steady state: no frame
/// dropped before encode and encode latency p95 within one frame period. Timing depends on what
/// else the Mac is doing, so it runs on demand on an idle machine.
#[test]
#[ignore = "hardware timing gate; run with --ignored on an idle Mac"]
fn composed_frames_keep_up_with_the_headset() {
    let period = 1000.0 / FPS as f64;
    let r = compose_and_encode(video::DEFAULT_RENDER_SCALE);
    r.print();
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
        r.encode_p95 < period,
        "encode p95 {:.2} ms must fit one frame period",
        r.encode_p95
    );
}

/// Encode latency p50/p95 in ms and the number of frames VideoToolbox dropped.
fn encode_only(codec: Codec, format: u32, low_latency: bool) -> (f64, f64, usize) {
    let size = (QUEST3_EYE.0 * 2, QUEST3_EYE.1);
    let buffers: Vec<_> = (0..4)
        .map(|_| iosurface_pixel_buffer(size.0, size.1, format).expect("pixel buffer"))
        .collect();
    let (tx, rx) = mpsc::channel();
    let config = EncoderConfig {
        codec,
        width: size.0,
        height: size.1,
        fps: FPS,
        bitrate: video::USB_BITRATE,
        low_latency,
    };
    let encoder = Arc::new(
        Encoder::<i64>::new(config, move |out| {
            let _ = tx.send(out.map(|e| now_ns() - e.meta));
        })
        .expect("encoder"),
    );
    let frames = 180;
    let period = Duration::from_secs_f32(1.0 / FPS);
    let start = Instant::now();
    for frame in 0..frames {
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
    for _ in 0..frames {
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
    for scale in [1.0, 1.2, 1.4, 1.6, 2.0] {
        compose_and_encode(scale).print();
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
