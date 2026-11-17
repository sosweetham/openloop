//! The foreground app's sound, from capture to the headset (docs/ARCHITECTURE.md § Audio): one
//! `tether-audiotap` capture, pointed at the PID of whichever session is in front, and a thread
//! that converts its packets to the headset's format and sends them through the [`Stage`].
//!
//! The Core Audio tap is never touched before the System Audio Recording permission step has
//! passed at the Mac (`scripts/tetherd.sh audio-permission`, which writes the grant file), nor
//! after anything suggested a prompt may be pending (the block file): a tap operation issued
//! while a prompt was pending once wedged audio for every app on the Mac for hours.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use tether_audiotap::{
    AudioCapture, Config, EndReason, Event, Item, Packet, Packetizer, StreamFormat, Tone,
    status_text,
};
use tether_protocol::AudioFormat;

use crate::convert::{Converted, Converter};
use crate::log;
use crate::log::local_time;
use crate::session::Session;
use crate::stage::{Played, Stage};

/// How long a command waits while a capture runs; the packetizer's doorbell wakes it sooner.
const POLL: Duration = Duration::from_millis(10);
const IDLE: Duration = Duration::from_millis(500);
/// Capture events kept in a session's report.
const NOTES: usize = 32;
/// Sessions remembered for the capture events that arrive after the next capture starts.
const CAPTURED: usize = 4;
const FIRST_CALLBACK: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The Core Audio process tap of the app in front.
    Tap,
    /// A test tone in place of every app: no Core Audio, no permission.
    Synthetic,
    Off,
}

impl Source {
    pub fn parse(text: &str) -> Option<Source> {
        match text {
            "tap" => Some(Source::Tap),
            "synthetic" => Some(Source::Synthetic),
            "off" => Some(Source::Off),
            _ => None,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Tap => "tap",
            Source::Synthetic => "synthetic",
            Source::Off => "off",
        })
    }
}

/// The files that gate the tap, shared with `tetherd audio-permission` and `audio-check`.
#[derive(Clone, Debug)]
pub struct Permission {
    /// Written once the permission step has captured audio.
    pub granted: PathBuf,
    /// Written when a capture stalled or got no audio; while it exists nothing touches the HAL.
    pub blocked: PathBuf,
}

impl Permission {
    /// `~/Library/Application Support/Tether`, beside the installed tetherd.
    pub fn default_dir() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Library/Application Support/Tether")
    }

    pub fn in_dir(dir: PathBuf) -> Permission {
        Permission {
            granted: dir.join("audio-granted"),
            blocked: dir.join("audio-blocked"),
        }
    }
}

/// One session's sound, for its report.
#[derive(Default)]
pub struct Tally {
    pub streams: AtomicU64,
    pub packets: AtomicU64,
    pub frames: AtomicU64,
    pub no_headset: AtomicU64,
    pub background: AtomicU64,
    notes: Mutex<Vec<String>>,
}

