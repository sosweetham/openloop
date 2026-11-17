//! Judges a run from the fake headset's JSON report and the processes' exits and logs.

use std::fmt;
use std::path::Path;
use std::process::ExitStatus;

use serde::Deserialize;
use tether_stream::{PIPELINE_LATENCY_NS, video};

use crate::harness;

/// A stream must start this soon after the headset connects.
const STREAM_START_S: f64 = 5.0;
/// Frames must arrive within this fraction of the headset's refresh rate, either way.
const RATE_TOLERANCE: f64 = 0.1;
/// A keyframe request this close to the end of the session may still be in flight.
const REQUEST_GRACE_S: f64 = 1.0;
/// Capture to send: up to a capture buffer (512 frames, 10.7 ms) and a packet (5 ms) of waiting,
/// with room for a loaded Mac.
const AUDIO_SEND_DELAY_MS: f64 = 20.0;
/// How far a presentation time may stray from its predecessor's plus the frames between them:
/// the clock mapping moves when a faster ping refines it.
const AUDIO_SPACING_MS: f64 = 2.0;

/// The fields of `tether-fake-headset`'s report that a run is judged on.
#[derive(Debug, Deserialize)]
pub struct Report {
    pub duration_s: f64,
    pub profile: Profile,
    pub link: Link,
    pub server_hellos: Vec<ServerHello>,
    pub video: Video,
    pub keyframe_requests: Vec<KeyframeRequest>,
    #[serde(default)]
    pub stream_ended_at_s: Vec<f64>,
    pub clock_pings: Pings,
    #[serde(default)]
    pub audio: Audio,
    pub error_count: u64,
    pub errors: Vec<String>,
    pub warning_count: u64,
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    pub eye_width: u32,
    pub eye_height: u32,
    pub refresh_rate: f32,
}

#[derive(Debug, Deserialize)]
pub struct Link {
    pub connects: u64,
    pub drops: u64,
    pub events: Vec<LinkEvent>,
}

#[derive(Debug, Deserialize)]
pub struct LinkEvent {
    pub at_s: f64,
    pub event: String,
}

#[derive(Debug, Deserialize)]
pub struct ServerHello {
    pub at_s: f64,
    pub video_width: u32,
    pub video_height: u32,
}

