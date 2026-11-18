//! The two one-shot runs a person makes at the Mac under tetherd's signing identity: asking for
//! the System Audio Recording permission, and checking a live capture against a known tone.
//! Both tap an `afplay` child, never a real app, and capture once, never retrying. Run them only
//! as a launchd job: TCC holds a process started from a shell to the shell's app instead.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::analysis::{ClockFit, first_loud_frame, fit_clock, tone_stats};
use crate::capture::{AudioCapture, Config, Source};
use crate::events::{BlockReason, EndReason, Event, status_text};
use crate::latch;
use crate::packetizer::{Item, Packetizer};
use crate::ring::CHANNELS;
use crate::time::now_ns;

pub struct ProbeOptions {
    /// Where the test sounds, the capture and the report go.
    pub out_dir: PathBuf,
    /// The block marker, shared with tetherd so a block seen here holds there too.
    pub marker: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Permission run: audio arrived. Check run: every measurement passed.
    Passed,
    /// Capture is blocked; see the reason. Nothing more was or will be tried.
    Blocked(BlockReason),
    Failed(Vec<String>),
}

const PERMISSION_TIMEOUT: Duration = Duration::from_secs(150);
/// Longer than the default stall threshold, so a stalled call is caught by the controller.
const CHECK_TIMEOUT: Duration = Duration::from_secs(40);
const TONE_SECONDS: u32 = 3;
const TONE_HZ: f64 = 440.0;
const TONE_AMPLITUDE: f64 = 0.05;

/// Taps a silent `afplay`; registering the IOProc raises the prompt. Answer it with Allow.
pub fn request_permission(options: &ProbeOptions) -> io::Result<Outcome> {
    let log = Log::new();
    // Long enough to outlast the prompt: the HAL waits up to 60 s for an answer.
    let silence = options.out_dir.join("silence.wav");
    write_wav(&silence, 8_000, 1, &vec![0; 8_000 * 90])?;
    let config = Config {
        // A slow answer to the prompt is not a stall; the HAL's own timeout is.
        stall_threshold: Duration::from_secs(45),
        first_callback_timeout: Duration::from_secs(60),
        block_marker: options.marker.clone(),
        ..Config::default()
    };
    let (capture, mut packetizer) = AudioCapture::spawn(Source::Tap, config)?;
    let player = Player::spawn(&silence)?;
    log.say(&format!(
        "tapping a silent afplay (pid {}); answer the System Audio Recording prompt with Allow",
        player.pid()
    ));
    capture.capture(player.pid());

    let deadline = Instant::now() + PERMISSION_TIMEOUT;
    let outcome = loop {
        while packetizer.poll().is_some() {}
        let Some(event) = capture.next_event(Duration::from_millis(100)) else {
            if Instant::now() >= deadline {
                break Outcome::Blocked(unanswered(options, PERMISSION_TIMEOUT));
            }
            continue;
        };
        log.event(&event);
        match event {
            Event::Receiving { .. } => break Outcome::Passed,
            Event::Blocked(reason) | Event::Refused { reason, .. } => {
                break Outcome::Blocked(reason);
            }
            Event::Ended { reason, .. } => {
                break Outcome::Failed(vec![format!("capture ended: {reason:?}")]);
            }
            _ => {}
        }
    };
    finish(&log, capture, matches!(outcome, Outcome::Blocked(_)));
    drop(player);
    Ok(outcome)
}