impl Tally {
    fn note(&self, text: &str) {
        let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        if notes.len() < NOTES {
            notes.push(format!("{} {text}", local_time()));
        }
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn played(&self, played: Played, frames: usize) {
        let counter = match played {
            Played::Sent => {
                self.frames.fetch_add(frames as u64, Ordering::Relaxed);
                &self.packets
            }
            Played::NoHeadset => &self.no_headset,
            Played::Background => &self.background,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

enum Command {
    Follow(Arc<Session>),
    Release(u64),
}

/// The handle the NSXPC service drives; the capture and the sending run on their own threads.
pub struct Audio {
    commands: mpsc::Sender<Command>,
}

impl Audio {
    pub fn start(source: Source, stage: Arc<Stage>, permission: Permission) -> Audio {
        let (commands, receiver) = mpsc::channel();
        match source {
            Source::Off => log!("audio: off; apps play on the Mac"),
            Source::Synthetic => log!(
                "audio: a synthetic {} Hz tone stands in for every app",
                Tone::default().frequency_hz
            ),
            Source::Tap => {}
        }
        let mut sender = Sender {
            source,
            stage,
            permission,
            commands: receiver,
            engine: None,
            unavailable: false,
            following: None,
            captured: VecDeque::with_capacity(CAPTURED),
            outgoing: Outgoing::default(),
            next_stream: 0,
        };
        let spawned = thread::Builder::new()
            .name("tetherd-audio".into())
            .spawn(move || sender.run());
        if let Err(e) = spawned {
            log!("audio: cannot start its thread: {e}");
        }
        Audio { commands }
    }

    /// Captures `session`'s app from now on, in place of any other.
    pub fn follow(&self, session: Arc<Session>) {
        let _ = self.commands.send(Command::Follow(session));
    }

    /// Stops capturing `session`'s app, if it is the one followed.
    pub fn release(&self, session: u64) {
        let _ = self.commands.send(Command::Release(session));
    }
}

struct Engine {
    capture: AudioCapture,
    packetizer: Packetizer,
}

/// What is being sent: one capture stream, converted for the headset it started on.
#[derive(Default)]
struct Outgoing {
    /// The capture stream, and the session its packets are counted for.
    source: Option<(StreamFormat, Arc<Session>)>,
    /// The stream on the wire, made at its first packet; a headset in another format ends it.
    wire: Option<Wire>,
}

struct Wire {
    format: AudioFormat,
    sequence: u32,
    converter: Converter,
}

struct Sender {
    source: Source,
    stage: Arc<Stage>,
    permission: Permission,
    commands: Receiver<Command>,
    engine: Option<Engine>,
    /// The capture engine could not be made; it is never tried again.
    unavailable: bool,
    following: Option<Arc<Session>>,
    /// The sessions whose apps were captured last, newest first, for the events that follow.
    captured: VecDeque<Arc<Session>>,
    outgoing: Outgoing,
    next_stream: u16,
}

impl Sender {
    fn run(&mut self) {
        loop {
            let capturing = self.following.is_some() || self.outgoing.source.is_some();
            match &self.engine {
                // The packetizer's doorbell is what wakes a running capture.
                Some(engine) if capturing => {
                    engine.packetizer.wait(POLL);
                }
                _ => match self.commands.recv_timeout(IDLE) {
                    Ok(command) => self.handle(command),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return self.shut_down(),
                },
            }
            loop {
                match self.commands.try_recv() {
                    Ok(command) => self.handle(command),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return self.shut_down(),
                }
            }
            self.pump();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Follow(session) => self.follow(session),
            Command::Release(session) => self.release(session),
        }
    }

    fn shut_down(&mut self) {
        if let Some(engine) = self.engine.take()
            && !engine.capture.shutdown(Duration::from_secs(2))
        {
            log!("audio: the capture thread is still inside a Core Audio call; leaving it");
        }
    }

    fn follow(&mut self, session: Arc<Session>) {
        self.following = Some(session.clone());
        let Some(pid) = session.audio_pid else {
            self.say(
                &session,
                "not captured: the app's connection has no audit token",
            );
            return;
        };
        match self.source {
            Source::Off => return,
            Source::Synthetic => {}
            Source::Tap => {
                if let Ok(reason) = std::fs::read_to_string(&self.permission.blocked) {
                    self.say(
                        &session,
                        &format!(
                            "not captured: capture is blocked ({}); answer any System Audio \
                             Recording prompt at the Mac, then run scripts/tetherd.sh \
                             audio-permission --prompt-answered",
                            reason.trim()
                        ),
                    );
                    return;
                }
                if !self.permission.granted.exists() {
                    self.say(
                        &session,
                        "not captured: run scripts/tetherd.sh audio-permission at the Mac once \
                         to grant System Audio Recording",
                    );
                    return;
                }
            }
        }
        if let Some(engine) = self.engine() {
            engine.capture.capture(pid);
            self.captured.truncate(CAPTURED - 1);
            self.captured.push_front(session);
        }
    }

    fn release(&mut self, session: u64) {
        if self.following.as_ref().is_some_and(|s| s.id == session) {
            self.following = None;
            if let Some(engine) = &self.engine {
                engine.capture.stop();
            }
        }
    }

    /// The capture engine, made the first time a session's app is to be captured.
    fn engine(&mut self) -> Option<&Engine> {
        if self.engine.is_none() && !self.unavailable {
            let source = match self.source {
                Source::Tap => tether_audiotap::Source::Tap,
                _ => tether_audiotap::Source::Synthetic(Tone::default()),
            };
            let config = Config {
                block_marker: (self.source == Source::Tap).then(|| self.permission.blocked.clone()),
                // Whether a tap calls back while its app plays nothing is unverified, and a
                // silent start blocks capture until a person acts. Waiting longer issues no
                // further HAL call, so it costs no safety.
                first_callback_timeout: FIRST_CALLBACK,
                ..Config::default()
            };
            match AudioCapture::spawn(source, config) {
                Ok((capture, packetizer)) => {
                    self.engine = Some(Engine {
                        capture,
                        packetizer,
                    })
                }
                Err(e) => {
                    log!("audio: no capture in this process ({e}); apps play on the Mac");
                    self.unavailable = true;
                }
            }
        }
        self.engine.as_ref()
    }

    fn say(&self, session: &Session, text: &str) {
        log!("audio, session {} ({}): {text}", session.id, session.app);
        session.audio.note(text);
    }

    /// Handles what the capture reported and sends what it captured.
    fn pump(&mut self) {
        let Some(engine) = &mut self.engine else {
            return;
        };
        while let Some(event) = engine.capture.try_event() {
            let session = match pid_of(&event) {
                Some(pid) => self.captured.iter().find(|s| s.audio_pid == Some(pid)),
                None => self.captured.front(),
            };
            describe(&event, session.map(|s| &**s));
        }
        while let Some(item) = engine.packetizer.poll() {
            match item {
                Item::Start(format) => {
                    self.outgoing.end(&self.stage);
                    if let Some(session) = self.following.clone() {
                        session.audio.streams.fetch_add(1, Ordering::Relaxed);
                        self.outgoing.source = Some((format, session));
                    }
                }
                Item::Packet(packet) => {
                    self.outgoing
                        .packet(&packet, &self.stage, &mut self.next_stream);
                }
                Item::End { stream } => {
                    if self
                        .outgoing
                        .source
                        .as_ref()
                        .is_some_and(|(format, _)| format.stream == stream)
                    {
                        self.outgoing.end(&self.stage);
                    }
                }
            }
        }
    }
}

impl Outgoing {
    fn packet(&mut self, packet: &Packet<'_>, stage: &Stage, next_stream: &mut u16) {
        let Some((source, session)) = &self.source else {
            return;
        };
        let source_rate = source.sample_rate;
        let Some((rate, channels)) = stage.audio_output() else {
            session.audio.played(Played::NoHeadset, 0);
            return;
        };
        if self
            .wire
            .as_ref()
            .is_some_and(|w| (w.format.sample_rate, w.format.channels) != (rate, channels))
        {
            self.end_wire(stage);
        }
        let Some((_, session)) = &self.source else {
            return;
        };
        let wire = self.wire.get_or_insert_with(|| {
            let stream = *next_stream;
            *next_stream = next_stream.wrapping_add(1);
            Wire {
                format: AudioFormat {
                    stream,
                    sample_rate: rate,
                    channels,
                },
                sequence: 0,
                converter: Converter::new(
                    source_rate,
                    rate,
                    channels,
                    packet.frames(),
                    packet.first_frame,
                ),
            }
        });
        let (format, sequence) = (wire.format, &mut wire.sequence);
        wire.converter.push(
            packet.first_frame,
            packet.host_time_ns,
            packet.samples,
            &mut |c| deliver(stage, session, format, sequence, c),
        );
    }

    /// Sends what the converter still holds, then `AudioStop`.
    fn end_wire(&mut self, stage: &Stage) {
        let Some(mut wire) = self.wire.take() else {
            return;
        };
        if let Some((_, session)) = &self.source {
            let (format, sequence) = (wire.format, &mut wire.sequence);
            wire.converter
                .finish(&mut |c| deliver(stage, session, format, sequence, c));
        }
        stage.end_audio(wire.format.stream);
    }

    fn end(&mut self, stage: &Stage) {
        self.end_wire(stage);
        self.source = None;
    }
}

fn deliver(
    stage: &Stage,
    session: &Session,
    format: AudioFormat,
    sequence: &mut u32,
    packet: Converted<'_>,
) {
    let played = stage.audio(
        session.id,
        format,
        *sequence,
        packet.first_frame,
        packet.host_time_ns,
        packet.samples,
    );
    *sequence = sequence.wrapping_add(1);
    let frames = packet.samples.len() / usize::from(format.channels.max(1));
    session.audio.played(played, frames);
}

fn pid_of(event: &Event) -> Option<i32> {
    match *event {
        Event::WaitingForProcess { pid }
        | Event::Started { pid, .. }
        | Event::Receiving { pid, .. }
        | Event::Restarted { pid, .. }
        | Event::UnsupportedFormat { pid, .. }
        | Event::Failed { pid, .. }
        | Event::Unreadable { pid, .. }
        | Event::Refused { pid, .. }
        | Event::Ended { pid, .. } => Some(pid),
        Event::SlowCall { .. } | Event::Overrun { .. } | Event::Blocked(_) => None,
    }
}

/// Logs a capture event and notes it in the report of the session it concerns.
fn describe(event: &Event, session: Option<&Session>) {
    let text = match event {
        Event::WaitingForProcess { pid } => {
            format!("pid {pid} has not used audio yet; capture starts when it does")
        }
        Event::Started { pid, sample_rate } => {
            format!("capturing pid {pid} at {sample_rate} Hz")
        }
        Event::Receiving { pid, after_ms } => {
            format!("pid {pid}'s sound is arriving ({after_ms} ms after start)")
        }
        Event::Restarted {
            pid,
            reason,
            sample_rate,
        } => format!("capture of pid {pid} rebuilt after {reason:?}, now {sample_rate} Hz"),
        Event::UnsupportedFormat {
            pid,
            sample_rate,
            channels,
            float32,
        } => format!(
            "pid {pid}'s tap delivers {sample_rate} Hz × {channels}{}, which is not captured",
            if *float32 { "" } else { ", not Float32" }
        ),
        Event::SlowCall { call, elapsed_ms } => {
            format!("Core Audio took {elapsed_ms} ms for {call:?}")
        }
        Event::Failed { pid, call, status } => format!(
            "capture of pid {pid} failed in {call:?}: {}",
            status_text(*status)
        ),
        Event::Overrun { dropped_frames } => {
            format!("{dropped_frames} captured frames dropped: the sender fell behind")
        }
        Event::Unreadable { pid, callbacks } => {
            format!("{callbacks} capture callbacks for pid {pid} carried no readable audio")
        }
        Event::Blocked(reason) => format!(
            "capture blocked: {reason}. Nothing touches Core Audio again until a person acts: \
             answer any System Audio Recording prompt, check System Settings > Privacy & \
             Security > Screen & System Audio Recording, then run scripts/tetherd.sh \
             audio-permission --prompt-answered"
        ),
        Event::Refused { pid, reason } => {
            format!("pid {pid} not captured: capture is blocked ({reason})")
        }
        Event::Ended { pid, reason } => match reason {
            EndReason::Stopped | EndReason::Replaced => format!("capture of pid {pid} ended"),
            other => format!("capture of pid {pid} ended: {other:?}"),
        },
    };
    match session {
        Some(session) => {
            log!("audio, session {} ({}): {text}", session.id, session.app);
            session.audio.note(&text);
        }
        None => log!("audio: {text}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tether_fake_headset::FakeHeadset;
    use tether_stream::{Headset, PIPELINE_LATENCY_NS, UsbTransport};

    use super::*;
    use crate::daemon::Hub;
    use crate::session::Owner;

    #[test]
    fn the_app_in_front_is_heard_the_pipeline_latency_after_its_capture() {
        let hub = Arc::new(Hub::default());
        let headset = Headset::observed(Some(hub.clone()));
        let transport = UsbTransport::bind_port(0).unwrap();
        let port = transport.port();
        headset.accept(Box::new(transport));
        let stage = Arc::new(Stage::new(headset, hub.clone()));
        let fake = FakeHeadset::connect(tether_fake_headset::Config::new(port)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while stage.audio_output().is_none() {
            assert!(
                Instant::now() < deadline,
                "the fake headset never became ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(stage.audio_output(), Some((48_000, 2)));

        let owner = Owner {
            pid: 4242,
            pid_version: 1,
        };
        let session = Session::new(7, owner, "test".into(), String::new(), Some(4242), 1).unwrap();
        let state = std::env::temp_dir().join(format!("tetherd-audio-{}", std::process::id()));
        let audio = Audio::start(Source::Synthetic, stage.clone(), Permission::in_dir(state));
        hub.bring_forward(session.id);
        audio.follow(session.clone());
        thread::sleep(Duration::from_millis(1500));
        audio.release(session.id);
        let stopped = fake.wait_for(Duration::from_secs(2), |r| r.audio.stops == 1);
        let report = fake.finish();
        let a = &report.audio;
        assert!(stopped, "no AudioStop: {a:#?}");
        assert_eq!(
            (a.starts, a.lost_packets, a.missing_frames, a.late_packets),
            (1, 0, 0, 0)
        );
        assert!(a.packets >= 250, "{} packets in 1.5 s", a.packets);
        assert_eq!(a.packet_frames.max, 240.0);
        assert!(
            a.spacing_error_ms.p50.abs() < 0.01,
            "packets are spaced by their frames: {:?}",
            a.spacing_error_ms
        );
        let target_ms = PIPELINE_LATENCY_NS as f64 / 1e6;
        assert!(
            a.lead_ms.p50 > target_ms - 20.0 && a.lead_ms.max < target_ms + 2.0,
            "presented about {target_ms} ms after capture: {:?}",
            a.lead_ms
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            session.audio.packets.load(Ordering::Relaxed),
            a.packets,
            "the session's report counts what the headset got"
        );
    }
}