#[derive(Debug, Deserialize)]
pub struct Video {
    pub frames: u64,
    pub index_gaps: u64,
    pub missing_frames: u64,
    pub arrival_interval_ms: Stats,
    pub arrival_intervals_over_2x_period: u64,
    pub streams: Vec<Stream>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Stats {
    pub count: u64,
    pub mean: f64,
    #[serde(default)]
    pub min: f64,
    pub p50: f64,
    pub p99: f64,
    pub max: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Audio {
    pub starts: u64,
    pub stops: u64,
    pub packets: u64,
    pub frames: u64,
    pub lost_packets: u64,
    pub missing_frames: u64,
    pub late_packets: u64,
    pub lead_ms: Stats,
    pub spacing_error_ms: Stats,
    pub streams: Vec<AudioStream>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AudioStream {
    pub stream: u16,
    pub sample_rate: u32,
    pub started_at_s: Option<f64>,
    pub ended_at_s: Option<f64>,
    pub packets: u64,
    pub frames: u64,
    pub rms: f64,
    pub frequency_hz: f64,
}

#[derive(Debug, Deserialize)]
pub struct Stream {
    pub video_width: u32,
    pub video_height: u32,
    pub began_at_s: f64,
    pub ended_at_s: Option<f64>,
    pub first_index: Option<u64>,
    pub began_with_keyframe: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct KeyframeRequest {
    pub at_s: f64,
    pub answered_after_frames: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Pings {
    pub answered: u64,
}

pub struct Check {
    pub ok: bool,
    pub name: String,
    /// Shown only when the check fails.
    pub detail: String,
}

impl Check {
    pub fn new(ok: bool, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            ok,
            name: name.into(),
            detail: detail.into(),
        }
    }

    pub fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(false, name, detail)
    }

    /// Passes on status 0 with no thread panicked. `status` is `None` when the process was still
    /// running and had to be killed.
    pub fn exited(name: impl Into<String>, status: Option<ExitStatus>, stderr: &Path) -> Self {
        let panicked = !harness::lines_with(stderr, &["panicked at"]).is_empty();
        Self::new(
            status.is_some_and(|s| s.success()) && !panicked,
            name,
            match status {
                Some(s) => format!("{s}\n{}", harness::tail(stderr, 20)),
                None => "still running; killed".into(),
            },
        )
    }

    /// hello_xr logs every failed OpenXR call at error level, on stderr.
    pub fn no_errors_logged(name: &str, stderr: &Path) -> Self {
        let errors = harness::lines_with(stderr, &["[Error", "panicked at"]);
        Self::new(
            errors.is_empty(),
            format!("{name} logs no errors"),
            errors.join("\n"),
        )
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}",
            if self.ok { "  ok  " } else { "  FAIL" },
            self.name
        )?;
        if !self.ok {
            for line in self.detail.lines() {
                write!(f, "\n         {line}")?;
            }
        }
        Ok(())
    }
}

/// An app killed mid-stream, `killed_at_s` after the fake headset started, and the next app
/// started on the same link.
pub struct Handover {
    pub killed_at_s: f64,
    /// The killed app's stream must end this soon after the kill.
    pub limit_s: f64,
}

/// The sound a run must deliver: `tetherd`'s synthetic tone, to be heard the pipeline latency
/// after its capture.
pub struct Sound {
    pub target_delay_ms: f64,
    pub tone_hz: f64,
    pub tone_rms: f64,
}

impl Sound {
    pub fn synthetic() -> Self {
        let tone = tether_audiotap::Tone::default();
        Self {
            target_delay_ms: PIPELINE_LATENCY_NS as f64 / 1e6,
            tone_hz: tone.frequency_hz,
            tone_rms: f64::from(tone.amplitude) / 2f64.sqrt(),
        }
    }
}

pub fn judge_file(path: &Path, handover: Option<Handover>, sound: Option<Sound>) -> Vec<Check> {
    match std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice::<Report>(&bytes).map_err(|e| e.to_string()))
    {
        Ok(report) => {
            let mut checks = judge(&report);
            if let Some(sound) = &sound {
                checks.extend(heard(&report, sound));
            }
            if let Some(h) = &handover {
                checks.push(handed_over(&report, h));
                if sound.is_some() {
                    checks.push(sound_handed_over(&report, h));
                }
            }
            checks
        }
        Err(e) => vec![Check::fail(
            "the fake headset's report reads",
            format!("{}: {e}", path.display()),
        )],
    }
}

pub fn judge(r: &Report) -> Vec<Check> {
    vec![
        link_held(r),
        stream_started(r),
        keyframe_first(r),
        refresh_rate(r),
        no_gaps(r),
        keyframe_requests_answered(r),
        clock_mapped(r),
        no_errors(r),
    ]
}

/// The killed app's stream ends with `StreamEnded`, and the next app's starts after it.
fn handed_over(r: &Report, h: &Handover) -> Check {
    let hellos: Vec<f64> = r
        .server_hellos
        .iter()
        .filter(|s| s.video_width > 0 && s.video_height > 0)
        .map(|s| s.at_s)
        .collect();
    let ended = match hellos.as_slice() {
        [first, second, ..] => r
            .stream_ended_at_s
            .iter()
            .copied()
            .find(|&e| e > *first && e < *second),
        _ => None,
    };
    // Both times count from about the headset's start; its own clock starts a little later.
    let after_kill = ended.map(|e| e - h.killed_at_s);
    Check::new(
        after_kill.is_some_and(|after| after < h.limit_s),
        format!(
            "the killed app's stream ends within {:.0} s and the next one starts",
            h.limit_s
        ),
        format!(
            "streams announced at {hellos:?} s; StreamEnded at {:?} s; killed at {:.2} s",
            r.stream_ended_at_s, h.killed_at_s
        ),
    )
}

