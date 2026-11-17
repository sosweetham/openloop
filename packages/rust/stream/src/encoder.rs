//! VideoToolbox encoding configured for latency first: no frame reordering, speed over quality,
//! keyframes only on demand. Output is Annex-B with parameter sets on every keyframe so the
//! headset can join or recover at any keyframe. The measurements behind each setting are in
//! docs/ARCHITECTURE.md § Video quality.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMVideoCodecType_H264,
    kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVPixelBuffer, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_SpatialAdaptiveQPLevel, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTQPModulationLevel_Disable,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
}

impl Codec {
    fn video_codec_type(self) -> CMVideoCodecType {
        match self {
            Codec::Hevc => kCMVideoCodecType_HEVC,
            Codec::H264 => kCMVideoCodecType_H264,
        }
    }

    /// Whether a NAL unit starts a decodable sequence (HEVC IRAP 16..=23, H.264 IDR 5).
    fn is_keyframe_nal(self, nal: &[u8]) -> bool {
        nal.first().is_some_and(|b| match self {
            Codec::Hevc => (16..=23).contains(&((b >> 1) & 0x3f)),
            Codec::H264 => b & 0x1f == 5,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    /// Bits per second.
    pub bitrate: u32,
    /// VideoToolbox's low-latency rate control (`EnableLowLatencyRateControl`).
    pub low_latency: bool,
}

/// What travels with a frame through the encoder and comes back with its bitstream.
pub struct Pending<M> {
    pub meta: M,
    /// Runs once VideoToolbox no longer needs the source pixel buffer.
    pub release: Box<dyn FnOnce() + Send>,
}

pub struct Encoded<M> {
    pub meta: M,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

struct Context<M> {
    codec: Codec,
    sink: Box<dyn Fn(Option<Encoded<M>>) + Send + Sync>,
}

pub struct Encoder<M> {
    session: CFRetained<VTCompressionSession>,
    // Boxed so the callback's refcon stays valid for the session's lifetime.
    context: Box<Context<M>>,
    pub config: EncoderConfig,
    /// The warm-up frame took the session's first keyframe, so the first frame encoded after it
    /// is forced to be one, as a new session's first frame always is.
    first: AtomicBool,
}

// SAFETY: VTCompressionSession accepts encode calls from any thread; the sink is Send + Sync.
unsafe impl<M> Send for Encoder<M> {}
unsafe impl<M> Sync for Encoder<M> {}

#[derive(Debug)]
pub struct EncoderError(pub &'static str, pub i32);

impl<M: Send + 'static> Encoder<M> {
    /// `sink` receives every frame: `Some` when encoded, `None` when VideoToolbox dropped it.
    pub fn new(
        config: EncoderConfig,
        sink: impl Fn(Option<Encoded<M>>) + Send + Sync + 'static,
    ) -> Result<Self, EncoderError> {
        let context = Box::new(Context {
            codec: config.codec,
            sink: Box::new(sink),
        });
        let refcon = &*context as *const Context<M> as *mut c_void;
        let session = Self::create(&config, refcon)?;

        let set = |key: &CFString, value: &CFType| {
            // SAFETY: valid session, key and value of the documented types.
            unsafe { VTSessionSetProperty(&session, key, Some(value)) }
        };
        let yes = CFBoolean::new(true);
        let no = CFBoolean::new(false);
        // SAFETY: the keys and profile levels are VideoToolbox's static CFStrings.
        unsafe {
            let profile = match config.codec {
                Codec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel,
                Codec::H264 => kVTProfileLevel_H264_High_AutoLevel,
            };
            // In real-time mode VideoToolbox slows itself to the frame rate after about 3 s of a
            // stream: ~16 ms a frame from then on, against ~8.5 ms without it. The app paces
            // the frames, and the compositor's slots bound what can queue.
            set(kVTCompressionPropertyKey_RealTime, no.as_ref());
            set(kVTCompressionPropertyKey_AllowFrameReordering, no.as_ref());
            // Adaptive QP moves bits from detail to flat areas; off, edges and text keep them.
            // Low-latency rate control has none and rejects the property.
            if !config.low_latency {
                set(
                    kVTCompressionPropertyKey_SpatialAdaptiveQPLevel,
                    CFNumber::new_i32(kVTQPModulationLevel_Disable).as_ref(),
                );
            }
            set(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                yes.as_ref(),
            );
            set(kVTCompressionPropertyKey_ProfileLevel, profile.as_ref());
            set(
                kVTCompressionPropertyKey_AverageBitRate,
                CFNumber::new_i32(config.bitrate as i32).as_ref(),
            );
            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                CFNumber::new_f32(config.fps).as_ref(),
            );
            set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                CFNumber::new_i32(i32::MAX).as_ref(),
            );
            // Matches the compositor's BT.709 video-range conversion; written into the VUI.
            set(
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2.as_ref(),
            );
            set(
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2.as_ref(),
            );
            set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(),
            );
            session.prepare_to_encode_frames();
        }
        let encoder = Self {
            session,
            context,
            config,
            first: AtomicBool::new(true),
        };
        encoder.warm_up();
        Ok(encoder)
    }

    /// Encodes one blank frame and drops it. A session's first frame takes several frame
    /// periods (70 ms at 3552x1856 on an M3 Max), and the frames behind it queue past the
    /// compositor's slots; here it is paid while the stream is set up instead.
    fn warm_up(&self) {
        let Ok(blank) = crate::compositor::iosurface_pixel_buffer(
            self.config.width,
            self.config.height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        ) else {
            return;
        };
        // SAFETY: valid session and buffer. A null source refcon tells `output` to drop the
        // frame; its time is before any stream's first.
        unsafe {
            let status = self.session.encode_frame(
                &blank,
                CMTime::new(-1, 1),
                CMTime::new(0, 0),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if status == 0 {
                self.session.complete_frames(CMTime::new(0, 0));
            }
        }
    }

    fn create(
        config: &EncoderConfig,
        refcon: *mut c_void,
    ) -> Result<CFRetained<VTCompressionSession>, EncoderError> {
        let yes = CFBoolean::new(true);
        // SAFETY: the specification keys are VideoToolbox's static CFStrings.
        let spec = unsafe {
            let mut keys: Vec<&CFString> =
                vec![kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder];
            if config.low_latency {
                keys.push(kVTVideoEncoderSpecification_EnableLowLatencyRateControl);
            }
            let values: Vec<&CFBoolean> = keys.iter().map(|_| &*yes).collect();
            CFDictionary::from_slices(&keys, &values)
        };
        let mut out: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: all pointers are valid; the refcon outlives the session (owned by Encoder).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                config.codec.video_codec_type(),
                Some(spec.as_opaque()),
                None,
                None,
                Some(output::<M>),
                refcon,
                NonNull::from(&mut out),
            )
        };
        match NonNull::new(out) {
            // SAFETY: a successful create returns a +1 reference.
            Some(session) if status == 0 => Ok(unsafe { CFRetained::from_raw(session) }),
            _ => Err(EncoderError("VTCompressionSessionCreate", status)),
        }
    }

