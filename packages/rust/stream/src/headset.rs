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
}

impl HeadsetState {
    pub fn connected(&self) -> bool {
        self.tracking.is_some()
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
            .filter(|c| c.active)
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
        let dt = (time_ns - sample_ns).clamp(0, MAX_PREDICTION_NS);
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
                    headset.serve(Arc::from(link));
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
        *self.state() = HeadsetState::default();

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
            *self.state() = HeadsetState::default();
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
            Message::Tracking(tracking) => {
                let at = if state.clock.is_synced() {
                    state.clock.to_mac(tracking.sample_time_ns)
                } else {
                    now
                };
                state.tracking = Some((*tracking, at));
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