/// Captures a 3 s, 440 Hz tone that `afplay` plays at -29 dBFS, through the same tap,
/// aggregate, IOProc, ring and packetizer tetherd uses, and measures what arrived.
pub fn live_check(options: &ProbeOptions) -> io::Result<Outcome> {
    let log = Log::new();
    let rate = 48_000u32;
    let tone_path = options.out_dir.join("tone440.wav");
    let tone: Vec<i16> = (0..rate * TONE_SECONDS)
        .flat_map(|n| {
            let v = (TONE_AMPLITUDE
                * (f64::from(n) * TONE_HZ / f64::from(rate) * std::f64::consts::TAU).sin()
                * 32768.0)
                .round() as i16;
            [v, v]
        })
        .collect();
    write_wav(&tone_path, rate, CHANNELS as u16, &tone)?;

    let config = Config {
        block_marker: options.marker.clone(),
        ..Config::default()
    };
    let (capture, mut packetizer) = AudioCapture::spawn(Source::Tap, config)?;
    let mut player = Player::spawn(&tone_path)?;
    log.say(&format!(
        "tapping afplay (pid {}) playing {TONE_SECONDS} s of {TONE_HZ} Hz",
        player.pid()
    ));
    capture.capture(player.pid());

    let mut record = Recording::default();
    let deadline = Instant::now() + CHECK_TIMEOUT;
    let mut problems = Vec::new();
    let mut exit_seen = None;
    let blocked = loop {
        record.take(&mut packetizer);
        if let Some(event) = capture.try_event() {
            log.event(&event);
            match event {
                Event::Receiving { after_ms, .. } => record.receiving_after_ms = Some(after_ms),
                Event::Blocked(reason) | Event::Refused { reason, .. } => break Some(reason),
                Event::Ended {
                    reason: EndReason::ProcessExited,
                    ..
                } => break None,
                Event::Ended { reason, .. } => {
                    problems.push(format!("capture ended early: {reason:?}"));
                    break None;
                }
                Event::Failed { call, status, .. } => {
                    problems.push(format!("{call:?} failed: {}", status_text(status)))
                }
                Event::Overrun { dropped_frames } => {
                    problems.push(format!("ring overran by {dropped_frames} frames"))
                }
                Event::Unreadable { callbacks, .. } => problems.push(format!(
                    "{callbacks} IOProc callbacks had buffers that could not be read"
                )),
                _ => {}
            }
            continue;
        }
        if exit_seen.is_none() && player.exited() {
            exit_seen = Some(Instant::now());
        }
        if exit_seen.is_some_and(|at| at.elapsed() > Duration::from_secs(3)) {
            problems.push("afplay exited but the capture did not notice within 3 s".into());
            break None;
        }
        if Instant::now() >= deadline {
            if record.receiving_after_ms.is_none() {
                break Some(unanswered(options, CHECK_TIMEOUT));
            }
            problems.push("the check did not finish in time".into());
            break None;
        }
        packetizer.wait(Duration::from_millis(10));
    };
    if let Some(reason) = blocked {
        finish(&log, capture, true);
        return Ok(Outcome::Blocked(reason));
    }
    capture.stop();
    let stop_deadline = Instant::now() + Duration::from_secs(5);
    while !record.ended && Instant::now() < stop_deadline {
        record.take(&mut packetizer);
        packetizer.wait(Duration::from_millis(10));
    }
    finish(&log, capture, false);
    drop(player);

    let capture_path = options.out_dir.join("captured.wav");
    write_wav(
        &capture_path,
        record.sample_rate.max(1),
        CHANNELS as u16,
        &record.samples,
    )?;
    log.say(&format!("wrote {}", capture_path.display()));
    problems.extend(record.verdict(&log));
    let report = options.out_dir.join("report.txt");
    fs::write(&report, log.text())?;
    Ok(if problems.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(problems)
    })
}

/// Records a block when the HAL has not answered: the control thread is still inside a call,
/// which is what a pending prompt, or a wedged coreaudiod, looks like.
fn unanswered(options: &ProbeOptions, waited: Duration) -> BlockReason {
    let reason = BlockReason::Recorded(format!(
        "the HAL did not answer within {} s: a prompt may be pending, or coreaudiod is wedged",
        waited.as_secs()
    ));
    if let Err(error) = latch::PROCESS.block(&reason, options.marker.as_deref()) {
        eprintln!("could not record the capture block: {error}");
    }
    reason
}

/// Stops cleanly unless capture is blocked, when no further HAL call may be made: the control
/// thread then does nothing and this process's exit reclaims the private tap objects.
fn finish(log: &Log, capture: AudioCapture, blocked: bool) {
    if !blocked {
        capture.stop();
    }
    let ended = capture.shutdown(Duration::from_secs(10));
    if !ended {
        log.say("the control thread is still inside a HAL call; exiting without waiting for it");
    }
}

