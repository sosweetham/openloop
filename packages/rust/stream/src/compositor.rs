//! Converts the app's released eye images into one side-by-side 4:2:0 frame (BT.709 video
//! range) in an IOSurface the encoder reads without a copy. Native 4:2:0 input is what keeps
//! VideoToolbox inside a frame period; the measurements are in `tests/encode_latency.rs`.
//!
//! An eye of the encoded size is read texel for texel. A larger one is filtered down to it with
//! a Lanczos-3 kernel stretched over each output pixel's footprint, one axis per compute pass,
//! the second also converting to 4:2:0 (docs/ARCHITECTURE.md § Video quality).
//!
//! The work is encoded on the app's own command queue, so it orders after the app's rendering
//! without a CPU wait; the encoder is fed from the GPU completion handler.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSRange, NSString};
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder,
    MTLComputePipelineState, MTLDevice, MTLLibrary, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLSamplerDescriptor,
    MTLSamplerMinMagFilter, MTLSamplerState, MTLSize, MTLStorageMode, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureType, MTLTextureUsage, MTLViewport,
};

const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct Params { float2 uv_offset; float2 uv_scale; uint slice; uint encode_srgb; };
struct Out { float4 position [[position]]; float2 uv; };

vertex Out eye_vertex(uint id [[vertex_id]], constant Params& p [[buffer(0)]]) {
    float2 xy = float2((id << 1) & 2, id & 2);
    Out o;
    o.position = float4(xy * 2.0 - 1.0, 0.0, 1.0);
    o.uv = p.uv_offset + float2(xy.x, 1.0 - xy.y) * p.uv_scale;
    return o;
}

float3 encode_srgb(float3 c) {
    c = saturate(c);
    return select(1.055 * pow(c, 1.0 / 2.4) - 0.055, c * 12.92, c <= 0.0031308);
}

constant float3 BT709 = float3(0.2126, 0.7152, 0.0722);

float to_luma(float3 c) {
    return (16.0 + 219.0 * dot(c, BT709)) / 255.0;
}

float2 to_chroma(float3 c) {
    float y = dot(c, BT709);
    return (128.0 + 224.0 * float2((c.b - y) / 1.8556, (c.r - y) / 1.5748)) / 255.0;
}

// Gamma-encoded R'G'B': stored bytes for sRGB formats, encoded here for linear ones.
float3 fetch(Out in, texture2d_array<float> eye, sampler s, constant Params& p) {
    float3 c = saturate(eye.sample(s, in.uv, p.slice).rgb);
    return p.encode_srgb != 0 ? encode_srgb(c) : c;
}

fragment float luma(Out in [[stage_in]], texture2d_array<float> eye [[texture(0)]],
                    sampler s [[sampler(0)]], constant Params& p [[buffer(0)]]) {
    return to_luma(fetch(in, eye, s, p));
}

fragment float2 chroma(Out in [[stage_in]], texture2d_array<float> eye [[texture(0)]],
                       sampler s [[sampler(0)]], constant Params& p [[buffer(0)]]) {
    return to_chroma(fetch(in, eye, s, p));
}

struct Across {
    int2 origin;
    int2 limit;
    uint width;
    uint height;
    uint slice;
    uint encode_srgb;
    uint taps;
    uint eye;
    uint span;
    uint reserved;
};

constant uint WIDE = 64;

