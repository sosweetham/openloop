//! The connected headset as the Mac sees it: its profile, latest tracking, clock mapping and
//! requests. One connection at a time; a new headset replaces the old one.

use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use openloop_protocol::{
    AudioFormat, AudioHeader, ControllerState, DeviceProfile, Message, PROTOCOL_VERSION,
    ServerHello, Tracking, VideoHeader, View,
};

use crate::link::{Link, Transport};
use crate::pose::{self, Rigid};
use crate::time::{ClockSync, now_ns};

/// Predicted Mac-to-photon time for a frame released now: compose and encode, the USB hop,
/// decode and the headset's own frame. A frame's display time and an audio packet's presentation
/// time are both this long after the Mac-side instant they belong to. Tuned against
/// measurements in R2 S4 and R6.
pub const PIPELINE_LATENCY_NS: i64 = 40_000_000;

/// Extrapolation beyond this is guesswork; poses are held instead.
const MAX_PREDICTION_NS: i64 = 100_000_000;
const PING_INTERVAL: Duration = Duration::from_millis(250);

/// The latest tracking sample and the predictions made from it: every pose an app sees.
#[derive(Clone, Debug, Default)]
pub struct Tracked {
    /// The sample and its time on the Mac clock.
    pub sample: Option<(Tracking, i64)>,
    /// Tracking is arriving on the current link. Without it the last pose is held.
    pub live: bool,
}

impl Tracked {
    pub fn head_at(&self, time_ns: i64) -> Rigid {
        self.extrapolate(time_ns, |t| (Rigid::from_pose(&t.head), t.head_velocity))
            .unwrap_or(pose::DEFAULT_HEAD)
    }

    /// Eyes relative to the head.
    pub fn views(&self) -> [View; 2] {
        self.sample
            .as_ref()
            .map_or_else(pose::default_views, |(t, _)| t.views)
    }

    pub fn controller(&self, hand: usize) -> Option<ControllerState> {
        self.sample
            .as_ref()
            .and_then(|(t, _)| t.controllers.get(hand).copied())
            .filter(|c| c.active && self.live)
    }

    /// Grip and aim poses at `time_ns`, both moved by the grip's velocity.
    pub fn hand_at(&self, hand: usize, time_ns: i64) -> Option<(Rigid, Rigid)> {
        let c = self.controller(hand)?;
        let grip = Rigid::from_pose(&c.grip);
        let predicted = self.extrapolate(time_ns, |_| (grip, c.grip_velocity))?;
        let delta = predicted * grip.inverse();
        Some((predicted, delta * Rigid::from_pose(&c.aim)))
    }

    fn extrapolate(
        &self,
        time_ns: i64,
        f: impl FnOnce(&Tracking) -> (Rigid, openloop_protocol::Velocity),
    ) -> Option<Rigid> {
        let (sample, sample_ns) = self.sample.as_ref()?;
        let (pose, velocity) = f(sample);
        let dt = if self.live {
            time_ns
                .saturating_sub(*sample_ns)
                .clamp(0, MAX_PREDICTION_NS)
        } else {
            0
        };
        Some(pose::predict(pose, &velocity, dt as f32 * 1e-9))
    }
}

#[derive(Default)]
pub struct HeadsetState {
    pub profile: Option<DeviceProfile>,
    pub tracked: Tracked,
    pub clock: ClockSync,
    /// Increments on every system recenter the headset reports.
    pub recenters: u64,
    pub keyframe_requested: bool,
    /// Encoded video size the client was last told about.
    pub video_size: Option<(u32, u32)>,
    /// The audio stream the client was last told about.
    pub audio: Option<AudioFormat>,
    /// A link is up and its client said hello.
    greeted: bool,
    /// OpenLoop's menu holds input focus on the headset. Cleared with the link, so a lost headset
    /// never leaves an app unfocused.
    focus_lost: bool,
}

impl HeadsetState {
    /// Tracking is arriving.
    pub fn connected(&self) -> bool {
        self.tracked.live
    }

    /// Video can be sent: tracking is arriving and its clock is mapped, so display times can be
    /// put on the headset's clock.
    pub fn ready_for_video(&self) -> bool {
        self.tracked.live && self.clock.is_synced()
    }

    /// The current link's client has said hello.
    pub fn greeted(&self) -> bool {
        self.greeted
    }

    /// Whether the app should have input focus.
    pub fn app_focused(&self) -> bool {
        !self.focus_lost
    }

    /// Forgets the link but keeps the last pose, so apps hold still instead of snapping to
    /// the default viewer, and keeps the recenter count monotonic for sessions watching it.
    fn retire(&mut self) {
        *self = Self {
            tracked: Tracked {
                sample: self.tracked.sample.take(),
                live: false,
            },
            profile: self.profile.take(),
            recenters: self.recenters,
            ..Self::default()
        };
    }
}

/// A change the link thread made to the [`HeadsetState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// A headset connected; it has not said hello yet.
    Linked,
    /// The current link ended.
    Unlinked,
    /// The client's hello arrived with its device profile.
    Profile,
    /// The client speaks another protocol version; its link was shut.
    Refused {
        version: u16,
    },
    Tracking,
    Clock,
    Recenter,
    KeyframeRequest,
    Focus,
}