/// The killed app's sound stops with `AudioStop`, and the next app's plays in a new stream.
fn sound_handed_over(r: &Report, h: &Handover) -> Check {
    let streams = &r.audio.streams;
    let stopped_after_kill = streams
        .first()
        .and_then(|s| s.ended_at_s)
        .map(|end| end - h.killed_at_s);
    let next = streams.get(1).and_then(|s| s.started_at_s);
    Check::new(
        r.audio.stops >= 1
            && stopped_after_kill.is_some_and(|after| after < h.limit_s)
            && next.is_some(),
        format!(
            "the killed app's sound stops within {:.0} s and the next app's starts",
            h.limit_s
        ),
        format!(
            "{} AudioStop(s); streams {:?}; killed at {:.2} s",
            r.audio.stops,
            streams
                .iter()
                .map(|s| (s.stream, s.started_at_s, s.ended_at_s))
                .collect::<Vec<_>>(),
            h.killed_at_s
        ),
    )
}

/// Continuous packets, spaced by their frames, presented about the target delay after they
/// arrive, carrying the tone that was captured.
fn heard(r: &Report, s: &Sound) -> Vec<Check> {
    let a = &r.audio;
    let seconds: f64 = a
        .streams
        .iter()
        .map(|st| st.frames as f64 / f64::from(st.sample_rate.max(1)))
        .sum();
    let spacing = &a.spacing_error_ms;
    let worst_spacing = spacing.min.abs().max(spacing.max.abs());
    let lead = &a.lead_ms;
    let lead_floor = s.target_delay_ms - AUDIO_SEND_DELAY_MS;
    let off_tone: Vec<String> = a
        .streams
        .iter()
        .filter(|st| st.frames >= u64::from(st.sample_rate))
        .filter(|st| {
            (st.frequency_hz / s.tone_hz - 1.0).abs() > 0.01
                || (st.rms / s.tone_rms - 1.0).abs() > 0.1
        })
        .map(|st| {
            format!(
                "stream {}: {:.2} Hz, RMS {:.4}",
                st.stream, st.frequency_hz, st.rms
            )
        })
        .collect();
    let measured = a
        .streams
        .iter()
        .filter(|st| st.frames >= u64::from(st.sample_rate))
        .count();
    vec![
        Check::new(
            a.packets > 0,
            format!(
                "the app's sound arrives: {} packets, {seconds:.2} s in {} stream(s)",
                a.packets,
                a.streams.len()
            ),
            "no Audio packets arrived",
        ),
        Check::new(
            a.packets > 0 && a.lost_packets == 0 && a.missing_frames == 0,
            "audio sequence numbers and frames run on without gaps",
            format!(
                "{} packet(s) lost, {} frame(s) missing",
                a.lost_packets, a.missing_frames
            ),
        ),
        Check::new(
            spacing.count > 0 && spacing.p50.abs() < 0.01 && worst_spacing < AUDIO_SPACING_MS,
            format!(
                "audio presentation times are spaced by frames / rate: p50 {:+.4} ms, worst {worst_spacing:.3} ms",
                spacing.p50
            ),
            format!("{spacing:?}"),
        ),
        Check::new(
            lead.count > 0
                && a.late_packets == 0
                && lead.p50 >= lead_floor
                && lead.max <= s.target_delay_ms + 2.0,
            format!(
                "audio is due {:.1} ms (p50) after it arrives: the {:.0} ms target, less up to \
                 {AUDIO_SEND_DELAY_MS:.0} ms from capture to arrival",
                lead.p50, s.target_delay_ms
            ),
            format!("{} late packet(s); lead {lead:?}", a.late_packets),
        ),
        Check::new(
            measured > 0 && off_tone.is_empty(),
            format!(
                "the tone arrives intact: {:.0} Hz at RMS {:.3} ({measured} stream(s) measured)",
                s.tone_hz, s.tone_rms
            ),
            off_tone.join("\n"),
        ),
    ]
}

fn link_held(r: &Report) -> Check {
    let events: Vec<String> = r
        .link
        .events
        .iter()
        .map(|e| format!("{:.3} s: {}", e.at_s, e.event))
        .collect();
    Check::new(
        r.link.connects == 1 && r.link.drops == 0,
        "the headset connects once and keeps its link",
        events.join("\n"),
    )
}

