//! One app session: the shared-memory region `openloopd` made for it, the records published there,
//! and the thread that forwards the app's encoded video and haptics to the headset
//! (docs/ARCHITECTURE.md § Runtime ↔ openloopd). Its sound goes through `audio`.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use openloop_ipc::records::{ClockMapping, FrameTiming, Message, SessionState};
use openloop_ipc::{AccessUnits, PopError, Publisher, SessionRegion, now_ns};
use openloop_protocol::Haptics;
use openloop_stream::{Change, HeadsetState, PIPELINE_LATENCY_NS, shared};

use crate::audio::Tally;
use crate::log;
use crate::report::SessionReport;
use crate::stage::{Forwarded, Stage};

/// How often an idle forwarder looks at its session without being rung.
const IDLE_WAKE_NS: i64 = 100_000_000;
/// How often a stream waiting for a keyframe asks the app again.
const KEYFRAME_RETRY_NS: i64 = 250_000_000;

/// Who a session belongs to: a process, told apart from a later one with the same PID by its
/// audit token's PID version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    pub pid: i32,
    pub pid_version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Created,
    Running,
    Ended,
    Destroyed,
}

impl Phase {
    fn from_u8(value: u8) -> Phase {
        match value {
            0 => Phase::Created,
            1 => Phase::Running,
            2 => Phase::Ended,
            _ => Phase::Destroyed,
        }
    }
}

pub struct Session {
    pub id: u64,
    pub owner: Owner,
    pub app: String,
    pub engine: String,
    /// The app's PID from its connection's audit token, which is what its sound is captured
    /// by; `None` without a token.
    pub audio_pid: Option<i32>,
    pub audio: Tally,
    region: SessionRegion,
    records: Mutex<Records>,
    phase: AtomicU8,
}

/// The single writer's side of every record `openloopd` publishes in the region.
struct Records {
    headset: Publisher,
    tracking: Publisher,
    clock: Publisher,
    timing: Publisher,
    state: Publisher,
    current: SessionState,
    published: Option<SessionState>,
    last_headset: Option<openloop_ipc::records::HeadsetInfo>,
}

impl Session {
    pub fn new(
        id: u64,
        owner: Owner,
        app: String,
        engine: String,
        audio_pid: Option<i32>,
        boot_id: u64,
    ) -> io::Result<Arc<Session>> {
        let region = SessionRegion::create(id, boot_id, now_ns())?;
        let records = Records {
            headset: region.headset.publisher(),
            tracking: region.tracking.publisher(),
            clock: region.clock.publisher(),
            timing: region.timing.publisher(),
            state: region.state.publisher(),
            current: SessionState::default(),
            published: None,
            last_headset: None,
        };
        region.beat(now_ns());
        Ok(Arc::new(Session {
            id,
            owner,
            app,
            engine,
            audio_pid,
            audio: Tally::default(),
            region,
            records: Mutex::new(records),
            phase: AtomicU8::new(Phase::Created as u8),
        }))
    }

    pub fn region(&self) -> &SessionRegion {
        &self.region
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }

    /// Wakes the forwarder so it acts on the new phase at once.
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Release);
        self.region.openloopd_bell.ring();
    }

    fn records(&self) -> MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publishes everything the headset state holds, for a session that has just been made.
    pub fn reflect_all(&self, state: &HeadsetState) {
        let now = now_ns();
        let mut records = self.records();
        let r = &mut *records;
        self.publish_headset(r, state);
        self.publish_tracking(r, state, now);
        self.publish_clock(r, state, now);
        r.current.recenters = state.recenters;
        r.current.flags = flags(state);
        self.publish_state(r);
    }

    /// Publishes what one change on the link thread touched. `foreground`: the session's
    /// video is the one the headset shows, so the headset's keyframe requests are its.
    pub fn reflect(&self, state: &HeadsetState, change: Change, foreground: bool) {
        let now = now_ns();
        let mut records = self.records();
        let r = &mut *records;
        match change {
            Change::Tracking => self.publish_tracking(r, state, now),
            Change::Clock => self.publish_clock(r, state, now),
            Change::Linked | Change::Unlinked | Change::Profile => {
                self.publish_headset(r, state);
                self.publish_tracking(r, state, now);
            }
            Change::Recenter => {
                r.current.recenters = state.recenters;
                r.current.last_recenter_mac_ns = now;
            }
            Change::KeyframeRequest if foreground => r.current.keyframe_requests += 1,
            Change::KeyframeRequest | Change::Focus | Change::Refused { .. } => {}
        }
        r.current.flags = flags(state);
        self.publish_state(r);
    }

    /// Asks the app's encoder for a keyframe.
    pub fn request_keyframe(&self) {
        let mut records = self.records();
        records.current.keyframe_requests += 1;
        self.publish_state(&mut records);
    }

    fn publish_headset(&self, r: &mut Records, state: &HeadsetState) {
        let Some(profile) = &state.profile else {
            return;
        };
        let info = shared::headset_info(profile, state.greeted());
        if r.last_headset != Some(info) {
            self.region.headset.publish(&mut r.headset, &info);
            r.last_headset = Some(info);
            r.current.headset_generation += 1;
            // No schedule until frames are paced to the headset (R2 S4): only its period, and
            // the latency the app's sound is delayed by too.
            let timing = FrameTiming {
                period_ns: (1e9 / f64::from(profile.refresh_rate.max(1.0))) as i64,
                latency_ns: PIPELINE_LATENCY_NS,
                ..FrameTiming::default()
            };
            self.region.timing.publish(&mut r.timing, &timing);
        }
    }

    fn publish_tracking(&self, r: &mut Records, state: &HeadsetState, now: i64) {
        if let Some(sample) = shared::tracking_sample(&state.tracked, now) {
            self.region.tracking.publish(&mut r.tracking, &sample);
        }
    }

    fn publish_clock(&self, r: &mut Records, state: &HeadsetState, now: i64) {
        let Some(round_trip_ns) = state.clock.round_trip_ns() else {
            return;
        };
        let mapping = ClockMapping {
            offset_ns: state.clock.offset_ns(),
            round_trip_ns,
            measured_mac_ns: now,
            flags: ClockMapping::SYNCED,
        };
        self.region.clock.publish(&mut r.clock, &mapping);
    }

    fn publish_state(&self, r: &mut Records) {
        if r.published != Some(r.current) {
            self.region.state.publish(&mut r.state, &r.current);
            r.published = Some(r.current);
        }
    }
}

