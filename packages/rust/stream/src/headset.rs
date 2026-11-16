//! The connected headset as the Mac sees it: its profile, latest tracking, clock mapping and
//! requests. One connection at a time; a new headset replaces the old one.

use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use tether_protocol::{
    ControllerState, DeviceProfile, Message, PROTOCOL_VERSION, ServerHello, Tracking, View,
};

use crate::link::{Link, Transport};
use crate::pose::{self, Rigid};
use crate::time::{ClockSync, now_ns};

/// Extrapolation beyond this is guesswork; poses are held instead.
const MAX_PREDICTION_NS: i64 = 100_000_000;
const PING_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct HeadsetState {
    pub profile: Option<DeviceProfile>,
    /// Latest sample and its time on the Mac clock.
    tracking: Option<(Tracking, i64)>,
    pub clock: ClockSync,
    /// Increments on every system recenter the headset reports.
    pub recenters: u64,
    pub keyframe_requested: bool,
    /// Encoded video size the client was last told about.
    pub video_size: Option<(u32, u32)>,
    /// Tracking is arriving on the current link. Without it the last pose is held.
    live: bool,
}

impl HeadsetState {
    pub fn connected(&self) -> bool {
        self.live
    }

    /// Forgets the link but keeps the last pose, so apps hold still instead of snapping to
    /// the default viewer, and keeps the recenter count monotonic for sessions watching it.
    fn retire(&mut self) {
        *self = Self {
            tracking: self.tracking.take(),
            profile: self.profile.take(),
            recenters: self.recenters,
            ..Self::default()
        };
    }

    pub fn head_at(&self, time_ns: i64) -> Rigid {
        self.extrapolate(time_ns, |t| (Rigid::from_pose(&t.head), t.head_velocity))
            .unwrap_or(pose::DEFAULT_HEAD)
    }

    /// Eyes relative to the head.
    pub fn views(&self) -> [View; 2] {
        self.tracking
            .as_ref()
            .map_or_else(pose::default_views, |(t, _)| t.views)
    }

    pub fn controller(&self, hand: usize) -> Option<ControllerState> {
        self.tracking
            .as_ref()
            .map(|(t, _)| t.controllers[hand])
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
        f: impl FnOnce(&Tracking) -> (Rigid, tether_protocol::Velocity),
    ) -> Option<Rigid> {
        let (sample, sample_ns) = self.tracking.as_ref()?;
        let (pose, velocity) = f(sample);
        let dt = if self.live {
            (time_ns - sample_ns).clamp(0, MAX_PREDICTION_NS)
        } else {
            0
        };
        Some(pose::predict(pose, &velocity, dt as f32 * 1e-9))
    }
}

/// Owns the transport and the current headset link; shared between the runtime and its threads.
pub struct Headset {
    state: Mutex<HeadsetState>,
    link: Mutex<Option<Arc<dyn Link>>>,
}

impl Headset {
    /// A headset that never connects; the runtime still runs, rendering for the default viewer.
    pub fn detached() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            link: Mutex::new(None),
        })
    }

    pub fn start(mut transport: Box<dyn Transport>) -> Arc<Self> {
        let headset = Self::detached();
        let weak = Arc::downgrade(&headset);
        thread::Builder::new()
            .name("tether-accept".into())
            .spawn(move || {
                while let Ok(link) = transport.accept() {
                    let Some(headset) = weak.upgrade() else {
                        return;
                    };
                    // Each link is served on its own thread so a relaunched headset is
                    // accepted at once; `serve` retires whichever link it replaces.
                    let link: Arc<dyn Link> = Arc::from(link);
                    thread::Builder::new()
                        .name("tether-link".into())
                        .spawn(move || headset.serve(link))
                        .expect("spawning a link thread");
                }
            })
            .expect("spawning the accept thread");
        headset
    }

    pub fn state(&self) -> MutexGuard<'_, HeadsetState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends to the current headset; dropped when none is connected.
    pub fn send(&self, message: &Message) -> bool {
        let link = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match link {
            Some(link) => link.send(message).is_ok(),
            None => false,
        }
    }

    pub fn announce_video_size(&self, width: u32, height: u32) {
        if self.state().video_size == Some((width, height)) {
            return;
        }
        let hello = Message::ServerHello(ServerHello {
            version: PROTOCOL_VERSION,
            video_width: width,
            video_height: height,
        });
        if self.send(&hello) {
            self.state().video_size = Some((width, height));
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
        self.state().retire();

        let pinger = Arc::downgrade(&link);
        thread::Builder::new()
            .name("tether-ping".into())
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
        let mut current = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &link)) {
            *current = None;
            self.state().retire();
        }
    }

    fn handle(&self, link: &Arc<dyn Link>, message: Message) {
        let now = now_ns();
        let mut state = self.state();
        match message {
            Message::ClientHello(hello) if hello.version == PROTOCOL_VERSION => {
                state.profile = Some(hello.profile);
            }
            Message::ClientHello(_) => link.shutdown(),
            Message::ClockPong { mac_ns, client_ns } => state.clock.observe(mac_ns, client_ns, now),
            // A pose that is not finite and unit-length would reach the app as NaN.
            Message::Tracking(tracking) if !tracking_is_sane(&tracking) => {}
            Message::Tracking(tracking) => {
                let at = if state.clock.is_synced() {
                    state.clock.to_mac(tracking.sample_time_ns)
                } else {
                    now
                };
                state.tracking = Some((*tracking, at));
                state.live = true;
            }
            Message::ClockPing { mac_ns } => {
                drop(state);
                let _ = link.send(&Message::ClockPong {
                    mac_ns,
                    client_ns: now,
                });
            }
            Message::Recenter => state.recenters += 1,
            Message::KeyframeRequest => state.keyframe_requested = true,
            Message::ServerHello(_) | Message::VideoFrame(_) | Message::Haptics(_) => {}
        }
    }
}

fn pose_is_sane(p: &tether_protocol::Pose) -> bool {
    let norm: f32 = p.orientation.iter().map(|q| q * q).sum();
    p.position
        .iter()
        .chain(&p.orientation)
        .all(|v| v.is_finite())
        && (norm - 1.0).abs() < 1e-2
}

fn tracking_is_sane(t: &Tracking) -> bool {
    pose_is_sane(&t.head)
        && t.views.iter().all(|v| pose_is_sane(&v.pose))
        && t.controllers
            .iter()
            .all(|c| !c.active || (pose_is_sane(&c.grip) && pose_is_sane(&c.aim)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tether_protocol::{Pose, Velocity};

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
        state.tracking = Some((
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
        ));
        state.live = true;
        state.recenters = 3;
        assert!(
            state.head_at(50_000_000).translation.x > 1.0,
            "live links extrapolate"
        );

        state.retire();
        assert!(!state.connected());
        assert_eq!(
            state.head_at(50_000_000),
            Rigid::from_pose(&head),
            "held, not extrapolated"
        );
        assert!(
            state.controller(0).is_none(),
            "untracked hands are inactive"
        );
        assert_eq!(state.recenters, 3, "recenter count survives the link");
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