/// Both eyes side by side: the eyes the runtime asks the app to render for the profile, at the
/// size the encoder sustains at the profile's refresh rate.
fn expected_video_size(p: &Profile) -> (u32, u32) {
    let scale = video::render_scale(std::env::var_os(video::RENDER_SCALE_VAR).as_deref());
    let app = video::render_eye((p.eye_width, p.eye_height), scale);
    let (w, h) = video::encode_eye(app, p.refresh_rate);
    (w * 2, h)
}

fn stream_started(r: &Report) -> Check {
    let want = expected_video_size(&r.profile);
    let name = format!(
        "a {}x{} stream starts within {STREAM_START_S:.0} s",
        want.0, want.1
    );
    match r
        .server_hellos
        .iter()
        .find(|h| h.video_width > 0 && h.video_height > 0)
    {
        None => Check::fail(name, "no ServerHello announced a stream"),
        Some(h) => Check::new(
            h.at_s <= STREAM_START_S && (h.video_width, h.video_height) == want,
            name,
            format!("{}x{} at {:.2} s", h.video_width, h.video_height, h.at_s),
        ),
    }
}

fn keyframe_first(r: &Report) -> Check {
    let not_keyframes: Vec<String> = r
        .video
        .streams
        .iter()
        .filter(|s| s.began_with_keyframe == Some(false))
        .map(|s| {
            format!(
                "the {}x{} stream from {:.2} s began with frame {:?}",
                s.video_width, s.video_height, s.began_at_s, s.first_index
            )
        })
        .collect();
    Check::new(
        r.video.frames > 0 && not_keyframes.is_empty(),
        "every stream starts with a keyframe",
        if r.video.frames == 0 {
            "no frames arrived".into()
        } else {
            not_keyframes.join("\n")
        },
    )
}

fn refresh_rate(r: &Report) -> Check {
    let refresh = f64::from(r.profile.refresh_rate);
    let streaming_s: f64 = r
        .video
        .streams
        .iter()
        .map(|s| s.ended_at_s.unwrap_or(r.duration_s) - s.began_at_s)
        .sum();
    let intervals = &r.video.arrival_interval_ms;
    let rate = if intervals.count > 0 && intervals.mean > 0.0 {
        1000.0 / intervals.mean
    } else {
        0.0
    };
    let expected = refresh * streaming_s;
    let frames = r.video.frames as f64;
    Check::new(
        (rate - refresh).abs() <= RATE_TOLERANCE * refresh
            && frames >= (1.0 - RATE_TOLERANCE) * expected,
        format!(
            "video arrives at the headset's {refresh} Hz: {rate:.1} Hz, {frames} frames in {streaming_s:.2} s"
        ),
        format!(
            "{expected:.0} frames expected; arrival intervals p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms, \
             {} over twice the period",
            intervals.p50, intervals.p99, intervals.max, r.video.arrival_intervals_over_2x_period
        ),
    )
}

fn no_gaps(r: &Report) -> Check {
    Check::new(
        r.video.index_gaps == 0,
        "frame indices have no gaps",
        format!(
            "{} frame(s) missing in {} gap(s)",
            r.video.missing_frames, r.video.index_gaps
        ),
    )
}

fn keyframe_requests_answered(r: &Report) -> Check {
    let due: Vec<&KeyframeRequest> = r
        .keyframe_requests
        .iter()
        .filter(|k| k.at_s <= r.duration_s - REQUEST_GRACE_S)
        .collect();
    let unanswered: Vec<String> = due
        .iter()
        .filter(|k| k.answered_after_frames.is_none())
        .map(|k| format!("the request at {:.2} s got no keyframe", k.at_s))
        .collect();
    Check::new(
        unanswered.is_empty(),
        format!("keyframe requests are answered ({})", due.len()),
        unanswered.join("\n"),
    )
}

fn clock_mapped(r: &Report) -> Check {
    Check::new(
        r.clock_pings.answered > 0,
        format!(
            "the Mac maps the headset's clock ({} pings)",
            r.clock_pings.answered
        ),
        "the Mac sent no ClockPing",
    )
}

