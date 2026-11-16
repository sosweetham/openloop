//! Converts the app's released eye images into one side-by-side 4:2:0 frame (BT.709 video
//! range) in an IOSurface the encoder reads without a copy. Native 4:2:0 input is what keeps
//! VideoToolbox inside a frame period; the measurements are in `tests/encode_latency.rs`.
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
    MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLLibrary, MTLLoadAction,
    MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSamplerDescriptor,
    MTLSamplerMinMagFilter, MTLSamplerState, MTLStorageMode, MTLStoreAction, MTLTexture,
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

// Gamma-encoded R'G'B': stored bytes for sRGB formats, encoded here for linear ones.
float3 fetch(Out in, texture2d_array<float> eye, sampler s, constant Params& p) {
    float3 c = saturate(eye.sample(s, in.uv, p.slice).rgb);
    if (p.encode_srgb != 0) {
        c = select(1.055 * pow(c, 1.0 / 2.4) - 0.055, c * 12.92, c <= 0.0031308);
    }
    return c;
}

constant float3 BT709 = float3(0.2126, 0.7152, 0.0722);

fragment float luma(Out in [[stage_in]], texture2d_array<float> eye [[texture(0)]],
                    sampler s [[sampler(0)]], constant Params& p [[buffer(0)]]) {
    return (16.0 + 219.0 * dot(fetch(in, eye, s, p), BT709)) / 255.0;
}

fragment float2 chroma(Out in [[stage_in]], texture2d_array<float> eye [[texture(0)]],
                       sampler s [[sampler(0)]], constant Params& p [[buffer(0)]]) {
    float3 c = fetch(in, eye, s, p);
    float y = dot(c, BT709);
    return (128.0 + 224.0 * float2((c.b - y) / 1.8556, (c.r - y) / 1.5748)) / 255.0;
}
"#;

#[repr(C)]
struct Params {
    uv_offset: [f32; 2],
    uv_scale: [f32; 2],
    slice: u32,
    encode_srgb: u32,
}

type Texture = ProtocolObject<dyn MTLTexture>;
type Pipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

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

    /// The luma plane as `(width, height, rows)`, for diagnostics. Reads CPU-side, so only
    /// call it once the GPU has finished with the slot.
    pub fn luma(&self) -> Option<(usize, usize, Vec<u8>)> {
        use objc2_core_video::{
            CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
            CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
            CVPixelBufferUnlockBaseAddress,
        };
        let buffer = &*self.pixel_buffer;
        // SAFETY: valid pixel buffer; the read-only lock maps it for the duration of the copy.
        unsafe {
            if CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return None;
            }
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0) as *const u8;
            let (w, h, stride) = (
                CVPixelBufferGetWidthOfPlane(buffer, 0),
                CVPixelBufferGetHeightOfPlane(buffer, 0),
                CVPixelBufferGetBytesPerRowOfPlane(buffer, 0),
            );
            let rows = (!base.is_null()).then(|| {
                (0..h).flat_map(|y| std::slice::from_raw_parts(base.add(y * stride), w).to_vec()).collect()
            });
            CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
            rows.map(|r| (w, h, r))
        }
    }
}

pub struct Compositor {
    luma: Pipeline,
    chroma: Pipeline,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    slots: Vec<Arc<Slot>>,
    pub eye_width: u32,
    pub eye_height: u32,
}

// SAFETY: Metal pipeline, sampler and texture objects are thread-safe.
unsafe impl Send for Compositor {}
unsafe impl Sync for Compositor {}

#[derive(Debug)]
pub struct CompositorError(pub String);

const SLOTS: usize = 6;

impl Compositor {
    /// `eye_width` and `eye_height` must be even (4:2:0 chroma).
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        eye_width: u32,
        eye_height: u32,
    ) -> Result<Self, CompositorError> {
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .map_err(|e| CompositorError(format!("shader: {e}")))?;
        let pipeline =
            |fragment: &str, format: MTLPixelFormat| -> Result<Pipeline, CompositorError> {
                let function = |name: &str| {
                    library
                        .newFunctionWithName(&NSString::from_str(name))
                        .ok_or_else(|| CompositorError(name.into()))
                };
                let (v, f) = (function("eye_vertex")?, function(fragment)?);
                let descriptor = MTLRenderPipelineDescriptor::new();
                descriptor.setVertexFunction(Some(&v));
                descriptor.setFragmentFunction(Some(&f));
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

        let slots = (0..SLOTS)
            .map(|_| make_slot(device, eye_width * 2, eye_height))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            luma: pipeline("luma", MTLPixelFormat::R8Unorm)?,
            chroma: pipeline("chroma", MTLPixelFormat::RG8Unorm)?,
            sampler,
            slots,
            eye_width,
            eye_height,
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
        self.pass(&commands, &self.luma, &slot.luma, 1, &eyes, &views);
        self.pass(&commands, &self.chroma, &slot.chroma, 2, &eyes, &views);

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

    /// Draws both eyes side by side into one plane; `divisor` is the plane's subsampling.
    fn pass(
        &self,
        commands: &ProtocolObject<dyn MTLCommandBuffer>,
        pipeline: &Pipeline,
        target: &Texture,
        divisor: u32,
        eyes: &[EyeSource<'_>; 2],
        views: &[Option<(Retained<Texture>, bool)>],
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
}

/// A 2D-array view in the non-sRGB twin of the texture's format, so sampling returns the stored
/// bytes unchanged, and whether those bytes are linear and still need sRGB encoding.
fn byte_view(texture: &Texture) -> Option<(Retained<Texture>, bool)> {
    let (format, linear) = match texture.pixelFormat() {
        MTLPixelFormat::RGBA8Unorm_sRGB => (MTLPixelFormat::RGBA8Unorm, false),
        MTLPixelFormat::BGRA8Unorm_sRGB => (MTLPixelFormat::BGRA8Unorm, false),
        other => (other, true),
    };
    let slices = texture.arrayLength().max(1);
    // SAFETY: Tether creates every swapchain texture with PixelFormatView usage.
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
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
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
