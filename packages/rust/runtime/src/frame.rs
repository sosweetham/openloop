//! Turns a submitted projection layer into a pose-tagged video frame for the headset.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_metal::MTLCommandQueue;
use tether_openxr_abi::{
    XrCompositionLayerBaseHeader, XrCompositionLayerProjection, XrCompositionLayerProjectionView,
    XrResult, XrStructureType,
};
use tether_protocol::{Message, VideoFrame, View};
use tether_stream::compositor::{Compositor, EyeSource};
use tether_stream::encoder::{Codec, Encoder, EncoderConfig, Pending};
use tether_stream::time::now_ns;

use crate::convert;
use crate::handles::id;
use crate::session::Session;
use crate::state::{OBJECTS, headset};

/// USB has bandwidth to spare; Wi-Fi will adapt this per link.
const USB_BITRATE: u32 = 80_000_000;

struct FrameMeta {
    index: u64,
    display_time_ns: i64,
    views: [View; 2],
}

pub struct Streamer {
    compositor: Compositor,
    encoder: Arc<Encoder<FrameMeta>>,
    eye: (u32, u32),
    next_index: u64,
    /// Set when VideoToolbox drops a frame, which breaks the reference chain.
    needs_keyframe: Arc<AtomicBool>,
}

impl Streamer {
    fn new(session: &Session, eye: (u32, u32)) -> Result<Self, XrResult> {
        let device = session.queue.0.device();
        let compositor = Compositor::new(&device, eye.0, eye.1).map_err(|e| {
            crate::log!("compositor: {e:?}");
            XrResult::XR_ERROR_RUNTIME_FAILURE
        })?;
        let (codec, fps) = {
            let state = headset().state();
            let profile = state.profile.as_ref();
            let codec = if profile.is_none_or(|p| p.supports_hevc) {
                Codec::Hevc
            } else {
                Codec::H264
            };
            (codec, profile.map_or(90.0, |p| p.refresh_rate))
        };
        let needs_keyframe = Arc::new(AtomicBool::new(true));
        let sink_headset = headset().clone();
        let sink_flag = needs_keyframe.clone();
        let config = EncoderConfig {
            codec,
            width: eye.0 * 2,
            height: eye.1,
            fps,
            bitrate: USB_BITRATE,
            low_latency: false,
        };
        let encoder = Encoder::new(
            config,
            move |out: Option<tether_stream::encoder::Encoded<FrameMeta>>| match out {
                Some(e) => {
                    sink_headset.send(&Message::VideoFrame(VideoFrame {
                        frame_index: e.meta.index,
                        display_time_ns: e.meta.display_time_ns,
                        keyframe: e.keyframe,
                        views: e.meta.views,
                        data: e.data,
                    }));
                }
                None => sink_flag.store(true, Ordering::Release),
            },
        )
        .map_err(|e| {
            crate::log!("encoder: {e:?}");
            XrResult::XR_ERROR_RUNTIME_FAILURE
        })?;
        crate::log!("streaming {}x{} {codec:?} @ {fps} Hz", eye.0 * 2, eye.1);
        Ok(Self {
            compositor,
            encoder: Arc::new(encoder),
            eye,
            next_index: 0,
            needs_keyframe,
        })
    }
}