fn no_errors(r: &Report) -> Check {
    let mut detail = r.errors.join("\n");
    let unlisted = r.error_count.saturating_sub(r.errors.len() as u64);
    if unlisted > 0 {
        detail.push_str(&format!("\nand {unlisted} more"));
    }
    Check::new(
        r.error_count == 0,
        format!(
            "the headset finds no errors in what the Mac sent ({} warning(s))",
            r.warning_count
        ),
        detail,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten seconds of 90 Hz video in one stream, as a healthy run reports it.
    fn healthy() -> serde_json::Value {
        let mut r = serde_json::json!({
            "passed": true,
            "duration_s": 10.03,
            "profile": { "name": "Fake Quest 3", "eye_width": 1680, "eye_height": 1760, "refresh_rate": 90.0 },
            "link": { "connects": 1, "drops": 0, "events": [{ "at_s": 0.001, "event": "connected" }] },
            "server_hellos": [{ "at_s": 0.12, "version": 2, "video_width": 3360, "video_height": 1760 }],
            "video": {
                "frames": 892, "keyframes": 2, "index_gaps": 0, "missing_frames": 0,
                "arrival_interval_ms": { "count": 891, "mean": 11.12, "p50": 11.1, "p99": 13.0, "max": 16.0 },
                "arrival_intervals_over_2x_period": 0,
                "streams": [{
                    "video_width": 3360, "video_height": 1760, "began_at_s": 0.12, "ended_at_s": null,
                    "first_index": 0, "began_with_keyframe": true
                }]
            },
            "keyframe_requests": [{ "at_s": 0.12, "reason": "stream_start", "answered_after_frames": 1 }],
            "clock_pings": { "answered": 40 },
            "error_count": 0, "errors": [], "warning_count": 0, "warnings": []
        });
        let profile: Profile = serde_json::from_value(r["profile"].clone()).unwrap();
        let (w, h) = expected_video_size(&profile);
        for path in ["/server_hellos/0", "/video/streams/0"] {
            let size = r.pointer_mut(path).unwrap();
            size["video_width"] = w.into();
            size["video_height"] = h.into();
        }
        r
    }

    fn failures(report: serde_json::Value) -> Vec<String> {
        let report: Report = serde_json::from_value(report).unwrap();
        judge(&report)
            .into_iter()
            .filter(|c| !c.ok)
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn a_healthy_run_passes() {
        assert_eq!(failures(healthy()), Vec::<String>::new());
    }

    #[test]
    fn a_stream_of_another_size_fails() {
        let mut r = healthy();
        r["server_hellos"][0]["video_width"] = 2352.into();
        r["server_hellos"][0]["video_height"] = 2464.into();
        let failed = failures(r);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("stream starts within"), "{failed:?}");
    }

    #[test]
    fn a_stream_opening_on_a_delta_frame_fails() {
        let mut r = healthy();
        r["video"]["streams"][0]["began_with_keyframe"] = false.into();
        assert_eq!(failures(r), ["every stream starts with a keyframe"]);
    }

    #[test]
    fn half_rate_video_fails() {
        let mut r = healthy();
        r["video"]["frames"] = 446.into();
        r["video"]["arrival_interval_ms"]["mean"] = 22.2.into();
        let failed = failures(r);
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0].starts_with("video arrives at the headset's 90 Hz"),
            "{failed:?}"
        );
    }

    #[test]
    fn a_stream_that_stalls_after_starting_fails_on_its_frame_count() {
        let mut r = healthy();
        r["video"]["frames"] = 300.into();
        r["video"]["arrival_interval_ms"]["count"] = 299.into();
        let failed = failures(r);
        assert!(failed[0].starts_with("video arrives"), "{failed:?}");
    }

    #[test]
    fn no_video_fails_everything_that_needs_it() {
        let mut r = healthy();
        r["server_hellos"] = serde_json::json!([]);
        r["video"]["frames"] = 0.into();
        r["video"]["arrival_interval_ms"]["count"] = 0.into();
        r["video"]["arrival_interval_ms"]["mean"] = 0.0.into();
        r["video"]["streams"] = serde_json::json!([]);
        let failed = failures(r);
        assert_eq!(failed.len(), 3, "{failed:?}");
    }

    #[test]
    fn gaps_errors_and_a_dropped_link_fail() {
        let mut r = healthy();
        r["video"]["index_gaps"] = 1.into();
        r["video"]["missing_frames"] = 2.into();
        r["error_count"] = 1.into();
        r["errors"] =
            serde_json::json!(["1.000 s: frame 4 is to be displayed 9000.0 ms after it arrived"]);
        r["link"]["drops"] = 1.into();
        assert_eq!(failures(r).len(), 3);
    }

    #[test]
    fn a_crashed_app_hands_over_only_with_a_stream_end_between_the_streams() {
        let mut r = healthy();
        r["server_hellos"] = serde_json::json!([
            { "at_s": 0.12, "version": 2, "video_width": 3360, "video_height": 1760 },
            { "at_s": 6.2, "version": 2, "video_width": 3360, "video_height": 1760 }
        ]);
        r["stream_ended_at_s"] = serde_json::json!([5.1]);
        let report: Report = serde_json::from_value(r.clone()).unwrap();
        let h = Handover {
            killed_at_s: 5.0,
            limit_s: 2.0,
        };
        assert!(handed_over(&report, &h).ok);
        r["stream_ended_at_s"] = serde_json::json!([]);
        let report: Report = serde_json::from_value(r).unwrap();
        assert!(
            !handed_over(&report, &h).ok,
            "no StreamEnded: the frozen frame stays"
        );
    }

    fn with_sound() -> serde_json::Value {
        let mut r = healthy();
        r["audio"] = serde_json::json!({
            "starts": 1, "stops": 0, "packets": 2000, "frames": 480000,
            "lost_packets": 0, "missing_frames": 0, "late_packets": 0,
            "lead_ms": { "count": 2000, "mean": 27.0, "min": 18.0, "p50": 27.0, "p99": 33.0, "max": 34.5 },
            "spacing_error_ms": { "count": 1999, "mean": 0.0, "min": -0.2, "p50": 0.0, "p99": 0.0, "max": 0.8 },
            "streams": [{ "stream": 0, "sample_rate": 48000, "channels": 2, "started_at_s": 0.1,
                          "ended_at_s": null, "packets": 2000, "frames": 480000,
                          "rms": 0.1768, "frequency_hz": 440.0 }]
        });
        r
    }

    fn unheard(report: serde_json::Value) -> Vec<String> {
        let report: Report = serde_json::from_value(report).unwrap();
        heard(&report, &Sound::synthetic())
            .into_iter()
            .filter(|c| !c.ok)
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn the_synthetic_tone_on_time_and_without_gaps_is_heard() {
        assert_eq!(unheard(with_sound()), Vec::<String>::new());
    }

    #[test]
    fn late_gapped_jittery_or_altered_sound_fails() {
        let mut r = with_sound();
        r["audio"]["lost_packets"] = 1.into();
        r["audio"]["spacing_error_ms"]["max"] = 5.0.into();
        r["audio"]["lead_ms"]["p50"] = 12.0.into();
        r["audio"]["late_packets"] = 3.into();
        r["audio"]["streams"][0]["frequency_hz"] = 220.0.into();
        assert_eq!(unheard(r).len(), 4);
        let mut r = with_sound();
        r["audio"] = serde_json::json!({});
        assert_eq!(unheard(r).len(), 5, "no sound at all");
    }

    #[test]
    fn a_keyframe_request_still_in_flight_at_the_end_is_not_judged() {
        let mut r = healthy();
        r["keyframe_requests"] = serde_json::json!([
            { "at_s": 0.12, "answered_after_frames": 1 },
            { "at_s": 9.9, "answered_after_frames": null }
        ]);
        assert_eq!(failures(r.clone()), Vec::<String>::new());
        r["keyframe_requests"][1]["at_s"] = 5.0.into();
        assert_eq!(failures(r), ["keyframe requests are answered (2)"]);
    }
}
