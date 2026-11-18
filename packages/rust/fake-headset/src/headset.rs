//! One fake headset session: connects to the Mac as the Quest client does and reconnects the same
//! way, streams the script's tracking at the headset's refresh rate, answers clock pings on the
//! receiving thread, and records everything the Mac sends.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use openloop_protocol::{
    ClientHello, DecodeError, DeviceProfile, Message, PROTOCOL_VERSION, StreamDecoder, Tracking,
    encode_framed,
};

use crate::clock::{ClockModel, HeadsetClock, monotonic_ns};
use crate::report::{KeyframeReason, Recorder, Report};
use crate::script::{Action, Hand, HandInput, Script};

/// The Quest client's retry interval.
const RECONNECT: Duration = Duration::from_millis(500);
/// How often a blocked read wakes to notice the end of the session or a pause.
const READ_POLL: Duration = Duration::from_millis(20);
/// A Mac that takes this long to accept a write has stalled; the link is dropped as lost.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// A Quest 3's profile: its recommended eye size and refresh rates, and the 48 kHz stereo that
/// Horizon OS mixes app audio at.
pub fn quest_3() -> DeviceProfile {
    DeviceProfile {
        name: "Fake Quest 3".into(),
        eye_width: 1680,
        eye_height: 1760,
        refresh_rates: vec![72.0, 80.0, 90.0, 120.0],
        refresh_rate: 90.0,
        interaction_profile: "/interaction_profiles/oculus/touch_controller".into(),
        supports_hevc: true,
        audio_sample_rate: 48_000,
        audio_channels: 2,
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The Mac's loopback port.
    pub port: u16,
    pub profile: DeviceProfile,
    pub clock: ClockModel,
    pub script: Script,
    /// How long [`FakeHeadset::connect`] retries before giving up.
    pub connect_timeout: Duration,
    /// Ask for a keyframe whenever a stream starts, as the Quest's decoder does when it opens.
    pub keyframe_request_on_stream_start: bool,
    /// How far ahead of now a tracking sample is stamped and posed. The Quest stamps the
    /// predicted display time of the frame it is about to render.
    pub tracking_lead: Duration,
    /// Display time minus arrival, in nanoseconds on the headset clock: a frame outside this
    /// window carries a time in the wrong clock domain. It allows the lateness of frames that
    /// waited out a stalled link, and the default clock offset lies far outside it.
    pub display_lead_window_ns: (i64, i64),
}

impl Config {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            profile: quest_3(),
            clock: ClockModel::default(),
            script: Script::default(),
            connect_timeout: Duration::from_secs(10),
            keyframe_request_on_stream_start: true,
            tracking_lead: Duration::from_millis(22),
            display_lead_window_ns: (-5_000_000_000, 500_000_000),
        }
    }

    fn address(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }
}