/// Told about every change on the link thread, with the state still locked, so it sees each one
/// in order. It must not call back into the [`Headset`].
pub trait Observer: Send + Sync {
    fn changed(&self, state: &HeadsetState, change: Change);
}

/// Owns the transport and the current headset link; shared between its owner and its threads.
pub struct Headset {
    state: Mutex<HeadsetState>,
    link: Mutex<Option<Arc<dyn Link>>>,
    observer: Option<Arc<dyn Observer>>,
}

impl Headset {
    /// A headset that never connects; the runtime still runs, rendering for the default viewer.
    pub fn detached() -> Arc<Self> {
        Self::observed(None)
    }

    /// A headset that connects once [`Headset::accept`] is given a transport.
    pub fn observed(observer: Option<Arc<dyn Observer>>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            link: Mutex::new(None),
            observer,
        })
    }

    pub fn start(transport: Box<dyn Transport>) -> Arc<Self> {
        let headset = Self::detached();
        headset.accept(transport);
        headset
    }

    /// Accepts headsets from `transport` on its own thread for as long as the `Headset` lives.
    pub fn accept(self: &Arc<Self>, mut transport: Box<dyn Transport>) {
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("openloop-accept".into())
            .spawn(move || {
                while let Ok(link) = transport.accept() {
                    let Some(headset) = weak.upgrade() else {
                        return;
                    };
                    // Each link is served on its own thread so a relaunched headset is
                    // accepted at once; `serve` retires whichever link it replaces.
                    let link: Arc<dyn Link> = Arc::from(link);
                    thread::Builder::new()
                        .name("openloop-link".into())
                        .spawn(move || headset.serve(link))
                        .expect("spawning a link thread");
                }
            })
            .expect("spawning the accept thread");
    }

    pub fn state(&self) -> MutexGuard<'_, HeadsetState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn current(&self) -> Option<Arc<dyn Link>> {
        self.link.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sends to the current headset; dropped when none is connected.
    pub fn send(&self, message: &Message) -> bool {
        self.current()
            .is_some_and(|link| link.send(message).is_ok())
    }

    /// Sends one video frame without copying its access unit; dropped when no headset is
    /// connected.
    pub fn send_video(&self, header: &VideoHeader, data: &[u8]) -> bool {
        self.current()
            .is_some_and(|link| link.send_video(header, data).is_ok())
    }

    pub fn announce_video_size(&self, width: u32, height: u32) {
        if self.state().video_size == Some((width, height)) {
            return;
        }
        self.start_stream(width, height);
    }

    /// Tells the headset a new stream of this size starts, whatever it was showing before.
    pub fn start_stream(&self, width: u32, height: u32) -> bool {
        let hello = Message::ServerHello(ServerHello {
            version: PROTOCOL_VERSION,
            video_width: width,
            video_height: height,
        });
        let sent = self.send(&hello);
        self.state().video_size = sent.then_some((width, height));
        sent
    }

    /// Tells the headset the current stream is over, so it shows its idle state.
    pub fn end_stream(&self) {
        if self.state().video_size.take().is_some() {
            self.send(&Message::StreamEnded);
        }
    }

    /// Tells the headset an audio stream starts, replacing any other.
    pub fn start_audio(&self, format: AudioFormat) -> bool {
        let sent = self.send(&Message::AudioStart(format));
        self.state().audio = sent.then_some(format);
        sent
    }

    /// Sends one audio packet without copying its samples; dropped when no headset is connected.
    pub fn send_audio(&self, header: &AudioHeader, samples: &[i16]) -> bool {
        self.current()
            .is_some_and(|link| link.send_audio(header, samples).is_ok())
    }

    /// Tells the headset `stream` is over, if it is the one it was told about.
    pub fn end_audio(&self, stream: u16) {
        let mut state = self.state();
        if state.audio.is_some_and(|format| format.stream == stream) {
            state.audio = None;
            drop(state);
            self.send(&Message::AudioStop { stream });
        }
    }

    fn serve(self: &Arc<Self>, link: Arc<dyn Link>) {
        if let Some(old) = self
            .link
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(link.clone())
        {
            old.shutdown();
        }
        {
            let mut state = self.state();
            state.retire();
            self.notify(&state, Change::Linked);
        }

        let pinger = Arc::downgrade(&link);
        thread::Builder::new()
            .name("openloop-ping".into())
            .spawn(move || {
                while let Some(link) = pinger.upgrade() {
                    if link.send(&Message::ClockPing { mac_ns: now_ns() }).is_err() {
                        return;
                    }
                    drop(link);
                    thread::sleep(PING_INTERVAL);
                }
            })
            .expect("spawning the ping thread");

        while let Ok(message) = link.recv() {
            self.handle(&link, message);
        }
        // A writer blocked on a peer that stopped reading returns once the socket is shut.
        link.shutdown();
        let mut current = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &link)) {
            *current = None;
            drop(current);
            let mut state = self.state();
            state.retire();
            self.notify(&state, Change::Unlinked);
        }
    }

    fn notify(&self, state: &HeadsetState, change: Change) {
        if let Some(observer) = &self.observer {
            observer.changed(state, change);
        }
    }

    fn handle(&self, link: &Arc<dyn Link>, message: Message) {
        let now = now_ns();
        let mut state = self.state();
        let change = match message {
            Message::ClientHello(hello) if hello.version == PROTOCOL_VERSION => {
                state.profile = Some(hello.profile);
                state.greeted = true;
                Change::Profile
            }
            Message::ClientHello(hello) => {
                link.shutdown();
                Change::Refused {
                    version: hello.version,
                }
            }
            Message::ClockPong { mac_ns, client_ns } => {
                state.clock.observe(mac_ns, client_ns, now);
                Change::Clock
            }
            // A pose that is not finite and unit-length would reach the app as NaN.
            Message::Tracking(tracking) if !tracking_is_sane(&tracking) => return,
            Message::Tracking(tracking) => {
                let at = if state.clock.is_synced() {
                    state.clock.to_mac(tracking.sample_time_ns)
                } else {
                    now
                };
                state.tracked = Tracked {
                    sample: Some((*tracking, at)),
                    live: true,
                };
                Change::Tracking
            }
            Message::ClockPing { mac_ns } => {
                drop(state);
                let _ = link.send(&Message::ClockPong {
                    mac_ns,
                    client_ns: now,
                });
                return;
            }
            Message::Recenter => {
                state.recenters += 1;
                Change::Recenter
            }
            Message::KeyframeRequest => {
                state.keyframe_requested = true;
                Change::KeyframeRequest
            }
            Message::InputFocus { focused } => {
                state.focus_lost = !focused;
                Change::Focus
            }
            Message::ServerHello(_)
            | Message::VideoFrame(_)
            | Message::Haptics(_)
            | Message::StreamEnded
            | Message::AudioStart(_)
            | Message::Audio(_)
            | Message::AudioStop { .. } => return,
        };
        self.notify(&state, change);
    }
}