/// Streams the frame's projection layer if a headset is connected. Other layer types are
/// accepted and not yet composited.
pub fn submit(
    session: &Session,
    layers: &[*const XrCompositionLayerBaseHeader],
    display_time: i64,
) -> Result<(), XrResult> {
    // SAFETY: the app passes valid layer structs; each starts with a base header.
    let projection = layers
        .iter()
        .filter_map(|&l| unsafe { l.as_ref() })
        .find_map(|l| {
            (l.type_ == XrStructureType::XR_TYPE_COMPOSITION_LAYER_PROJECTION)
                .then(|| unsafe { &*(l as *const _ as *const XrCompositionLayerProjection) })
        });
    let Some(projection) = projection else {
        return Ok(());
    };
    // SAFETY: the spec requires `viewCount` valid views.
    let views: &[XrCompositionLayerProjectionView] =
        unsafe { crate::ffi::slice(projection.views, projection.viewCount) }?;
    let [left, right] = views else {
        return Err(XrResult::XR_ERROR_VALIDATION_FAILURE);
    };

    let (connected, clock_to_client) = {
        let state = headset().state();
        (state.connected(), state.clock.to_client(display_time))
    };
    if !connected {
        return Ok(());
    }

    let space = OBJECTS
        .spaces
        .get(id(projection.space))
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
    let Some(space_pose) = crate::space::pose_in_tracking(&space, session, display_time) else {
        return Ok(());
    };
    let swapchains = [left, right].map(|v| {
        OBJECTS
            .swapchains
            .get(id(v.subImage.swapchain))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)
    });
    let [Ok(left_chain), Ok(right_chain)] = swapchains else {
        return Err(XrResult::XR_ERROR_HANDLE_INVALID);
    };
    let (Some(left_image), Some(right_image)) = (left_chain.released(), right_chain.released())
    else {
        return Err(XrResult::XR_ERROR_LAYER_INVALID);
    };

    let rect = |v: &XrCompositionLayerProjectionView| {
        let r = v.subImage.imageRect;
        [r.offset.x, r.offset.y, r.extent.width, r.extent.height]
    };
    let eye = (
        (left.subImage.imageRect.extent.width as u32) & !1,
        (left.subImage.imageRect.extent.height as u32) & !1,
    );
    let tracked_views = [left, right].map(|v| View {
        pose: (space_pose * convert::rigid(&v.pose)).to_pose(),
        fov: convert::fov(&v.fov),
    });

    let mut streamer = session.streamer.lock().unwrap_or_else(|e| e.into_inner());
    if streamer.as_ref().is_none_or(|s| s.eye != eye) {
        *streamer = Some(Streamer::new(session, eye)?);
    }
    let streamer = streamer.as_mut().expect("just created");

    let video = (eye.0 * 2, eye.1);
    let new_stream = headset().state().video_size != Some(video);
    if new_stream {
        headset().announce_video_size(video.0, video.1);
    }
    let requested = std::mem::take(&mut headset().state().keyframe_requested);
    let keyframe = new_stream || requested || streamer.needs_keyframe.swap(false, Ordering::AcqRel);

    let meta = FrameMeta {
        index: streamer.next_index,
        display_time_ns: clock_to_client,
        views: tracked_views,
    };
    streamer.next_index += 1;
    let encoder = streamer.encoder.clone();
    let eyes = [
        EyeSource {
            texture: &left_image,
            slice: left.subImage.imageArrayIndex,
            rect: rect(left),
        },
        EyeSource {
            texture: &right_image,
            slice: right.subImage.imageArrayIndex,
            rect: rect(right),
        },
    ];
    let queue: &objc2::runtime::ProtocolObject<dyn MTLCommandQueue> = &session.queue.0;
    let dump = (meta.index == DUMP_FRAME).then(|| std::env::var_os("TETHER_DUMP_FRAME")).flatten();
    let composed = streamer.compositor.compose(queue, eyes, move |slot| {
        if let Some(path) = dump {
            dump_luma(&slot, &path);
        }
        let release = slot.clone();
        encoder.encode(
            &slot.pixel_buffer,
            now_ns(),
            keyframe,
            Pending {
                meta,
                release: Box::new(move || release.release()),
            },
        );
    });
    if !composed && keyframe {
        streamer.needs_keyframe.store(true, Ordering::Release);
    }
    Ok(())
}

/// With `TETHER_DUMP_FRAME=<path>`, this frame's composed luma is written as a PGM image.
const DUMP_FRAME: u64 = 90;

fn dump_luma(slot: &tether_stream::compositor::Slot, path: &std::ffi::OsStr) {
    let Some((w, h, rows)) = slot.luma() else { return };
    let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
    pgm.extend_from_slice(&rows);
    match std::fs::write(path, pgm) {
        Ok(()) => crate::log!("dumped frame {DUMP_FRAME} luma to {}", path.to_string_lossy()),
        Err(e) => crate::log!("frame dump failed: {e}"),
    }
}
