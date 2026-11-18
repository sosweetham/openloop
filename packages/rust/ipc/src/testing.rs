//! Helpers the crate's tests and its two-process test service share; not part of the contract.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLSharedEvent, MTLStorageMode,
    MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};

use crate::control::{
    Hello, Peer, Service, SessionGrant, Status, SwapchainDesc, SwapchainGrant, Welcome,
};
use crate::records::{
    ClockMapping, FrameTiming, HeadsetInfo, Message, SessionState, TrackingSample,
};
use crate::{Pod, SessionRegion, Wake, now_ns};

/// Latency samples in nanoseconds.
pub struct Stats {
    samples: Vec<u64>,
}

impl Stats {
    pub fn with_capacity(capacity: usize) -> Stats {
        Stats {
            samples: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, ns: u64) {
        self.samples.push(ns);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The `q` quantile, in nanoseconds.
    pub fn quantile(&mut self, q: f64) -> u64 {
        self.samples.sort_unstable();
        let last = self.samples.len().saturating_sub(1);
        self.samples
            .get((last as f64 * q).round() as usize)
            .copied()
            .unwrap_or(0)
    }

    pub fn summary(&mut self, name: &str) -> String {
        if self.samples.is_empty() {
            return format!("{name}: no samples");
        }
        let mean = self.samples.iter().sum::<u64>() as f64 / self.samples.len() as f64;
        let us = |ns: u64| ns as f64 / 1000.0;
        format!(
            "{name}: n={} min={:.2}us p50={:.2}us mean={:.2}us p99={:.2}us p99.9={:.2}us max={:.2}us",
            self.samples.len(),
            us(self.quantile(0.0)),
            us(self.quantile(0.5)),
            mean / 1000.0,
            us(self.quantile(0.99)),
            us(self.quantile(0.999)),
            us(self.quantile(1.0)),
        )
    }
}

/// A byte pattern that differs per frame and per offset, so a misplaced or stale byte shows.
pub fn fill_au_bytes(frame: u64, bytes: &mut [u8]) {
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = pattern(frame, offset);
    }
}

pub fn au_bytes_match(frame: u64, bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .all(|(offset, &byte)| byte == pattern(frame, offset))
}

fn pattern(frame: u64, offset: usize) -> u8 {
    let mixed =
        (frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ offset as u64).wrapping_mul(0xff51_afd7);
    (mixed >> 24) as u8
}

/// A sample whose every word derives from `generation`, so a torn copy shows.
pub fn stamped_sample(generation: u64) -> TrackingSample {
    let mut words = [0u64; size_of::<TrackingSample>() / 8];
    for (index, word) in words.iter_mut().enumerate() {
        *word = generation.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ index as u64;
    }
    TrackingSample::from_bytes(words.as_bytes()).expect("a sample is whole words")
}

pub fn is_stamped(sample: &TrackingSample) -> bool {
    let bytes = sample.as_bytes();
    let word = |index: usize| u64::from_ne_bytes(bytes[index * 8..][..8].try_into().unwrap());
    (0..bytes.len() / 8).all(|index| word(index) ^ index as u64 == word(0))
}

pub fn headset_info() -> HeadsetInfo {
    let mut info = HeadsetInfo {
        eye_width: 1680,
        eye_height: 1760,
        refresh_rate: 90.0,
        refresh_rate_count: 3,
        refresh_rates: [72.0, 90.0, 120.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        audio_sample_rate: 48_000,
        audio_channels: 2,
        flags: HeadsetInfo::CONNECTED | HeadsetInfo::HEVC,
        ..HeadsetInfo::default()
    };
    info.set_name("Test Headset");
    info.set_interaction_profile("/interaction_profiles/oculus/touch_controller");
    info
}

pub fn clock_mapping() -> ClockMapping {
    ClockMapping {
        offset_ns: 10_000_000_000,
        round_trip_ns: 180_000,
        measured_mac_ns: 1,
        flags: ClockMapping::SYNCED,
    }
}

pub fn frame_timing() -> FrameTiming {
    FrameTiming {
        frame_index: 1,
        wake_mac_ns: 2,
        display_mac_ns: 3,
        period_ns: 11_111_111,
        latency_ns: 40_000_000,
        flags: FrameTiming::SHOULD_RENDER,
    }
}

pub fn session_state() -> SessionState {
    SessionState {
        flags: SessionState::LINK_UP | SessionState::FOCUSED,
        recenters: 1,
        last_recenter_mac_ns: 4,
        keyframe_requests: 0,
        headset_generation: 1,
    }
}

/// `Message::Echo` values that ask the test service for something instead of an echo.
pub mod probe {
    /// Publish tracking without pause until [`FLOOD_STOP`]; answers with the count published.
    pub const FLOOD_START: u64 = u64::MAX - 1;
    pub const FLOOD_STOP: u64 = u64::MAX - 2;
    /// Answers with the access units verified, and in `sent_mac_ns` the ones that were not.
    pub const ACCESS_UNITS: u64 = u64::MAX - 3;
    /// Answers with the app-ready event's value as the service sees it.
    pub const APP_READY: u64 = u64::MAX - 4;
}

/// A stand-in for `openloopd`: it publishes the fixed records above, echoes events with its wake
/// time, verifies access units against [`fill_au_bytes`], and allocates shared Metal objects.
pub struct TestService {
    boot_id: u64,
    device: Option<Retained<ProtocolObject<dyn MTLDevice>>>,
    sessions: Mutex<HashMap<u64, TestSession>>,
    next: AtomicU64,
}

struct TestSession {
    region: Arc<SessionRegion>,
    app_ready: Option<Retained<ProtocolObject<dyn MTLSharedEvent>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Default for TestService {
    fn default() -> Self {
        TestService {
            boot_id: (u64::from(std::process::id()) << 32) | (now_ns() as u64 & 0xffff_ffff),
            device: MTLCreateSystemDefaultDevice(),
            sessions: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }
}

impl TestService {
    fn stop(session: &mut TestSession) {
        session.stop.store(true, Ordering::SeqCst);
        session.region.openloopd_bell.ring();
        if let Some(worker) = session.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Service for TestService {
    fn hello(&self, _peer: &Peer, _hello: &Hello) -> Result<Welcome, Status> {
        Ok(Welcome {
            registry_id: self.device.as_ref().map_or(0, |device| device.registryID()),
            boot_id: self.boot_id,
            headset: Some(headset_info()),
        })
    }

    fn headset(&self, _peer: &Peer) -> Result<HeadsetInfo, Status> {
        Ok(headset_info())
    }

    fn create_session(&self, _peer: &Peer) -> Result<SessionGrant, Status> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let region =
            SessionRegion::create(id, self.boot_id, now_ns()).map_err(|_| Status::Failed)?;
        let region = Arc::new(region);
        let grant_region = region.to_xpc().map_err(|_| Status::Failed)?;
        let app_ready = self
            .device
            .as_ref()
            .and_then(|device| device.newSharedEvent());
        let compositor_done = self
            .device
            .as_ref()
            .and_then(|device| device.newSharedEvent());
        let grant = SessionGrant {
            session: id,
            region: grant_region,
            app_ready: app_ready.as_ref().map(|event| event.newSharedEventHandle()),
            compositor_done: compositor_done
                .as_ref()
                .map(|event| event.newSharedEventHandle()),
        };
        let session = TestSession {
            region,
            app_ready,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        self.sessions.lock().unwrap().insert(id, session);
        Ok(grant)
    }

    fn begin_session(&self, _peer: &Peer, session: u64) -> Status {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&session) else {
            return Status::UnknownSession;
        };
        if session.worker.is_some() {
            return Status::Ok;
        }
        let region = &session.region;
        region
            .headset
            .publish(&mut region.headset.publisher(), &headset_info());
        region
            .clock
            .publish(&mut region.clock.publisher(), &clock_mapping());
        region
            .timing
            .publish(&mut region.timing.publisher(), &frame_timing());
        region
            .state
            .publish(&mut region.state.publisher(), &session_state());
        let mut to_app = region.to_app.producer();
        for message in [
            Message::Focus {
                focused: true,
                at_mac_ns: 5,
            },
            Message::Recenter {
                count: 1,
                at_mac_ns: 4,
            },
        ] {
            region
                .to_app
                .push(&mut to_app, &message.into())
                .expect("an empty ring");
        }
        region.app_bell.ring();
        session.stop.store(false, Ordering::SeqCst);
        let (region, stop) = (session.region.clone(), session.stop.clone());
        let app_ready = session.app_ready.clone();
        session.worker = Some(
            std::thread::Builder::new()
                .name("test-session".into())
                .spawn(move || serve(region, &stop, to_app, app_ready.as_deref()))
                .expect("spawning a session thread"),
        );
        Status::Ok
    }

    fn end_session(&self, _peer: &Peer, session: u64) -> Status {
        match self.sessions.lock().unwrap().get_mut(&session) {
            Some(session) => {
                TestService::stop(session);
                Status::Ok
            }
            None => Status::UnknownSession,
        }
    }

    fn destroy_session(&self, _peer: &Peer, session: u64) -> Status {
        match self.sessions.lock().unwrap().remove(&session) {
            Some(mut session) => {
                TestService::stop(&mut session);
                Status::Ok
            }
            None => Status::UnknownSession,
        }
    }

    fn create_swapchain(
        &self,
        _peer: &Peer,
        session: u64,
        desc: &SwapchainDesc,
    ) -> Result<SwapchainGrant, Status> {
        if !self.sessions.lock().unwrap().contains_key(&session) {
            return Err(Status::UnknownSession);
        }
        let device = self.device.as_ref().ok_or(Status::Unsupported)?;
        let descriptor = MTLTextureDescriptor::new();
        descriptor.setTextureType(if desc.array_length > 1 {
            MTLTextureType::Type2DArray
        } else {
            MTLTextureType::Type2D
        });
        descriptor.setPixelFormat(MTLPixelFormat(desc.pixel_format as usize));
        // SAFETY: the tests ask for small, valid sizes; a real openloopd validates them first.
        unsafe {
            descriptor.setWidth(desc.width as usize);
            descriptor.setHeight(desc.height as usize);
            descriptor.setArrayLength(desc.array_length.max(1) as usize);
            descriptor.setMipmapLevelCount(desc.mipmap_level_count.max(1) as usize);
        }
        descriptor.setUsage(MTLTextureUsage(desc.usage as usize));
        descriptor.setStorageMode(MTLStorageMode::Private);
        let images = (0..3)
            .map(|_| {
                let texture = device.newSharedTextureWithDescriptor(&descriptor)?;
                texture.newSharedTextureHandle()
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(Status::Failed)?;
        Ok(SwapchainGrant {
            swapchain: 1,
            images,
        })
    }

    fn destroy_swapchain(&self, _peer: &Peer, _session: u64, _swapchain: u64) -> Status {
        Status::Ok
    }

    fn disconnected(&self, peer: &Peer) {
        eprintln!("test service: peer {} disconnected", peer.pid);
        for (_, mut session) in self.sessions.lock().unwrap().drain() {
            TestService::stop(&mut session);
        }
    }
}

/// The session thread: what `openloopd`'s per-client threads will do, minus the headset.
fn serve(
    region: Arc<SessionRegion>,
    stop: &AtomicBool,
    mut to_app: crate::Producer,
    app_ready: Option<&ProtocolObject<dyn MTLSharedEvent>>,
) {
    let mut from_app = region.to_openloopd.consumer();
    let mut units = region.access_units.consumer();
    let mut buffer = vec![0; crate::AccessUnits::MAX_BYTES];
    let (mut verified, mut corrupt) = (0u64, 0u64);
    let mut flood: Option<(Arc<AtomicBool>, JoinHandle<u64>)> = None;
    let mut reply = |message: Message| {
        while region.to_app.push(&mut to_app, &message.into()) == Err(crate::PushError::Full) {
            std::thread::yield_now();
        }
        region.app_bell.ring();
    };
    while !stop.load(Ordering::SeqCst) {
        let seen = region.openloopd_bell.rings();
        let mut drain_units = |verified: &mut u64, corrupt: &mut u64| {
            while let Ok(Some(unit)) = region.access_units.pop_into(&mut units, &mut buffer) {
                if au_bytes_match(unit.frame_index, &buffer[..unit.len as usize]) {
                    *verified += 1;
                } else {
                    *corrupt += 1;
                }
            }
        };
        drain_units(&mut verified, &mut corrupt);
        while let Ok(Some(event)) = region.to_openloopd.pop(&mut from_app) {
            let woke = now_ns();
            match event.decode() {
                Ok(Message::Echo {
                    value: probe::FLOOD_START,
                    ..
                }) => {
                    let done = Arc::new(AtomicBool::new(false));
                    let (writer, until) = (region.clone(), done.clone());
                    let thread = std::thread::spawn(move || flood_tracking(&writer, &until));
                    flood = Some((done, thread));
                    reply(Message::Echo {
                        value: 0,
                        sent_mac_ns: 0,
                    });
                }
                Ok(Message::Echo {
                    value: probe::FLOOD_STOP,
                    ..
                }) => {
                    let published = flood.take().map_or(0, |(done, thread)| {
                        done.store(true, Ordering::SeqCst);
                        thread.join().unwrap_or(0)
                    });
                    reply(Message::Echo {
                        value: published,
                        sent_mac_ns: 0,
                    });
                }
                Ok(Message::Echo {
                    value: probe::ACCESS_UNITS,
                    ..
                }) => {
                    drain_units(&mut verified, &mut corrupt);
                    reply(Message::Echo {
                        value: verified,
                        sent_mac_ns: corrupt as i64,
                    });
                }
                Ok(Message::Echo {
                    value: probe::APP_READY,
                    ..
                }) => {
                    let value = app_ready.map_or(0, |event| event.signaledValue());
                    reply(Message::Echo {
                        value,
                        sent_mac_ns: 0,
                    });
                }
                Ok(Message::Echo { sent_mac_ns, .. }) => reply(Message::Echo {
                    value: woke as u64,
                    sent_mac_ns,
                }),
                _ => {}
            }
        }
        let _: Result<Wake, _> = region
            .openloopd_bell
            .wait(seen, Some(now_ns() + 100_000_000));
    }
    if let Some((done, thread)) = flood {
        done.store(true, Ordering::SeqCst);
        let _ = thread.join();
    }
}

fn flood_tracking(region: &SessionRegion, done: &AtomicBool) -> u64 {
    let mut publisher = region.tracking.publisher();
    let mut generation = 0;
    while !done.load(Ordering::Relaxed) {
        generation = region
            .tracking
            .publish(&mut publisher, &stamped_sample(generation + 1));
    }
    generation
}

/// How long a two-process test waits for the service before it fails instead of hanging.
pub const PATIENCE: Duration = Duration::from_secs(10);