// The downscale's first half: each eye's rows filtered to the encoded width, in gamma-encoded
// R'G'B'. A threadgroup covers 64 output columns of 4 rows: it reads the source texels they
// reach once, then output column x sums `taps` of them from first[x], weighted by
// weights[x * taps...]. Texels past the rect repeat its edge.
kernel void scale_across(texture2d_array<float, access::read> source [[texture(0)]],
                         texture2d_array<half, access::write> across [[texture(1)]],
                         constant Across& a [[buffer(0)]],
                         device const int* first [[buffer(1)]],
                         device const float* weights [[buffer(2)]],
                         threadgroup half3* texels [[threadgroup(0)]],
                         uint2 group [[threadgroup_position_in_grid]],
                         uint2 local [[thread_position_in_threadgroup]]) {
    uint x0 = group.x * WIDE;
    uint last = min(x0 + WIDE, a.width) - 1;
    int start = first[x0];
    int span = first[last] + int(a.taps) - start;
    uint row = group.y * 4 + local.y;
    int y = min(int(row), a.limit.y);
    for (int i = int(local.x); i < span; i += int(WIDE)) {
        int x = clamp(start + i, 0, a.limit.x);
        float3 c = source.read(uint2(a.origin + int2(x, y)), a.slice).rgb;
        texels[local.y * a.span + i] = half3(a.encode_srgb != 0 ? encode_srgb(c) : c);
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    uint x = x0 + local.x;
    if (x > last || row >= a.height) {
        return;
    }
    int o = first[x] - start;
    device const float* w = weights + x * a.taps;
    float3 sum = 0.0;
    for (uint t = 0; t < a.taps; t++) {
        sum += float3(texels[local.y * a.span + o + int(t)]) * w[t];
    }
    across.write(half4(half3(sum), 1.0h), uint2(x, row), a.eye);
}

struct Down {
    uint2 size;
    uint2 offset;
    uint limit;
    uint eye;
    uint taps;
    uint reserved;
};

constant uint TALL = 32;

// The second half: each eye's columns filtered to the encoded height and converted to 4:2:0.
// A threadgroup covers 16 output columns of 32 rows, two per thread, read once from `across`
// as above; each 2x2 block's mean colour becomes one chroma sample.
kernel void scale_down(texture2d_array<half, access::read> across [[texture(0)]],
                       texture2d<float, access::write> luma [[texture(1)]],
                       texture2d<float, access::write> chroma [[texture(2)]],
                       constant Down& d [[buffer(0)]],
                       device const int* first [[buffer(1)]],
                       device const float* weights [[buffer(2)]],
                       threadgroup half3* rows [[threadgroup(0)]],
                       uint2 group [[threadgroup_position_in_grid]],
                       uint2 local [[thread_position_in_threadgroup]]) {
    threadgroup half3 colour[TALL][16];
    uint x = group.x * 16 + local.x;
    uint y0 = group.y * TALL;
    uint last = min(y0 + TALL, d.size.y) - 1;
    int start = first[y0];
    int depth = first[last] + int(d.taps) - start;
    bool column = x < d.size.x;
    for (int i = int(local.y); i < depth; i += 16) {
        uint y = uint(clamp(start + i, 0, int(d.limit)));
        rows[i * 16 + local.x] = column ? across.read(uint2(x, y), d.eye).rgb : half3(0.0h);
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint k = 0; k < TALL; k += 16) {
        uint ly = local.y + k;
        uint y = y0 + ly;
        float3 c = 0.0;
        if (column && y <= last) {
            int o = first[y] - start;
            device const float* w = weights + y * d.taps;
            for (uint t = 0; t < d.taps; t++) {
                c += float3(rows[(o + int(t)) * 16 + local.x]) * w[t];
            }
            c = saturate(c);
            luma.write(float4(to_luma(c)), d.offset + uint2(x, y));
        }
        colour[ly][local.x] = half3(c);
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    uint ly = local.y * 2;
    if ((local.x & 1) == 0 && column && y0 + ly <= last) {
        float3 block = float3(colour[ly][local.x]) + float3(colour[ly][local.x + 1])
            + float3(colour[ly + 1][local.x]) + float3(colour[ly + 1][local.x + 1]);
        chroma.write(float4(to_chroma(block * 0.25), 0.0, 0.0), (d.offset + uint2(x, y0 + ly)) / 2);
    }
}
"#;

/// The shader's threadgroup shapes: `scale_across` covers `WIDE` output columns of 4 rows,
/// `scale_down` 16 columns of `TALL` rows. Each holds the source texels its outputs reach in
/// threadgroup memory, bounded here by `SPAN` per row and `DEPTH` rows, which keeps a 2.0 render
/// scale filtered to the encoded size well inside the 32 KB a threadgroup has.
const WIDE: u32 = 64;
const SPAN: i32 = 288;
const TALL: u32 = 32;
const DEPTH: i32 = 128;

#[repr(C)]
struct Params {
    uv_offset: [f32; 2],
    uv_scale: [f32; 2],
    slice: u32,
    encode_srgb: u32,
}

#[repr(C)]
struct Across {
    origin: [i32; 2],
    limit: [i32; 2],
    width: u32,
    height: u32,
    slice: u32,
    encode_srgb: u32,
    taps: u32,
    eye: u32,
    span: u32,
    reserved: u32,
}

#[repr(C)]
struct Down {
    size: [u32; 2],
    offset: [u32; 2],
    limit: u32,
    eye: u32,
    taps: u32,
    reserved: u32,
}

type Texture = ProtocolObject<dyn MTLTexture>;
type Pipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
/// A 2D-array view of an eye's texture, and whether its values still need sRGB encoding.
type View = (Retained<Texture>, bool);

/// One eye as the app submitted it.
pub struct EyeSource<'a> {
    pub texture: &'a Texture,
    pub slice: u32,
    /// Pixel rect `[x, y, width, height]` inside the texture.
    pub rect: [i32; 4],
}

pub struct Slot {
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
    luma: Retained<Texture>,
    chroma: Retained<Texture>,
    busy: AtomicBool,
}

// SAFETY: CVPixelBuffer and MTLTexture are reference-counted objects safe to pass between
// threads; `busy` arbitrates which side may touch the pixels.
unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

impl Slot {
    pub fn release(&self) {
        self.busy.store(false, Ordering::Release);
    }
}

/// Lanczos-3 weights along one axis, the kernel stretched over each output pixel's footprint:
/// output pixel `i` sums `taps` source texels from `first[i]`, weighted by
/// `weights[i * taps..][..taps]`, which sum to 1.
struct Taps {
    first: Vec<i32>,
    weights: Vec<f32>,
    taps: u32,
}

impl Taps {
    fn lanczos3(source: u32, out: u32) -> Taps {
        let step = f64::from(source) / f64::from(out.max(1));
        let stretch = step.max(1.0);
        let reach = 3.0 * stretch;
        let taps = (2.0 * reach).ceil() as usize + 1;
        let kernel = |x: f64| {
            let x = x.abs();
            if x < 1e-9 {
                1.0
            } else if x >= 3.0 {
                0.0
            } else {
                let t = std::f64::consts::PI * x;
                3.0 * t.sin() * (t / 3.0).sin() / (t * t)
            }
        };
        let mut first = Vec::with_capacity(out as usize);
        let mut weights = Vec::with_capacity(out as usize * taps);
        for i in 0..out {
            let centre = (f64::from(i) + 0.5) * step;
            let start = (centre - reach - 0.5).ceil();
            let row: Vec<f64> = (0..taps)
                .map(|t| kernel((start + t as f64 + 0.5 - centre) / stretch))
                .collect();
            let total: f64 = row.iter().sum();
            first.push(start as i32);
            weights.extend(row.iter().map(|w| (w / total) as f32));
        }
        Taps {
            first,
            weights,
            taps: taps as u32,
        }
    }

    /// The most source texels any `group` consecutive output pixels reach.
    fn reach(&self, group: u32) -> i32 {
        self.first
            .chunks(group as usize)
            .map(|outputs| outputs[outputs.len() - 1] + self.taps as i32 - outputs[0])
            .max()
            .unwrap_or(0)
    }
}

type Kernel = Retained<ProtocolObject<dyn MTLComputePipelineState>>;

/// The downscale of `source`-sized eyes to the encoded size: both axes' weights on the GPU, and
/// each eye filtered across, at the encoded width and the source height, in a two-slice array.
/// Every frame's passes run in order on one queue, so one array serves every frame in flight.
struct Scaler {
    across_kernel: Kernel,
    down_kernel: Kernel,
    across_taps: (Buffer, Buffer, u32),
    down_taps: (Buffer, Buffer, u32),
    /// Source texels one `scale_across` row reaches, and source rows one `scale_down`
    /// threadgroup reaches: the threadgroup memory each holds.
    span: u32,
    depth: u32,
    across: Retained<Texture>,
    source: (u32, u32),
}

pub struct Compositor {
    luma: Pipeline,
    chroma: Pipeline,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    scaler: Option<Scaler>,
    slots: Vec<Arc<Slot>>,
    pub eye_width: u32,
    pub eye_height: u32,
}

// SAFETY: Metal pipeline, sampler, buffer and texture objects are thread-safe.
unsafe impl Send for Compositor {}
unsafe impl Sync for Compositor {}

#[derive(Debug)]
pub struct CompositorError(pub String);

const SLOTS: usize = 6;

impl Compositor {
    /// `eye` is the size each eye is encoded at, and must be even (4:2:0 chroma). `source` is
    /// the size of the eyes the app submits; when it differs from `eye`, eyes of that size are
    /// filtered to `eye`, and any other size is sampled once per pixel.
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        source: (u32, u32),
        eye: (u32, u32),
    ) -> Result<Self, CompositorError> {
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .map_err(|e| CompositorError(format!("shader: {e}")))?;
        let function = |name: &str| {
            library
                .newFunctionWithName(&NSString::from_str(name))
                .ok_or_else(|| CompositorError(name.into()))
        };
        let pipeline =
            |fragment: &str, format: MTLPixelFormat| -> Result<Pipeline, CompositorError> {
                let (vertex, fragment_function) = (function("eye_vertex")?, function(fragment)?);
                let descriptor = MTLRenderPipelineDescriptor::new();
                descriptor.setVertexFunction(Some(&vertex));
                descriptor.setFragmentFunction(Some(&fragment_function));
                // SAFETY: index 0 always exists.
                unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) }
                    .setPixelFormat(format);
                device
                    .newRenderPipelineStateWithDescriptor_error(&descriptor)
                    .map_err(|e| CompositorError(format!("{fragment} pipeline: {e}")))
            };

        let sampler_descriptor = MTLSamplerDescriptor::new();
        sampler_descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
        sampler_descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
        let sampler = device
            .newSamplerStateWithDescriptor(&sampler_descriptor)
            .ok_or_else(|| CompositorError("sampler".into()))?;

        let scaler = if source == eye {
            None
        } else {
            let (across, down) = (
                Taps::lanczos3(source.0, eye.0),
                Taps::lanczos3(source.1, eye.1),
            );
            if across.reach(WIDE) > SPAN || down.reach(TALL) > DEPTH {
                return Err(CompositorError(format!(
                    "{}x{} eyes are too large to filter to {}x{}",
                    source.0, source.1, eye.0, eye.1
                )));
            }
            let upload = |taps: &Taps| -> Result<(Buffer, Buffer, u32), CompositorError> {
                Ok((
                    buffer(device, &taps.first)?,
                    buffer(device, &taps.weights)?,
                    taps.taps,
                ))
            };
            let kernel = |name: &str| -> Result<Kernel, CompositorError> {
                let function = function(name)?;
                device
                    .newComputePipelineStateWithFunction_error(&function)
                    .map_err(|e| CompositorError(format!("{name} pipeline: {e}")))
            };
            let working = eye_array(device, eye.0, source.1).ok_or_else(|| {
                CompositorError(format!("{}x{} downscale texture", eye.0, source.1))
            })?;
            Some(Scaler {
                across_kernel: kernel("scale_across")?,
                down_kernel: kernel("scale_down")?,
                across_taps: upload(&across)?,
                down_taps: upload(&down)?,
                span: across.reach(WIDE) as u32,
                depth: down.reach(TALL) as u32,
                across: working,
                source,
            })
        };

        let slots = (0..SLOTS)
            .map(|_| make_slot(device, eye.0 * 2, eye.1))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            luma: pipeline("luma", MTLPixelFormat::R8Unorm)?,
            chroma: pipeline("chroma", MTLPixelFormat::RG8Unorm)?,
            sampler,
            scaler,
            slots,
            eye_width: eye.0,
            eye_height: eye.1,
        })
    }

    /// Encodes the conversion on `queue`. `done` runs on a Metal thread once the GPU has written
    /// the slot; the caller must `release` the slot when the encoder is done with it. Returns
    /// `false`, encoding nothing, when every slot is still in flight.
    pub fn compose(
        &self,
        queue: &ProtocolObject<dyn MTLCommandQueue>,
        eyes: [EyeSource<'_>; 2],
        done: impl FnOnce(Arc<Slot>) + Send + 'static,
    ) -> bool {
        let Some(slot) = self
            .slots
            .iter()
            .find(|s| !s.busy.swap(true, Ordering::AcqRel))
            .cloned()
        else {
            return false;
        };
        let Some(commands) = queue.commandBuffer() else {
            slot.release();
            return false;
        };
        let views: Vec<_> = eyes.iter().map(|e| byte_view(e.texture)).collect();
        let scaled: [bool; 2] = std::array::from_fn(|i| {
            let [_, _, w, h] = eyes[i].rect;
            self.scaler
                .as_ref()
                .is_some_and(|s| (w, h) == (s.source.0 as i32, s.source.1 as i32))
        });
        let read: [Option<&View>; 2] =
            std::array::from_fn(|i| views[i].as_ref().filter(|_| !scaled[i]));
        let filtered: [Option<&View>; 2] =
            std::array::from_fn(|i| views[i].as_ref().filter(|_| scaled[i]));
        if read.iter().any(Option::is_some) {
            self.pass(&commands, &self.luma, &slot.luma, 1, &eyes, read);
            self.pass(&commands, &self.chroma, &slot.chroma, 2, &eyes, read);
        }
        if let Some(scaler) = self
            .scaler
            .as_ref()
            .filter(|_| filtered.iter().any(Option::is_some))
        {
            self.scale(&commands, scaler, &slot, &eyes, filtered);
        }

        let done = Mutex::new(Some(done));
        let finished = slot.clone();
        let handler = RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
            if let Some(done) = done.lock().ok().and_then(|mut d| d.take()) {
                done(finished.clone());
            }
        });
        // SAFETY: Metal retains the block until it has run.
        unsafe { commands.addCompletedHandler(RcBlock::as_ptr(&handler)) };
        commands.commit();
        true
    }

    /// Draws the eyes that have a view side by side into one plane, one sample per pixel;
    /// `divisor` is the plane's subsampling.
    fn pass(
        &self,
        commands: &ProtocolObject<dyn MTLCommandBuffer>,
        pipeline: &Pipeline,
        target: &Texture,
        divisor: u32,
        eyes: &[EyeSource<'_>; 2],
        views: [Option<&View>; 2],
    ) {
        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: index 0 always exists.
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(target));
        attachment.setLoadAction(MTLLoadAction::DontCare);
        attachment.setStoreAction(MTLStoreAction::Store);
        let Some(encoder) = commands.renderCommandEncoderWithDescriptor(&pass) else {
            return;
        };
        encoder.setRenderPipelineState(pipeline);
        // SAFETY: valid sampler at fragment index 0.
        unsafe { encoder.setFragmentSamplerState_atIndex(Some(&self.sampler), 0) };

        let (eye_w, eye_h) = (
            (self.eye_width / divisor) as f64,
            (self.eye_height / divisor) as f64,
        );
        for (index, (eye, view)) in eyes.iter().zip(views).enumerate() {
            let Some((view, encode_srgb)) = view else {
                continue;
            };
            let (w, h) = (eye.texture.width() as f32, eye.texture.height() as f32);
            let [x, y, rw, rh] = eye.rect.map(|v| v as f32);
            let params = Params {
                uv_offset: [x / w, y / h],
                uv_scale: [rw / w, rh / h],
                slice: eye.slice,
                encode_srgb: *encode_srgb as u32,
            };
            let bytes = NonNull::from(&params).cast::<c_void>();
            encoder.setViewport(MTLViewport {
                originX: index as f64 * eye_w,
                originY: 0.0,
                width: eye_w,
                height: eye_h,
                znear: 0.0,
                zfar: 1.0,
            });
            // SAFETY: `params` outlives the calls; Metal copies the bytes immediately.
            unsafe {
                encoder.setVertexBytes_length_atIndex(bytes, size_of::<Params>(), 0);
                encoder.setFragmentBytes_length_atIndex(bytes, size_of::<Params>(), 0);
                encoder.setFragmentTexture_atIndex(Some(view), 0);
                encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            }
        }
        encoder.endEncoding();
    }

    /// Filters the eyes that have a view down into both planes of `slot`.
    fn scale(
        &self,
        commands: &ProtocolObject<dyn MTLCommandBuffer>,
        scaler: &Scaler,
        slot: &Slot,
        eyes: &[EyeSource<'_>; 2],
        views: [Option<&View>; 2],
    ) {
        let Some(encoder) = commands.computeCommandEncoder() else {
            return;
        };
        let groups = |w: u32, h: u32| MTLSize {
            width: w as usize,
            height: h as usize,
            depth: 1,
        };
        let (eye_w, eye_h, source_h) = (self.eye_width, self.eye_height, scaler.source.1);
        let tables = |taps: &(Buffer, Buffer, u32)| {
            // SAFETY: the tables outlive the encoder, which retains them; they hold `taps`
            // weights for every output pixel the dispatch covers.
            unsafe {
                encoder.setBuffer_offset_atIndex(Some(&taps.0), 0, 1);
                encoder.setBuffer_offset_atIndex(Some(&taps.1), 0, 2);
            }
        };
        let bytes = |value: NonNull<c_void>, len: usize| {
            // SAFETY: the value outlives the call; Metal copies the bytes immediately.
            unsafe { encoder.setBytes_length_atIndex(value, len, 0) };
        };

        // half3 occupies 8 bytes; Metal wants threadgroup lengths in multiples of 16.
        let threadgroup = |values: u32| (values as usize * 8).next_multiple_of(16);
        encoder.setComputePipelineState(&scaler.across_kernel);
        tables(&scaler.across_taps);
        // SAFETY: `scale_across` indexes at most 4 rows of `span` texels.
        unsafe { encoder.setThreadgroupMemoryLength_atIndex(threadgroup(4 * scaler.span), 0) };
        // SAFETY: the working array outlives the encoder, which retains it.
        unsafe { encoder.setTexture_atIndex(Some(&scaler.across), 1) };
        for (index, (eye, view)) in eyes.iter().zip(views).enumerate() {
            let Some((view, encode_srgb)) = view else {
                continue;
            };
            let [x, y, w, h] = eye.rect;
            let params = Across {
                origin: [x, y],
                limit: [w - 1, h - 1],
                width: eye_w,
                height: source_h,
                slice: eye.slice,
                encode_srgb: *encode_srgb as u32,
                taps: scaler.across_taps.2,
                eye: index as u32,
                span: scaler.span,
                reserved: 0,
            };
            bytes(NonNull::from(&params).cast(), size_of::<Across>());
            // SAFETY: the view outlives the encoder, which retains it.
            unsafe { encoder.setTexture_atIndex(Some(view), 0) };
            encoder.dispatchThreadgroups_threadsPerThreadgroup(
                groups(eye_w.div_ceil(WIDE), source_h.div_ceil(4)),
                groups(WIDE, 4),
            );
        }

        encoder.setComputePipelineState(&scaler.down_kernel);
        tables(&scaler.down_taps);
        // SAFETY: `scale_down` indexes at most `depth` rows of 16 texels.
        unsafe { encoder.setThreadgroupMemoryLength_atIndex(threadgroup(16 * scaler.depth), 0) };
        // SAFETY: the working array and the planes outlive the encoder, which retains them.
        unsafe {
            encoder.setTexture_atIndex(Some(&scaler.across), 0);
            encoder.setTexture_atIndex(Some(&slot.luma), 1);
            encoder.setTexture_atIndex(Some(&slot.chroma), 2);
        }
        for index in (0..2).filter(|&i| views[i].is_some()) {
            let params = Down {
                size: [eye_w, eye_h],
                offset: [index as u32 * eye_w, 0],
                limit: source_h - 1,
                eye: index as u32,
                taps: scaler.down_taps.2,
                reserved: 0,
            };
            bytes(NonNull::from(&params).cast(), size_of::<Down>());
            encoder.dispatchThreadgroups_threadsPerThreadgroup(
                groups(eye_w.div_ceil(16), eye_h.div_ceil(TALL)),
                groups(16, 16),
            );
        }
        encoder.endEncoding();
    }
}