fn pose_is_sane(p: &openloop_protocol::Pose) -> bool {
    let norm: f32 = p.orientation.iter().map(|q| q * q).sum();
    p.position
        .iter()
        .chain(&p.orientation)
        .all(|v| v.is_finite())
        && (norm - 1.0).abs() < 1e-2
}

pub(crate) fn tracking_is_sane(t: &Tracking) -> bool {
    pose_is_sane(&t.head)
        && t.views.iter().all(|v| pose_is_sane(&v.pose))
        && t.controllers
            .iter()
            .all(|c| !c.active || (pose_is_sane(&c.grip) && pose_is_sane(&c.aim)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openloop_protocol::{Pose, Velocity};

    #[test]
    fn a_retired_link_holds_the_last_pose() {
        let mut state = HeadsetState::default();
        let head = Pose {
            position: [1.0, 1.5, -2.0],
            ..Pose::IDENTITY
        };
        let mut controller = ControllerState {
            active: true,
            ..Default::default()
        };
        controller.grip = head;
        state.tracked = Tracked {
            sample: Some((
                Tracking {
                    head,
                    head_velocity: Velocity {
                        linear: [1.0, 0.0, 0.0],
                        ..Default::default()
                    },
                    controllers: [controller, controller],
                    ..Default::default()
                },
                0,
            )),
            live: true,
        };
        state.recenters = 3;
        state.focus_lost = true;
        state.greeted = true;
        assert!(
            state.tracked.head_at(50_000_000).translation.x > 1.0,
            "live links extrapolate"
        );

        state.retire();
        assert!(!state.connected());
        assert!(!state.greeted());
        assert_eq!(
            state.tracked.head_at(50_000_000),
            Rigid::from_pose(&head),
            "held, not extrapolated"
        );
        assert!(
            state.tracked.controller(0).is_none(),
            "untracked hands are inactive"
        );
        assert_eq!(state.recenters, 3, "recenter count survives the link");
        assert!(
            state.app_focused(),
            "a lost headset hands focus back to the app"
        );
    }

    #[test]
    fn video_waits_for_the_clock() {
        let mut state = HeadsetState::default();
        state.tracked.live = true;
        assert!(state.connected());
        assert!(
            !state.ready_for_video(),
            "a display time cannot be mapped before the first pong"
        );
        state.clock.observe(0, 1000, 10);
        assert!(state.ready_for_video());
    }

    #[test]
    fn undefined_poses_are_rejected() {
        let good = Tracking {
            head: Pose::IDENTITY,
            views: pose::default_views(),
            ..Default::default()
        };
        assert!(tracking_is_sane(&good));
        let mut zero = good.clone();
        zero.head.orientation = [0.0; 4];
        assert!(
            !tracking_is_sane(&zero),
            "an all-zero quaternion is not a rotation"
        );
        let mut nan = good.clone();
        nan.views[1].pose.position[0] = f32::NAN;
        assert!(!tracking_is_sane(&nan));
    }
}