/// A running session. Dropping it ends the session; [`FakeHeadset::finish`] also returns the
/// final report.
pub struct FakeHeadset {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl FakeHeadset {
    /// Connects, retrying as the Quest does until `config.connect_timeout`, and starts the
    /// session's threads.
    pub fn connect(config: Config) -> io::Result<Self> {
        let shared = Arc::new(Shared::new(config));
        let stream = shared.first_connection()?;
        shared.link_up(&stream)?;
        let link = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("fake-headset-link".into())
                .spawn(move || shared.run_link(stream))?
        };
        let script = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("fake-headset-tracking".into())
                .spawn(move || shared.run_script())?
        };
        Ok(Self {
            shared,
            threads: vec![link, script],
        })
    }

    /// Acts now, as a cue would.
    pub fn act(&self, action: Action) {
        self.shared.act(action, KeyframeReason::Caller);
    }

    pub fn report(&self) -> Report {
        self.shared.report()
    }

    /// Polls the report until `done` holds or `timeout` passes; `true` if it held.
    pub fn wait_for(&self, timeout: Duration, mut done: impl FnMut(&Report) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if done(&self.report()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn finish(mut self) -> Report {
        self.stop();
        self.shared.report()
    }

    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(stream) = lock(&self.shared.writer).take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for FakeHeadset {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Connects, runs for `duration`, and reports. A headset that never connects still reports,
/// with the reason as its error.
pub fn run(config: Config, duration: Duration) -> Report {
    let (port, profile, clock, window) = (
        config.port,
        config.profile.clone(),
        config.clock,
        config.display_lead_window_ns,
    );
    let start = monotonic_ns();
    let timeout = config.connect_timeout;
    match FakeHeadset::connect(config) {
        Ok(headset) => {
            thread::sleep(duration);
            headset.finish()
        }
        Err(e) => {
            let mut recorder = Recorder::new(start, port, profile, clock, window);
            let now = monotonic_ns();
            recorder.error(
                now,
                format!(
                    "could not connect to 127.0.0.1:{port} within {:.1} s: {e}",
                    timeout.as_secs_f64()
                ),
            );
            recorder.report(now)
        }
    }
}

struct Shared {
    config: Config,
    clock: HeadsetClock,
    start: i64,
    writer: Mutex<Option<TcpStream>>,
    recorder: Mutex<Recorder>,
    inputs: Mutex<[HandInput; 2]>,
    /// CLOCK_MONOTONIC until which the link is not read.
    paused_until: AtomicI64,
    stop: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn new(config: Config) -> Self {
        let start = monotonic_ns();
        let recorder = Recorder::new(
            start,
            config.port,
            config.profile.clone(),
            config.clock,
            config.display_lead_window_ns,
        );
        Self {
            clock: HeadsetClock::start(config.clock),
            start,
            writer: Mutex::new(None),
            recorder: Mutex::new(recorder),
            inputs: Mutex::new([HandInput::default(); 2]),
            paused_until: AtomicI64::new(i64::MIN),
            stop: AtomicBool::new(false),
            config,
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn seconds(&self, at: i64) -> f64 {
        (at - self.start) as f64 / 1e9
    }

    fn report(&self) -> Report {
        lock(&self.recorder).report(monotonic_ns())
    }

    fn first_connection(&self) -> io::Result<TcpStream> {
        let deadline = Instant::now() + self.config.connect_timeout;
        loop {
            match TcpStream::connect_timeout(&self.config.address(), RECONNECT) {
                Ok(stream) => return Ok(stream),
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(_) => thread::sleep(Duration::from_millis(50).min(RECONNECT)),
            }
        }
    }

    /// Opens a link as the Quest does: its hello first, then everything else.
    fn link_up(&self, stream: &TcpStream) -> io::Result<()> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(READ_POLL))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let mut hello = Vec::new();
        encode_framed(
            &Message::ClientHello(ClientHello {
                version: PROTOCOL_VERSION,
                profile: self.config.profile.clone(),
            }),
            &mut hello,
        );
        (&*stream).write_all(&hello)?;
        *lock(&self.writer) = Some(stream.try_clone()?);
        lock(&self.recorder).link_up(monotonic_ns());
        Ok(())
    }

    fn run_link(&self, mut stream: TcpStream) {
        loop {
            let reason = self.serve(&stream);
            if let Some(writer) = lock(&self.writer).take() {
                let _ = writer.shutdown(Shutdown::Both);
            }
            lock(&self.recorder).link_down(monotonic_ns(), reason, !self.stopped());
            stream = loop {
                if self.sleep_unless_stopped(RECONNECT) {
                    return;
                }
                if let Ok(next) = TcpStream::connect_timeout(&self.config.address(), RECONNECT)
                    && self.link_up(&next).is_ok()
                {
                    break next;
                }
            };
        }
    }

    /// `true` if the session ended first.
    fn sleep_unless_stopped(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        while !self.stopped() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::sleep(left.min(READ_POLL));
        }
        true
    }

    /// Reads the link until it ends; returns why it ended.
    fn serve(&self, stream: &TcpStream) -> String {
        let mut decoder = StreamDecoder::default();
        let mut buffer = vec![0u8; 256 << 10];
        let mut skipped = 0;
        loop {
            if self.stopped() {
                return "the session ended".into();
            }
            let (now, paused_until) = (monotonic_ns(), self.paused_until.load(Ordering::Acquire));
            if paused_until > now {
                thread::sleep(Duration::from_nanos((paused_until - now) as u64).min(READ_POLL));
                continue;
            }
            let n = match (&*stream).read(&mut buffer) {
                Ok(0) if self.stopped() => return "the session ended".into(),
                Ok(0) => return "the Mac closed the link".into(),
                Ok(n) => n,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    continue;
                }
                Err(_) if self.stopped() => return "the session ended".into(),
                Err(e) => return format!("reading failed: {e}"),
            };
            let arrival = monotonic_ns();
            decoder.push(&buffer[..n]);
            loop {
                match decoder.next_message() {
                    Ok(Some(message)) => self.handle(arrival, message),
                    Ok(None) => break,
                    Err(DecodeError::TooLarge(len)) => {
                        let reason = format!("a message claims {len} bytes; the stream is corrupt");
                        lock(&self.recorder).error(arrival, reason.clone());
                        return reason;
                    }
                    Err(e) => {
                        lock(&self.recorder).error(arrival, format!("malformed message: {e:?}"))
                    }
                }
            }
            if decoder.skipped() > skipped {
                lock(&self.recorder).unknown_skipped(decoder.skipped() - skipped);
                skipped = decoder.skipped();
            }
        }
    }

    fn handle(&self, arrival: i64, message: Message) {
        match message {
            Message::ClockPing { mac_ns } => {
                let client_ns = self.clock.now().0;
                if self.send(&Message::ClockPong { mac_ns, client_ns }) {
                    lock(&self.recorder).ping(monotonic_ns() - arrival);
                }
            }
            Message::ServerHello(hello) => {
                let started = lock(&self.recorder).server_hello(arrival, &hello);
                if started && self.config.keyframe_request_on_stream_start {
                    self.request_keyframe(KeyframeReason::StreamStart);
                }
            }
            Message::VideoFrame(frame) => {
                lock(&self.recorder).video_frame(arrival, self.clock.at(arrival), &frame);
            }
            Message::Haptics(h) => lock(&self.recorder).haptics(arrival, &h),
            Message::StreamEnded => lock(&self.recorder).stream_ended(arrival),
            Message::AudioStart(format) => lock(&self.recorder).audio_start(arrival, format),
            Message::Audio(packet) => {
                lock(&self.recorder).audio_packet(arrival, self.clock.at(arrival), &packet);
            }
            Message::AudioStop { stream } => lock(&self.recorder).audio_stop(arrival, stream),
            other @ (Message::ClientHello(_)
            | Message::Tracking(_)
            | Message::ClockPong { .. }
            | Message::Recenter
            | Message::KeyframeRequest
            | Message::InputFocus { .. }) => lock(&self.recorder).error(
                arrival,
                format!("the Mac sent {}, which only a headset sends", name(&other)),
            ),
        }
    }

    /// Writes one message; `false` when there is no link or it just failed.
    fn send(&self, message: &Message) -> bool {
        let mut bytes = Vec::new();
        encode_framed(message, &mut bytes);
        let mut writer = lock(&self.writer);
        let Some(stream) = writer.as_mut() else {
            return false;
        };
        if stream.write_all(&bytes).is_ok() {
            return true;
        }
        // The reader sees the shutdown and records why the link ended.
        let _ = stream.shutdown(Shutdown::Both);
        *writer = None;
        false
    }

    fn request_keyframe(&self, reason: KeyframeReason) {
        if self.send(&Message::KeyframeRequest) {
            lock(&self.recorder).keyframe_request(monotonic_ns(), reason);
        }
    }

    fn act(&self, action: Action, reason: KeyframeReason) {
        let now = monotonic_ns();
        lock(&self.recorder).action(now, format!("{action:?}"));
        let t = self.seconds(now);
        let hand = |h: Hand| h as usize;
        match action {
            Action::Press {
                hand: h,
                button,
                hold,
            } => {
                lock(&self.inputs)[hand(h)].press(button, t + hold.as_secs_f64());
            }
            Action::Trigger { hand: h, value } => lock(&self.inputs)[hand(h)].trigger = value,
            Action::Squeeze { hand: h, value } => lock(&self.inputs)[hand(h)].squeeze = value,
            Action::Thumbstick { hand: h, x, y } => {
                lock(&self.inputs)[hand(h)].thumbstick = [x, y];
            }
            Action::Recenter => {
                self.send(&Message::Recenter);
            }
            Action::RequestKeyframe => self.request_keyframe(reason),
            Action::Focus(focused) => {
                self.send(&Message::InputFocus { focused });
            }
            Action::PauseReading(duration) => {
                let until = now.saturating_add(duration.as_nanos().min(i64::MAX as u128) as i64);
                self.paused_until.fetch_max(until, Ordering::AcqRel);
            }
        }
    }

    /// Plays the cues and streams tracking at the profile's refresh rate, on an absolute grid
    /// that restarts instead of bursting after a stall.
    fn run_script(&self) {
        let period = (1e9 / f64::from(self.config.profile.refresh_rate.max(1.0))) as i64;
        let mut cues = self.config.script.cues.clone();
        cues.sort_by_key(|c| c.at);
        let mut cues = cues.into_iter().peekable();
        let mut deadline = monotonic_ns();
        while !self.stopped() {
            let now = monotonic_ns();
            let t = self.seconds(now);
            while let Some(cue) = cues.next_if(|c| c.at.as_secs_f64() <= t) {
                self.act(cue.action, KeyframeReason::Scripted);
            }
            self.send_tracking(now);
            deadline += period;
            if deadline < now - 2 * period {
                deadline = now + period;
            }
            let wait = deadline - monotonic_ns();
            if wait > 0 {
                thread::sleep(Duration::from_nanos(wait as u64));
            }
        }
    }

    fn send_tracking(&self, now: i64) {
        let sampled_at = now + self.config.tracking_lead.as_nanos() as i64;
        let sample = self.config.script.sample(self.seconds(sampled_at));
        let mut controllers = sample.controllers;
        let inputs = *lock(&self.inputs);
        for (state, input) in controllers.iter_mut().zip(&inputs) {
            input.apply(self.seconds(now), state);
        }
        let tracking = Tracking {
            sample_time_ns: self.clock.at(sampled_at).0,
            head: sample.head,
            head_velocity: sample.head_velocity,
            views: sample.views,
            controllers,
        };
        if self.send(&Message::Tracking(Box::new(tracking))) {
            lock(&self.recorder).tracking_sent();
        }
    }
}

fn name(message: &Message) -> &'static str {
    match message {
        Message::ClientHello(_) => "ClientHello",
        Message::ServerHello(_) => "ServerHello",
        Message::Tracking(_) => "Tracking",
        Message::ClockPing { .. } => "ClockPing",
        Message::ClockPong { .. } => "ClockPong",
        Message::VideoFrame(_) => "VideoFrame",
        Message::Recenter => "Recenter",
        Message::Haptics(_) => "Haptics",
        Message::KeyframeRequest => "KeyframeRequest",
        Message::InputFocus { .. } => "InputFocus",
        Message::StreamEnded => "StreamEnded",
        Message::AudioStart(_) => "AudioStart",
        Message::Audio(_) => "Audio",
        Message::AudioStop { .. } => "AudioStop",
    }
}