/// A two-slice working array the downscale writes and reads back.
fn eye_array(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
) -> Option<Retained<Texture>> {
    let descriptor = MTLTextureDescriptor::new();
    descriptor.setTextureType(MTLTextureType::Type2DArray);
    descriptor.setPixelFormat(MTLPixelFormat::RGBA16Float);
    // SAFETY: plain descriptor configuration.
    unsafe {
        descriptor.setWidth(width as usize);
        descriptor.setHeight(height.max(1) as usize);
        descriptor.setArrayLength(2);
    }
    descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
    descriptor.setStorageMode(MTLStorageMode::Private);
    device.newTextureWithDescriptor(&descriptor)
}

/// A shared buffer holding `values`.
fn buffer<T: Copy>(
    device: &ProtocolObject<dyn MTLDevice>,
    values: &[T],
) -> Result<Buffer, CompositorError> {
    let bytes = NonNull::new(values.as_ptr() as *mut c_void)
        .ok_or_else(|| CompositorError("empty weight table".into()))?;
    // SAFETY: `values` is valid for its byte length, which Metal copies before returning.
    unsafe {
        device.newBufferWithBytes_length_options(
            bytes,
            size_of_val(values),
            MTLResourceOptions::StorageModeShared,
        )
    }
    .ok_or_else(|| CompositorError("weight table buffer".into()))
}

