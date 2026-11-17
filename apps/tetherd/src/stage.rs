//! What the headset shows and plays: one session's video and sound at a time, the session begun
//! last. The app still encodes until R2 S3; `tetherd` puts each access unit's display time on
//! the headset's clock, opens every stream with a `ServerHello` and a keyframe, and ends it with
//! `StreamEnded`. Sound is timed the same way (docs/ARCHITECTURE.md § Audio).

use std::sync::{Arc, Mutex, PoisonError};

use tether_ipc::now_ns;
use tether_ipc::records::{self, AccessUnit};
use tether_protocol::{
    AudioFormat, AudioHeader, ClientTime, Fov, Haptics, MAX_AUDIO_CHANNELS, Message, Pose,
    VideoHeader, View,
};
use tether_stream::{Headset, HeadsetState, PIPELINE_LATENCY_NS};

use crate::daemon::Hub;
use crate::session::{Session, Video};

/// A frame forwarded this long after its display time is a backlog left by a stall, which the
/// headset could only show out of step; the stream drops it and resumes at a keyframe.
const STALE_NS: i64 = 500_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forwarded {
    Sent,
    /// The stream cannot start, or go on after a loss, until the app sends a keyframe.
    AwaitingKeyframe,
    /// Past its display time by more than [`STALE_NS`]; the stream needs a keyframe after it.
    Stale,
    /// No headset ready for video, or its link failed during the send.
    NoHeadset,
    /// Another session is in front.
    Background,
    /// The session is not between `xrBeginSession` and `xrEndSession`.
    NotRunning,
}

/// What became of one packet of sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Played {
    Sent,
    /// No headset ready for it, or its link failed during the send.
    NoHeadset,
    /// Another session is in front, or none is.
    Background,
}

pub struct Stage {
    pub headset: Arc<Headset>,
    hub: Arc<Hub>,
    /// The session whose stream the headset was last told about. Held across each send, so
    /// two sessions' frames never interleave on the link.
    streaming: Mutex<Option<u64>>,
}

impl Stage {
    pub fn new(headset: Arc<Headset>, hub: Arc<Hub>) -> Stage {
        Stage {
            headset,
            hub,
            streaming: Mutex::new(None),
        }
    }

    fn foreground(&self) -> Option<u64> {
        self.hub.foreground()
    }

    pub fn forward(
        &self,
        session: &Session,
        unit: &AccessUnit,
        data: &[u8],
        video: &mut Video,
    ) -> Forwarded {
        let mut streaming = self
            .streaming
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.foreground() != Some(session.id) {
            video.needs_keyframe = true;
            return Forwarded::Background;
        }
        let size = (unit.width, unit.height);
        let (ready, announced, display_time_ns) = {
            let state = self.headset.state();
            (
                state.ready_for_video(),
                *streaming == Some(session.id) && state.video_size == Some(size),
                state.clock.to_client(unit.display_mac_ns),
            )
        };
        if !ready {
            video.needs_keyframe = true;
            return Forwarded::NoHeadset;
        }
        if now_ns().saturating_sub(unit.display_mac_ns) > STALE_NS {
            video.needs_keyframe = true;
            return Forwarded::Stale;
        }
        if !announced || video.needs_keyframe {
            if !unit.keyframe() {
                return Forwarded::AwaitingKeyframe;
            }
            if !announced {
                if !self.headset.start_stream(size.0, size.1) {
                    return Forwarded::NoHeadset;
                }
                *streaming = Some(session.id);
            }
            video.needs_keyframe = false;
        }
        let header = VideoHeader {
            frame_index: unit.frame_index,
            display_time_ns,
            keyframe: unit.keyframe(),
            views: unit.views.map(view),
        };
        if self.headset.send_video(&header, data) {
            Forwarded::Sent
        } else {
            video.needs_keyframe = true;
            Forwarded::NoHeadset
        }
    }

    /// Sends `StreamEnded` if the headset shows `session`'s stream.
    pub fn end_stream_of(&self, session: u64) {
        let mut streaming = self
            .streaming
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *streaming == Some(session) {
            self.headset.end_stream();
            *streaming = None;
        }
    }

    /// The rate and channel count the headset plays, while it can be sent sound.
    pub fn audio_output(&self) -> Option<(u32, u8)> {
        let state = self.headset.state();
        output(&state).filter(|_| state.ready_for_video())
    }

    /// Sends one packet of `session`'s sound, captured from `host_time_ns`. It is to be heard
    /// [`PIPELINE_LATENCY_NS`] later on the headset's clock, as a frame rendered for that
    /// instant is displayed, and through the same clock mapping as [`Stage::forward`].
    pub fn audio(
        &self,
        session: u64,
        format: AudioFormat,
        sequence: u32,
        first_frame: u64,
        host_time_ns: i64,
        samples: &[i16],
    ) -> Played {
        if self.foreground() != Some(session) {
            return Played::Background;
        }
        let (ready, announced, present_at) = {
            let state = self.headset.state();
            (
                output(&state) == Some((format.sample_rate, format.channels))
                    && state.ready_for_video(),
                state.audio == Some(format),
                state
                    .clock
                    .to_client(host_time_ns.saturating_add(PIPELINE_LATENCY_NS)),
            )
        };
        if !ready || (!announced && !self.headset.start_audio(format)) {
            return Played::NoHeadset;
        }
        let header = AudioHeader {
            format,
            sequence,
            first_frame,
            present_at: ClientTime(present_at),
        };
        if self.headset.send_audio(&header, samples) {
            Played::Sent
        } else {
            Played::NoHeadset
        }
    }

    /// Sends `AudioStop` if the headset plays `stream`.
    pub fn end_audio(&self, stream: u16) {
        self.headset.end_audio(stream);
    }

    /// Only the session in front may vibrate the controllers.
    pub fn haptic(&self, session: &Session, haptics: &Haptics) -> bool {
        self.foreground() == Some(session.id) && self.headset.send(&Message::Haptics(*haptics))
    }
}

/// What the headset's profile says it plays; `None` for a headset without audio.
fn output(state: &HeadsetState) -> Option<(u32, u8)> {
    let profile = state.profile.as_ref()?;
    let channels = profile.audio_channels.min(MAX_AUDIO_CHANNELS);
    (profile.audio_sample_rate > 0 && channels > 0).then_some((profile.audio_sample_rate, channels))
}

fn view(v: records::View) -> View {
    View {
        pose: Pose {
            position: v.pose.position,
            orientation: v.pose.orientation,
        },
        fov: Fov {
            left: v.fov.left,
            right: v.fov.right,
            up: v.fov.up,
            down: v.fov.down,
        },
    }
}
