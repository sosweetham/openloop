//! One JSON report per app session, written when the session goes, to
//! `~/Library/Logs/Tether/sessions/` (docs/ROADMAP.md § Quality gates).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use std::sync::atomic::Ordering;

use tether_ipc::records::AccessUnit;
use tether_stream::PIPELINE_LATENCY_NS;

use crate::audio::Tally;
use crate::log::{file_stamp, unix_time};
use crate::session::Session;
use crate::stage::Forwarded;

pub struct SessionReport {
    session: u64,
    app: String,
    engine: String,
    pid: i32,
    started_unix_s: f64,
    started: Instant,
    duration_s: f64,
    frames_sent: u64,
    keyframes_sent: u64,
    bytes_sent: u64,
    last_size: Option<(u32, u32)>,
    sizes: Vec<(u32, u32)>,
    awaiting_keyframe: u64,
    stale: u64,
    no_headset: u64,
    background: u64,
    not_running: u64,
    keyframes_requested: u64,
    haptics: u64,
    audio_pid: Option<i32>,
    audio: serde_json::Value,
    corrupt: bool,
}

impl SessionReport {
    pub fn new(session: &Session) -> SessionReport {
        SessionReport {
            session: session.id,
            app: session.app.clone(),
            engine: session.engine.clone(),
            pid: session.owner.pid,
            started_unix_s: unix_time(),
            started: Instant::now(),
            duration_s: 0.0,
            frames_sent: 0,
            keyframes_sent: 0,
            bytes_sent: 0,
            last_size: None,
            sizes: Vec::new(),
            awaiting_keyframe: 0,
            stale: 0,
            no_headset: 0,
            background: 0,
            not_running: 0,
            keyframes_requested: 0,
            haptics: 0,
            audio_pid: session.audio_pid,
            audio: serde_json::Value::Null,
            corrupt: false,
        }
    }

    pub fn access_unit(&mut self, unit: &AccessUnit, outcome: Forwarded) {
        match outcome {
            Forwarded::Sent => {
                self.frames_sent += 1;
                self.keyframes_sent += u64::from(unit.keyframe());
                self.bytes_sent += u64::from(unit.len);
                let size = (unit.width, unit.height);
                if self.last_size.replace(size) != Some(size) {
                    self.sizes.push(size);
                }
            }
            Forwarded::AwaitingKeyframe => self.awaiting_keyframe += 1,
            Forwarded::Stale => self.stale += 1,
            Forwarded::NoHeadset => self.no_headset += 1,
            Forwarded::Background => self.background += 1,
            Forwarded::NotRunning => self.not_running += 1,
        }
    }

    pub fn keyframe_requested(&mut self) {
        self.keyframes_requested += 1;
    }

    pub fn haptic(&mut self) {
        self.haptics += 1;
    }

    pub fn finish(&mut self, corrupt: bool, audio: &Tally) {
        self.duration_s = self.started.elapsed().as_secs_f64();
        self.corrupt = corrupt;
        let count = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
        self.audio = serde_json::json!({
            "pid": self.audio_pid,
            "streams": count(&audio.streams),
            "packets_sent": count(&audio.packets),
            "frames_sent": count(&audio.frames),
            "dropped": {
                "no_headset": count(&audio.no_headset),
                "background": count(&audio.background),
            },
            "presented_after_capture_ms": PIPELINE_LATENCY_NS as f64 / 1e6,
            "events": audio.notes(),
        });
    }

    pub fn to_json(&self) -> serde_json::Value {
        let sizes: Vec<String> = self.sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
        serde_json::json!({
            "session": self.session,
            "app": self.app,
            "engine": self.engine,
            "pid": self.pid,
            "started_unix_s": self.started_unix_s,
            "duration_s": self.duration_s,
            "video": {
                "frames_sent": self.frames_sent,
                "keyframes_sent": self.keyframes_sent,
                "bytes_sent": self.bytes_sent,
                "sizes": sizes,
                "mean_rate_hz": if self.duration_s > 0.0 { self.frames_sent as f64 / self.duration_s } else { 0.0 },
                "dropped": {
                    "awaiting_keyframe": self.awaiting_keyframe,
                    "stale": self.stale,
                    "no_headset": self.no_headset,
                    "background": self.background,
                    "not_running": self.not_running,
                },
                "keyframes_requested_from_app": self.keyframes_requested,
            },
            "haptics_sent": self.haptics,
            "audio": self.audio,
            "rings_corrupt": self.corrupt,
        })
    }

    /// `<dir>/<time>-<app>-<session>.json`.
    pub fn write(&self, dir: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let app: String = self
            .app
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        let path = dir.join(format!("{}-{app}-{}.json", file_stamp(), self.session));
        let json = serde_json::to_vec_pretty(&self.to_json()).map_err(io::Error::other)?;
        fs::write(&path, json)?;
        Ok(path)
    }
}