/// A 2D-array view in the non-sRGB twin of the texture's format, so sampling returns the stored
/// bytes unchanged, and whether those bytes are linear and still need sRGB encoding.
fn byte_view(texture: &Texture) -> Option<View> {
    let (format, linear) = match texture.pixelFormat() {
        MTLPixelFormat::RGBA8Unorm_sRGB => (MTLPixelFormat::RGBA8Unorm, false),
        MTLPixelFormat::BGRA8Unorm_sRGB => (MTLPixelFormat::BGRA8Unorm, false),
        other => (other, true),
    };
    let slices = texture.arrayLength().max(1);
    // SAFETY: OpenLoop creates every swapchain texture with PixelFormatView usage.
    let view = unsafe {
        texture.newTextureViewWithPixelFormat_textureType_levels_slices(
            format,
            MTLTextureType::Type2DArray,
            NSRange::new(0, 1),
            NSRange::new(0, slices),
        )
    }?;
    Some((view, linear))
}

fn make_slot(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
) -> Result<Arc<Slot>, CompositorError> {
    let pixel_buffer = iosurface_pixel_buffer(
        width,
        height,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    )?;
    let surface = CVPixelBufferGetIOSurface(Some(&pixel_buffer))
        .ok_or_else(|| CompositorError("no IOSurface".into()))?;
    let plane = |format, w: u32, h: u32, index| {
        // SAFETY: plain descriptor construction.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format, w as usize, h as usize, false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderWrite);
        descriptor.setStorageMode(MTLStorageMode::Shared);
        device
            .newTextureWithDescriptor_iosurface_plane(&descriptor, &surface, index)
            .ok_or_else(|| CompositorError(format!("IOSurface plane {index}")))
    };
    Ok(Arc::new(Slot {
        luma: plane(MTLPixelFormat::R8Unorm, width, height, 0)?,
        chroma: plane(MTLPixelFormat::RG8Unorm, width / 2, height / 2, 1)?,
        pixel_buffer,
        busy: AtomicBool::new(false),
    }))
}

/// An IOSurface-backed, Metal-compatible pixel buffer: what both Metal and VideoToolbox can
/// touch without a copy.
pub fn iosurface_pixel_buffer(
    width: u32,
    height: u32,
    format: u32,
) -> Result<CFRetained<CVPixelBuffer>, CompositorError> {
    let yes = CFBoolean::new(true);
    let empty = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
    // SAFETY: CoreVideo's static attribute keys.
    let attributes = unsafe {
        CFDictionary::<CFString, CFType>::from_slices(
            &[
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ],
            &[empty.as_ref(), yes.as_ref()],
        )
    };
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: valid attributes; `out` receives a +1 reference on success.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            format,
            Some(attributes.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    NonNull::new(out)
        .filter(|_| status == 0)
        // SAFETY: a successful create returns a +1 reference.
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or_else(|| CompositorError(format!("CVPixelBufferCreate {status}")))
}