fn flags(state: &HeadsetState) -> u64 {
    let mut flags = 0;
    if state.ready_for_video() {
        flags |= SessionState::LINK_UP;
    }
    if state.app_focused() {
        flags |= SessionState::FOCUSED;
    }
    flags
}

/// What the forwarder keeps between access units.
pub struct Video {
    /// The headset cannot decode what comes next without a keyframe: the stream is new, or a
    /// frame of it was lost.
    pub needs_keyframe: bool,
    last_request_ns: i64,
}

/// Forwards the session's access units and haptics until it is destroyed, then ends its stream
/// and writes its report.
pub fn spawn_forwarder(session: Arc<Session>, stage: Arc<Stage>, reports: PathBuf) {
    let name = format!("openloopd-session-{}", session.id);
    let spawned = thread::Builder::new()
        .name(name)
        .spawn(move || forward(&session, &stage, reports));
    if let Err(e) = spawned {
        log!("cannot start a forwarder: {e}");
    }
}

fn forward(session: &Session, stage: &Stage, reports: PathBuf) {
    let region = session.region();
    let mut events = region.to_openloopd.consumer();
    let mut units = region.access_units.consumer();
    let mut buffer = vec![0u8; AccessUnits::MAX_BYTES];
    let mut video = Video {
        needs_keyframe: true,
        last_request_ns: i64::MIN,
    };
    let mut report = SessionReport::new(session);
    let mut corrupt = false;
    loop {
        let seen = region.openloopd_bell.rings();
        let phase = session.phase();
        while !corrupt {
            match region.to_openloopd.pop(&mut events) {
                Ok(Some(event)) => match event.decode() {
                    Ok(message) => haptic(session, stage, message, &mut report),
                    Err(bad) => {
                        log!(
                            "session {}: ignoring a malformed event {:?}",
                            session.id,
                            bad.0
                        );
                    }
                },
                Ok(None) => break,
                Err(_) => {
                    log!(
                        "session {}: its event ring is corrupt; ignoring it",
                        session.id
                    );
                    corrupt = true;
                }
            }
        }
        while !corrupt {
            match region.access_units.pop_into(&mut units, &mut buffer) {
                Ok(Some(unit)) => {
                    let data = &buffer[..unit.len as usize];
                    let outcome = if phase == Phase::Running {
                        stage.forward(session, &unit, data, &mut video)
                    } else {
                        Forwarded::NotRunning
                    };
                    if matches!(outcome, Forwarded::AwaitingKeyframe | Forwarded::Stale) {
                        ask_for_keyframe(session, &mut video, &mut report);
                    }
                    report.access_unit(&unit, outcome);
                }
                Ok(None) => break,
                Err(PopError::Corrupt | PopError::BufferTooSmall { .. }) => {
                    log!(
                        "session {}: its video ring is corrupt; ignoring it",
                        session.id
                    );
                    corrupt = true;
                }
            }
        }
        if phase != Phase::Running {
            stage.end_stream_of(session.id);
            video.needs_keyframe = true;
        }
        if phase == Phase::Destroyed {
            break;
        }
        // Errors here are the kernel refusing the wait; the loop just looks again.
        let _ = region
            .openloopd_bell
            .wait(seen, Some(now_ns().saturating_add(IDLE_WAKE_NS)));
    }
    report.finish(corrupt, &session.audio);
    match report.write(&reports) {
        Ok(path) => log!("session {} report: {}", session.id, path.display()),
        Err(e) => log!("session {}: cannot write its report: {e}", session.id),
    }
}

fn ask_for_keyframe(session: &Session, video: &mut Video, report: &mut SessionReport) {
    let now = now_ns();
    if now.saturating_sub(video.last_request_ns) >= KEYFRAME_RETRY_NS {
        session.request_keyframe();
        video.last_request_ns = now;
        report.keyframe_requested();
    }
}

fn haptic(session: &Session, stage: &Stage, message: Message, report: &mut SessionReport) {
    let haptics = match message {
        Message::Haptic {
            hand,
            amplitude,
            duration_ns,
            frequency_hz,
        } => Haptics {
            hand: hand as u8,
            amplitude,
            duration_ns,
            frequency: frequency_hz,
        },
        Message::StopHaptic { hand } => Haptics {
            hand: hand as u8,
            amplitude: 0.0,
            duration_ns: 0,
            frequency: 0.0,
        },
        _ => return,
    };
    if stage.haptic(session, &haptics) {
        report.haptic();
    }
}