#[derive(Default)]
struct Recording {
    streams: Vec<(u16, u32)>,
    ended: bool,
    sample_rate: u32,
    /// `(first_frame, host_time_ns, arrival_ns, emitted_ns, frames)` per packet.
    packets: Vec<(u64, i64, i64, i64, usize)>,
    samples: Vec<i16>,
    gaps: u64,
    receiving_after_ms: Option<u64>,
}

impl Recording {
    fn take(&mut self, packetizer: &mut Packetizer) {
        while let Some(item) = packetizer.poll() {
            match item {
                Item::Start(format) => {
                    self.streams.push((format.stream, format.sample_rate));
                    self.sample_rate = format.sample_rate;
                }
                Item::Packet(packet) => {
                    if let Some(&(first, _, _, _, frames)) = self.packets.last() {
                        self.gaps += packet.first_frame.saturating_sub(first + frames as u64);
                    }
                    self.packets.push((
                        packet.first_frame,
                        packet.host_time_ns,
                        packet.arrival_ns,
                        now_ns(),
                        packet.frames(),
                    ));
                    self.samples.extend_from_slice(packet.samples);
                }
                Item::End { .. } => self.ended = true,
            }
        }
    }

    fn verdict(&self, log: &Log) -> Vec<String> {
        let mut problems = Vec::new();
        let rate = self.sample_rate.max(1);
        let frames = self.samples.len() / CHANNELS;
        log.say(&format!(
            "streams {:?}, {} packets, {frames} frames ({:.2} s), gaps {} frames, first callback {:?} ms after start",
            self.streams,
            self.packets.len(),
            frames as f64 / f64::from(rate),
            self.gaps,
            self.receiving_after_ms
        ));
        if self.streams.len() != 1 {
            problems.push(format!("expected one stream, got {:?}", self.streams));
        }
        if !self.ended {
            problems.push("the stream never ended".into());
        }
        if self.gaps > 0 {
            problems.push(format!("{} frames missing between packets", self.gaps));
        }

        let points: Vec<(u64, i64)> = self.packets.iter().map(|p| (p.0, p.1)).collect();
        match fit_clock(&points) {
            Some(ClockFit {
                rate_hz,
                max_residual_ns,
            }) => {
                log.say(&format!("capture clock: {rate_hz:.2} Hz on the Mac clock, worst packet {:.3} ms off the line", max_residual_ns / 1e6));
                if (rate_hz / f64::from(rate) - 1.0).abs() > 0.002 {
                    problems.push(format!("capture rate {rate_hz:.1} Hz is not {rate} Hz"));
                }
                if max_residual_ns > 2e6 {
                    problems.push(format!(
                        "packet times stray {:.2} ms from a straight line",
                        max_residual_ns / 1e6
                    ));
                }
            }
            None => problems.push("too few packets to measure the clock".into()),
        }
        let mut delays: Vec<i64> = self.packets.iter().map(|p| p.2 - p.1).collect();
        let mut handoffs: Vec<i64> = self.packets.iter().map(|p| p.3 - p.1).collect();
        delays.sort_unstable();
        handoffs.sort_unstable();
        if let (Some(&p50), Some(&max), Some(&h50)) = (
            delays.get(delays.len() / 2),
            delays.last(),
            handoffs.get(handoffs.len() / 2),
        ) {
            log.say(&format!(
                "capture to IOProc: median {:.2} ms, max {:.2} ms; capture to packet: median {:.2} ms",
                p50 as f64 / 1e6,
                max as f64 / 1e6,
                h50 as f64 / 1e6
            ));
        }

        let expected_rms = TONE_AMPLITUDE / 2f64.sqrt();
        match first_loud_frame(&self.samples, TONE_AMPLITUDE / 5.0) {
            None => problems.push("the tone never arrived (all silent)".into()),
            Some(onset) => {
                let start = (onset + rate as usize / 10) * CHANNELS;
                let end = (start + rate as usize * CHANNELS).min(self.samples.len());
                let stats = tone_stats(&self.samples[start.min(end)..end], 0, rate);
                log.say(&format!(
                    "tone from {:.3} s: {} frames, {:.2} Hz, RMS {:.5} (expected {expected_rms:.5}), peak {:.4}",
                    onset as f64 / f64::from(rate),
                    stats.frames,
                    stats.frequency_hz,
                    stats.rms,
                    stats.peak
                ));
                if stats.frames < rate as usize / 2 {
                    problems.push(format!("only {} frames of tone", stats.frames));
                }
                if (stats.frequency_hz / TONE_HZ - 1.0).abs() > 0.01 {
                    problems.push(format!(
                        "tone at {:.2} Hz, not {TONE_HZ} Hz",
                        stats.frequency_hz
                    ));
                }
                if (stats.rms / expected_rms - 1.0).abs() > 0.1 {
                    problems.push(format!(
                        "tone RMS {:.5}, expected {expected_rms:.5}",
                        stats.rms
                    ));
                }
            }
        }
        problems
    }
}