    pub fn encode(&self, frame: &CVPixelBuffer, time_ns: i64, keyframe: bool, pending: Pending<M>) {
        let keyframe = self.first.swap(false, Ordering::Relaxed) || keyframe;
        let yes = CFBoolean::new(true);
        // SAFETY: static CFString key.
        let props = keyframe.then(|| {
            CFDictionary::from_slices(
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[&*yes],
            )
        });
        let refcon = Box::into_raw(Box::new(pending)) as *mut c_void;
        // SAFETY: valid session and buffer; ownership of `refcon` passes to the callback.
        let status = unsafe {
            self.session.encode_frame(
                frame,
                CMTime::new(time_ns, 1_000_000_000),
                CMTime::new(0, 0),
                props.as_deref().map(|d| d.as_opaque()),
                refcon,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            // The callback never runs for a frame VideoToolbox refused.
            // SAFETY: `refcon` was leaked above and not handed over.
            let pending = unsafe { Box::from_raw(refcon as *mut Pending<M>) };
            (pending.release)();
            (self.context.sink)(None);
        }
    }
}

impl<M> Drop for Encoder<M> {
    fn drop(&mut self) {
        // SAFETY: invalidation stops callbacks before the context is freed.
        unsafe { self.session.invalidate() };
    }
}

unsafe extern "C-unwind" fn output<M>(
    refcon: *mut c_void,
    source: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if source.is_null() {
        return;
    }
    // SAFETY: `refcon` is the Encoder's boxed context; `source` is the Pending leaked in `encode`.
    let context = unsafe { &*(refcon as *const Context<M>) };
    let Pending { meta, release } = *unsafe { Box::from_raw(source as *mut Pending<M>) };
    release();
    let encoded = (status == 0)
        .then_some(unsafe { sample.as_ref() })
        .flatten()
        .and_then(|s| unsafe { annex_b(s, context.codec) })
        .map(|(data, keyframe)| Encoded {
            meta,
            keyframe,
            data,
        });
    (context.sink)(encoded);
}

/// Converts VideoToolbox's length-prefixed output to Annex-B, adding parameter sets on keyframes.
unsafe fn annex_b(sample: &CMSampleBuffer, codec: Codec) -> Option<(Vec<u8>, bool)> {
    const START: [u8; 4] = [0, 0, 0, 1];
    // SAFETY: valid sample buffer from the encoder callback.
    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if unsafe { block.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr().cast())?) } != 0 {
        return None;
    }

    let mut nals = Vec::new();
    let mut rest = avcc.as_slice();
    while let Some((head, tail)) = rest.split_first_chunk::<4>() {
        let n = u32::from_be_bytes(*head) as usize;
        nals.push(tail.get(..n)?);
        rest = &tail[n..];
    }
    let keyframe = nals.iter().any(|n| codec.is_keyframe_nal(n));

    let mut out = Vec::with_capacity(len + 256);
    if keyframe {
        let format = unsafe { sample.format_description() }?;
        for set in unsafe { parameter_sets(&format, codec) } {
            out.extend_from_slice(&START);
            out.extend_from_slice(set);
        }
    }
    for nal in nals {
        out.extend_from_slice(&START);
        out.extend_from_slice(nal);
    }
    Some((out, keyframe))
}

/// VPS/SPS/PPS (HEVC) or SPS/PPS (H.264), borrowed from the format description.
unsafe fn parameter_sets(format: &CMFormatDescription, codec: Codec) -> Vec<&[u8]> {
    let mut sets = Vec::new();
    let mut count = 1usize;
    let mut index = 0;
    while index < count {
        let (mut ptr, mut size) = (ptr::null(), 0usize);
        // SAFETY: valid format description of the matching codec; out-pointers are locals.
        let status = unsafe {
            match codec {
                Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    format,
                    index,
                    &mut ptr,
                    &mut size,
                    &mut count,
                    ptr::null_mut(),
                ),
                Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format,
                    index,
                    &mut ptr,
                    &mut size,
                    &mut count,
                    ptr::null_mut(),
                ),
            }
        };
        if status != 0 || ptr.is_null() {
            break;
        }
        // SAFETY: the bytes live as long as the format description.
        sets.push(unsafe { std::slice::from_raw_parts(ptr, size) });
        index += 1;
    }
    sets
}
