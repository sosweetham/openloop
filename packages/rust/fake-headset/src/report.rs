//! The session report: what the Mac sent and when, the checks it failed, and the numbers the
//! quality gates are judged on. Serialises to the JSON the command line writes.

use serde::Serialize;
use tether_protocol::{
    AudioFormat, AudioPacket, ClientTime, DeviceProfile, Haptics, PROTOCOL_VERSION, ServerHello,
    VideoFrame,
};

use crate::audio::{AudioCheck, AudioReport};
use crate::clock::ClockModel;
use crate::video::{Codec, VideoCheck, VideoReport};

/// The report lists this many errors and as many warnings; the counts cover all of them.
const LISTED: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The Mac broke the protocol or sent something no headset could use.
    Error,
    /// Worth a look, and fine in moderation: lost frames, late audio, a dropped link.
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    pub fn error(message: String) -> Self {
        Self {
            severity: Severity::Error,
            message,
        }
    }

    pub fn warning(message: String) -> Self {
        Self {
            severity: Severity::Warning,
            message,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub count: usize,
    pub mean: f64,
    /// Population standard deviation.
    pub stddev: f64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Stats {
    /// Percentiles are nearest-rank. All zero when `values` is empty.
    pub fn of(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();
        let mean = sorted.iter().sum::<f64>() / n as f64;
        let variance = sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        let rank = |p: f64| sorted[((p * n as f64).ceil() as usize).clamp(1, n) - 1];
        Self {
            count: n,
            mean,
            stddev: variance.sqrt(),
            min: sorted[0],
            p50: rank(0.50),
            p95: rank(0.95),
            p99: rank(0.99),
            max: sorted[n - 1],
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileReport {
    pub name: String,
    pub eye_width: u32,
    pub eye_height: u32,
    pub refresh_rates: Vec<f32>,
    pub refresh_rate: f32,
    pub interaction_profile: String,
    pub supports_hevc: bool,
    pub audio_sample_rate: u32,
    pub audio_channels: u8,
}

impl From<&DeviceProfile> for ProfileReport {
    fn from(p: &DeviceProfile) -> Self {
        Self {
            name: p.name.clone(),
            eye_width: p.eye_width,
            eye_height: p.eye_height,
            refresh_rates: p.refresh_rates.clone(),
            refresh_rate: p.refresh_rate,
            interaction_profile: p.interaction_profile.clone(),
            supports_hevc: p.supports_hevc,
            audio_sample_rate: p.audio_sample_rate,
            audio_channels: p.audio_channels,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LinkEvent {
    pub at_s: f64,
    pub event: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LinkReport {
    pub connects: u64,
    /// Links that ended before the session did.
    pub drops: u64,
    pub events: Vec<LinkEvent>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServerHelloEvent {
    pub at_s: f64,
    pub version: u16,
    pub video_width: u32,
    pub video_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyframeReason {
    /// A stream started; the Quest's decoder asks for one whenever it starts.
    StreamStart,
    Scripted,
    /// `FakeHeadset::act` from the code driving the session.
    Caller,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyframeRequestEvent {
    pub at_s: f64,
    pub reason: KeyframeReason,
    /// Frames that arrived after the request up to and including the next keyframe.
    pub answered_after_frames: Option<u64>,
    pub answered_after_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HapticsEvent {
    pub at_s: f64,
    pub hand: u8,
    pub amplitude: f32,
    /// 0 stops the vibration.
    pub duration_ms: f64,
    pub frequency: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActionEvent {
    pub at_s: f64,
    pub action: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PingReport {
    pub answered: u64,
    /// From the ping's arrival to its pong being written.
    pub turnaround_us: Stats,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// No errors: everything the Mac sent kept to the protocol and was usable.
    pub passed: bool,
    pub protocol_version: u16,
    pub port: u16,
    /// From the start of the session to this report.
    pub duration_s: f64,
    pub profile: ProfileReport,
    pub clock: ClockModel,
    pub link: LinkReport,
    pub server_hellos: Vec<ServerHelloEvent>,
    pub video: VideoReport,
    pub keyframe_requests: Vec<KeyframeRequestEvent>,
    pub stream_ended_at_s: Vec<f64>,
    pub haptics: Vec<HapticsEvent>,
    pub audio: AudioReport,
    pub tracking_sent: u64,
    pub clock_pings: PingReport,
    /// Cues and calls the session acted on, in order.
    pub actions: Vec<ActionEvent>,
    /// Messages of kinds this build does not know; skipping them is the protocol's rule.
    pub unknown_messages_skipped: u64,
    pub error_count: u64,
    pub errors: Vec<String>,
    pub warning_count: u64,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("the report is plain data")
    }
}

/// Collects one session's observations. Times are CLOCK_MONOTONIC nanoseconds unless typed as
/// the headset clock.
pub struct Recorder {
    start: i64,
    port: u16,
    profile: DeviceProfile,
    clock: ClockModel,
    video: VideoCheck,
    audio: AudioCheck,
    link: LinkReport,
    server_hellos: Vec<ServerHelloEvent>,
    keyframe_requests: Vec<KeyframeRequestEvent>,
    /// Requests still waiting for a keyframe: index into `keyframe_requests`, frames since, and
    /// when the request was made.
    open_requests: Vec<(usize, u64, i64)>,
    stream_ended_at_s: Vec<f64>,
    haptics: Vec<HapticsEvent>,
    tracking_sent: u64,
    pings: u64,
    ping_turnarounds: Vec<f64>,
    actions: Vec<ActionEvent>,
    unknown_skipped: u64,
    errors: (u64, Vec<String>),
    warnings: (u64, Vec<String>),
}

impl Recorder {
    pub fn new(
        start: i64,
        port: u16,
        profile: DeviceProfile,
        clock: ClockModel,
        lead_window_ns: (i64, i64),
    ) -> Self {
        let codec = if profile.supports_hevc {
            Codec::Hevc
        } else {
            Codec::H264
        };
        Self {
            start,
            port,
            video: VideoCheck::new(codec, profile.refresh_rate, lead_window_ns),
            audio: AudioCheck::new(profile.audio_sample_rate, profile.audio_channels),
            profile,
            clock,
            link: LinkReport::default(),
            server_hellos: Vec::new(),
            keyframe_requests: Vec::new(),
            open_requests: Vec::new(),
            stream_ended_at_s: Vec::new(),
            haptics: Vec::new(),
            tracking_sent: 0,
            pings: 0,
            ping_turnarounds: Vec::new(),
            actions: Vec::new(),
            unknown_skipped: 0,
            errors: (0, Vec::new()),
            warnings: (0, Vec::new()),
        }
    }

    pub fn seconds(&self, at: i64) -> f64 {
        (at - self.start) as f64 / 1e9
    }

    pub fn note(&mut self, at: i64, findings: Vec<Finding>) {
        let at_s = self.seconds(at);
        for finding in findings {
            let (count, listed) = match finding.severity {
                Severity::Error => &mut self.errors,
                Severity::Warning => &mut self.warnings,
            };
            *count += 1;
            if listed.len() < LISTED {
                listed.push(format!("{at_s:.3} s: {}", finding.message));
            }
        }
    }

    pub fn error(&mut self, at: i64, message: String) {
        self.note(at, vec![Finding::error(message)]);
    }

    pub fn link_up(&mut self, at: i64) {
        self.link.connects += 1;
        self.link_event(at, "connected".into());
    }

    /// `dropped`: the link ended before the session did.
    pub fn link_down(&mut self, at: i64, reason: String, dropped: bool) {
        if dropped {
            self.link.drops += 1;
            self.note(at, vec![Finding::warning(format!("link lost: {reason}"))]);
        }
        self.link_event(at, format!("disconnected: {reason}"));
        let at_s = self.seconds(at);
        self.video.end(at_s);
        self.audio.link_down(at_s);
    }

    fn link_event(&mut self, at: i64, event: String) {
        let at_s = self.seconds(at);
        self.link.events.push(LinkEvent { at_s, event });
    }

    /// `true` when the hello starts a video stream.
    pub fn server_hello(&mut self, at: i64, hello: &ServerHello) -> bool {
        let at_s = self.seconds(at);
        self.server_hellos.push(ServerHelloEvent {
            at_s,
            version: hello.version,
            video_width: hello.video_width,
            video_height: hello.video_height,
        });
        let mut findings = Vec::new();
        let started = self.video.server_hello(at_s, hello, &mut findings);
        self.note(at, findings);
        started
    }

    pub fn video_frame(&mut self, at: i64, at_client: ClientTime, frame: &VideoFrame) {
        let mut findings = Vec::new();
        self.video.frame(at, at_client, frame, &mut findings);
        self.note(at, findings);
        for open in &mut self.open_requests {
            open.1 += 1;
        }
        if frame.keyframe {
            for (index, frames, requested_at) in self.open_requests.drain(..) {
                let request = &mut self.keyframe_requests[index];
                request.answered_after_frames = Some(frames);
                request.answered_after_ms = Some((at - requested_at) as f64 / 1e6);
            }
        }
    }

    pub fn keyframe_request(&mut self, at: i64, reason: KeyframeReason) {
        self.open_requests
            .push((self.keyframe_requests.len(), 0, at));
        self.keyframe_requests.push(KeyframeRequestEvent {
            at_s: self.seconds(at),
            reason,
            answered_after_frames: None,
            answered_after_ms: None,
        });
    }

    pub fn stream_ended(&mut self, at: i64) {
        let at_s = self.seconds(at);
        self.stream_ended_at_s.push(at_s);
        self.video.end(at_s);
    }

    pub fn haptics(&mut self, at: i64, h: &Haptics) {
        let mut problems = Vec::new();
        if h.hand > 1 {
            problems.push(format!("hand {}", h.hand));
        }
        if !(0.0..=1.0).contains(&h.amplitude) {
            problems.push(format!("amplitude {}", h.amplitude));
        }
        if h.duration_ns < 0 {
            problems.push(format!("duration {} ns", h.duration_ns));
        }
        if !(h.frequency.is_finite() && h.frequency >= 0.0) {
            problems.push(format!("frequency {}", h.frequency));
        }
        if !problems.is_empty() {
            self.error(at, format!("haptics with {}", problems.join(", ")));
        }
        self.haptics.push(HapticsEvent {
            at_s: self.seconds(at),
            hand: h.hand,
            amplitude: h.amplitude,
            duration_ms: h.duration_ns as f64 / 1e6,
            frequency: h.frequency,
        });
    }

    pub fn audio_start(&mut self, at: i64, format: AudioFormat) {
        let mut findings = Vec::new();
        self.audio.start(self.seconds(at), format, &mut findings);
        self.note(at, findings);
    }

    pub fn audio_packet(&mut self, at: i64, at_client: ClientTime, packet: &AudioPacket) {
        let mut findings = Vec::new();
        self.audio.packet(at_client, packet, &mut findings);
        self.note(at, findings);
    }

    pub fn audio_stop(&mut self, at: i64, stream: u16) {
        let mut findings = Vec::new();
        self.audio.stop(self.seconds(at), stream, &mut findings);
        self.note(at, findings);
    }

    pub fn ping(&mut self, turnaround_ns: i64) {
        self.pings += 1;
        self.ping_turnarounds.push(turnaround_ns as f64 / 1e3);
    }

    pub fn tracking_sent(&mut self) {
        self.tracking_sent += 1;
    }

    pub fn action(&mut self, at: i64, action: String) {
        let at_s = self.seconds(at);
        self.actions.push(ActionEvent { at_s, action });
    }

    pub fn unknown_skipped(&mut self, count: u64) {
        self.unknown_skipped += count;
    }

    pub fn report(&self, now: i64) -> Report {
        Report {
            passed: self.errors.0 == 0,
            protocol_version: PROTOCOL_VERSION,
            port: self.port,
            duration_s: self.seconds(now),
            profile: ProfileReport::from(&self.profile),
            clock: self.clock,
            link: self.link.clone(),
            server_hellos: self.server_hellos.clone(),
            video: self.video.report(),
            keyframe_requests: self.keyframe_requests.clone(),
            stream_ended_at_s: self.stream_ended_at_s.clone(),
            haptics: self.haptics.clone(),
            audio: self.audio.report(),
            tracking_sent: self.tracking_sent,
            clock_pings: PingReport {
                answered: self.pings,
                turnaround_us: Stats::of(&self.ping_turnarounds),
            },
            actions: self.actions.clone(),
            unknown_messages_skipped: self.unknown_skipped,
            error_count: self.errors.0,
            errors: self.errors.1.clone(),
            warning_count: self.warnings.0,
            warnings: self.warnings.1.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_use_nearest_rank_percentiles() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = Stats::of(&values);
        assert_eq!(
            (s.count, s.min, s.p50, s.p95, s.p99, s.max),
            (100, 1.0, 50.0, 95.0, 99.0, 100.0)
        );
        assert!((s.mean - 50.5).abs() < 1e-12);
        assert!((s.stddev - 28.866_070_047_722_12).abs() < 1e-9);
        assert_eq!(Stats::of(&[]), Stats::default());
        assert_eq!(Stats::of(&[3.0]).p99, 3.0);
    }

    #[test]
    fn findings_are_counted_beyond_the_listed_ones() {
        let mut recorder = Recorder::new(
            0,
            1,
            DeviceProfile::default(),
            ClockModel::default(),
            (0, 1),
        );
        for i in 0..(LISTED as i64 + 5) {
            recorder.error(i * 1_000_000, format!("error {i}"));
        }
        let report = recorder.report(1_000_000_000);
        assert_eq!(report.error_count, LISTED as u64 + 5);
        assert_eq!(report.errors.len(), LISTED);
        assert_eq!(report.errors[1], "0.001 s: error 1");
        assert!(!report.passed);
    }
}