/// An `afplay` child, killed when dropped.
struct Player(Child);

impl Player {
    fn spawn(path: &Path) -> io::Result<Player> {
        Command::new("/usr/bin/afplay")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(Player)
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    fn exited(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Timestamped lines on stdout, kept for the report.
struct Log {
    start: Instant,
    lines: std::cell::RefCell<Vec<String>>,
}

impl Log {
    fn new() -> Log {
        Log {
            start: Instant::now(),
            lines: Default::default(),
        }
    }

    fn say(&self, text: &str) {
        let line = format!("[{:8.3}] {text}", self.start.elapsed().as_secs_f64());
        println!("{line}");
        self.lines.borrow_mut().push(line);
    }

    fn event(&self, event: &Event) {
        self.say(&match event {
            Event::Failed { pid, call, status } => format!(
                "Failed {{ pid: {pid}, call: {call:?}, status: {} }}",
                status_text(*status)
            ),
            Event::Blocked(reason) => format!("Blocked: {reason}"),
            other => format!("{other:?}"),
        });
    }

    fn text(&self) -> String {
        self.lines.borrow().join("\n") + "\n"
    }
}

/// 16-bit PCM WAV.
pub fn write_wav(path: &Path, sample_rate: u32, channels: u16, samples: &[i16]) -> io::Result<()> {
    let data_len = u32::try_from(samples.len() * 2)
        .map_err(|_| io::Error::other("too long for a WAV file"))?;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::Tone;

    #[test]
    fn writes_a_canonical_wav_header() {
        let path = std::env::temp_dir().join(format!("audiotap-wav-{}.wav", std::process::id()));
        write_wav(&path, 48_000, 2, &[1, -1, 2, -2]).unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 8);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 44);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            192_000
        );
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), -1);
    }

    /// The live check's verdict on a synthetic capture of the same tone: a check that fails
    /// on the Mac then points at the capture, not at the measurement.
    #[test]
    fn the_verdict_passes_a_faithful_capture() {
        let tone = Tone {
            amplitude: TONE_AMPLITUDE as f32,
            frequency_hz: TONE_HZ,
            ..Tone::default()
        };
        let (capture, mut packetizer) =
            AudioCapture::spawn(Source::Synthetic(tone), Config::default()).unwrap();
        capture.capture(1);
        let mut record = Recording::default();
        let until = Instant::now() + Duration::from_millis(1_400);
        while Instant::now() < until {
            record.take(&mut packetizer);
            packetizer.wait(Duration::from_millis(10));
        }
        capture.stop();
        let until = Instant::now() + Duration::from_secs(2);
        while !record.ended && Instant::now() < until {
            record.take(&mut packetizer);
            packetizer.wait(Duration::from_millis(10));
        }
        assert!(capture.shutdown(Duration::from_secs(1)));
        let problems = record.verdict(&Log::new());
        assert!(problems.is_empty(), "{problems:?}");

        let mut damaged = Recording {
            streams: vec![(0, 48_000), (1, 48_000)],
            ..record
        };
        damaged.samples.iter_mut().for_each(|s| *s /= 2);
        let problems = damaged.verdict(&Log::new());
        assert_eq!(problems.len(), 2, "{problems:?}");
    }
}
