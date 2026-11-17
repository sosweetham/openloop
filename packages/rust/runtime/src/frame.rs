//! Turns a submitted projection layer into a pose-tagged encoded frame for the headset. Until
//! R2 S3 the app's process composites and encodes; `tetherd` takes each access unit from the
//! session's shared ring and sends it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_metal::MTLCommandQueue;
use tether_ipc::records::AccessUnit;
use tether_openxr_abi::{
    XrCompositionLayerBaseHeader, XrCompositionLayerProjection, XrCompositionLayerProjectionView,
    XrResult, XrStructureType,
};
use tether_protocol::{Message, VideoFrame, View};
use tether_stream::compositor::{Compositor, EyeSource};
use tether_stream::encoder::{Codec, Encoded, Encoder, EncoderConfig, Pending};
use tether_stream::time::now_ns;
use tether_stream::{Headset, shared};

use crate::convert;
use crate::handles::id;
use crate::link::{SessionLink, TetherdSession};
use crate::session::Session;
use crate::state::OBJECTS;

/// USB has bandwidth to spare; Wi-Fi will adapt this per link.
const USB_BITRATE: u32 = 80_000_000;

struct FrameMeta {
    index: u64,
    /// On the headset's clock in-process; on the Mac's for `tetherd`, which maps it as it sends.
    display_time_ns: i64,
    views: [View; 2],
}

pub struct Streamer {
    compositor: Compositor,
    encoder: Arc<Encoder<FrameMeta>>,
    eye: (u32, u32),
    next_index: u64,
    /// Set when a frame is lost after encoding began (VideoToolbox dropped it, or `tetherd`'s
    /// ring was full), which breaks the reference chain.
    needs_keyframe: Arc<AtomicBool>,
    /// The session's keyframe request count last answered.
    keyframe_requests: u64,
}

impl Streamer {
    fn new(session: &Session, eye: (u32, u32)) -> Result<Self, XrResult> {
        let device = session.queue.0.device();
        let compositor = Compositor::new(&device, eye.0, eye.1).map_err(|e| {
            crate::log!("compositor: {e:?}");
            XrResult::XR_ERROR_RUNTIME_FAILURE
        })?;
        let display = session.link.display();
        let codec = if display.is_none_or(|(_, hevc)| hevc) {
            Codec::Hevc
        } else {
            Codec::H264
        };
        let fps = crate::system::refresh_rate(display.map(|(rate, _)| rate));
        let needs_keyframe = Arc::new(AtomicBool::new(true));
        let config = EncoderConfig {
            codec,
            width: eye.0 * 2,
            height: eye.1,
            fps,
            bitrate: USB_BITRATE,
            low_latency: false,
        };
        let sink = Sink::of(&session.link, (config.width, config.height));
        let lost = needs_keyframe.clone();
        let encoder = Encoder::new(config, move |out: Option<Encoded<FrameMeta>>| {
            if !out.is_some_and(|encoded| sink.send(encoded)) {
                lost.store(true, Ordering::Release);
            }
        })
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
            keyframe_requests: session.link.status().keyframe_requests,
        })
    }
}

/// Where encoded frames go.
enum Sink {
    Headset(Arc<Headset>),
    Tetherd {
        session: Arc<TetherdSession>,
        video: (u32, u32),
    },
}

impl Sink {
    fn of(link: &SessionLink, video: (u32, u32)) -> Sink {
        match link {
            SessionLink::InProcess(headset) => Sink::Headset(headset.clone()),
            SessionLink::Tetherd(session) => Sink::Tetherd {
                session: session.clone(),
                video,
            },
        }
    }

    /// `false` when the frame was lost on the way.
    fn send(&self, encoded: Encoded<FrameMeta>) -> bool {
        let meta = encoded.meta;
        match self {
            Sink::Headset(headset) => {
                headset.send(&Message::VideoFrame(VideoFrame {
                    frame_index: meta.index,
                    display_time_ns: meta.display_time_ns,
                    keyframe: encoded.keyframe,
                    views: meta.views,
                    data: encoded.data,
                }));
                true
            }
            Sink::Tetherd { session, video } => {
                let unit = AccessUnit {
                    frame_index: meta.index,
                    display_mac_ns: meta.display_time_ns,
                    encoded_mac_ns: now_ns(),
                    views: meta.views.map(|v| shared::record_view(&v)),
                    width: video.0,
                    height: video.1,
                    flags: if encoded.keyframe {
                        AccessUnit::KEYFRAME
                    } else {
                        0
                    },
                    len: 0,
                };
                session.push_access_unit(&unit, &encoded.data)
            }
        }
    }
}

/// Streams the frame's projection layer if a headset is ready for video. Other layer types are
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

    let status = session.link.status();
    if !status.linked {
        // Whatever the headset decoded before the link went is gone with it.
        if let Some(streamer) = session
            .streamer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            streamer.needs_keyframe.store(true, Ordering::Release);
        }
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
    let Some(streamer) = streamer.as_mut().filter(|s| s.eye == eye) else {
        // Building the compositor and encoder takes about 100 ms, so this frame's display time
        // has passed; the next one, predicted after the stall, starts the stream.
        *streamer = Some(Streamer::new(session, eye)?);
        return Ok(());
    };

    let (display_time_ns, requested) = match &session.link {
        SessionLink::InProcess(headset) => {
            let video = (eye.0 * 2, eye.1);
            let new_stream = headset.state().video_size != Some(video);
            if new_stream {
                headset.announce_video_size(video.0, video.1);
            }
            let mut state = headset.state();
            let requested = std::mem::take(&mut state.keyframe_requested) || new_stream;
            (state.clock.to_client(display_time), requested)
        }
        SessionLink::Tetherd(_) => {
            let requests = status.keyframe_requests;
            let requested = requests != streamer.keyframe_requests;
            streamer.keyframe_requests = requests;
            (display_time, requested)
        }
    };
    let keyframe = requested || streamer.needs_keyframe.swap(false, Ordering::AcqRel);

    let meta = FrameMeta {
        index: streamer.next_index,
        display_time_ns,
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
    let composed = streamer.compositor.compose(queue, eyes, move |slot| {
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
