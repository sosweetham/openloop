//! A stand-in for the tap: a generated tone written to the capture ring in chunks stamped with
//! the Mac clock, the way a device's IOProc delivers them, so tetherd and the end-to-end tests
//! run the same packet path without Core Audio or the capture permission.

use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::control::Command;
use crate::events::{EndReason, Event};
use crate::ring::{CHANNELS, CaptureRing, Chunk};
use crate::time::{frames_to_ns, now_ns};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    pub frequency_hz: f64,
    pub amplitude: f32,
    pub sample_rate: u32,
    /// Frames per chunk, like a device's IO buffer.
    pub chunk_frames: u32,
    /// Sound only during the first `on_ns` of every `period_ns` of `XrTime`, so a receiver
    /// knows when each beep was captured.
    pub beep: Option<Beep>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Beep {
    pub period_ns: i64,
    pub on_ns: i64,
}

impl Default for Tone {
    fn default() -> Tone {
        Tone {
            frequency_hz: 440.0,
            amplitude: 0.25,
            sample_rate: 48_000,
            chunk_frames: 512,
            beep: None,
        }
    }
}

/// Frames of a tone that started at `start_ns`.
pub(crate) struct Generator {
    tone: Tone,
    start_ns: i64,
}

impl Generator {
    pub fn new(tone: Tone, start_ns: i64) -> Generator {
        Generator {
            tone: Tone {
                sample_rate: tone.sample_rate.max(1),
                ..tone
            },
            start_ns,
        }
    }

    pub fn host_ns(&self, frame: u64) -> i64 {
        self.start_ns + frames_to_ns(frame, self.tone.sample_rate)
    }

    /// Writes frames `first..` into `dst`, the same sample on both channels.
    pub fn fill(&self, first: u64, dst: &mut [f32]) {
        let rate = f64::from(self.tone.sample_rate);
        for (i, frame) in dst.chunks_exact_mut(CHANNELS).enumerate() {
            let n = first + i as u64;
            let cycles = (n as f64 * self.tone.frequency_hz / rate).fract();
            let gate = match self.tone.beep {
                Some(beep) if beep.period_ns > 0 => {
                    self.host_ns(n).rem_euclid(beep.period_ns) < beep.on_ns
                }
                _ => true,
            };
            let sample = if gate {
                self.tone.amplitude * (cycles * TAU).sin() as f32
            } else {
                0.0
            };
            frame.fill(sample);
        }
    }
}

/// The synthetic source's thread: answers the same commands as the tap's control thread, with
/// the same events, and is the ring's producer while it captures.
pub(crate) fn run(
    tone: Tone,
    ring: Arc<CaptureRing>,
    events: Sender<Event>,
    commands: Receiver<Command>,
    first_epoch: u32,
) {
    let chunk = u64::from(tone.chunk_frames.max(1));
    let mut epoch = first_epoch.max(1);
    let mut capture: Option<(i32, Generator, u64, bool)> = None;
    let emit = |event: Event| {
        let _ = events.send(event);
    };
    let end = |capture: &mut Option<(i32, Generator, u64, bool)>, epoch: u32, reason: EndReason| {
        if let Some((pid, ..)) = capture.take() {
            ring.close(epoch);
            emit(Event::Ended { pid, reason });
        }
    };
    loop {
        let timeout = match &capture {
            Some((_, generator, produced, _)) => {
                let due = generator.host_ns(produced + chunk);
                Duration::from_nanos(u64::try_from(due - now_ns()).unwrap_or(0))
            }
            None => Duration::from_secs(1),
        };
        match commands.recv_timeout(timeout) {
            Ok(Command::Capture(pid)) => {
                if capture.is_some() {
                    end(&mut capture, epoch, EndReason::Replaced);
                    epoch = epoch.checked_add(1).unwrap_or(1);
                }
                capture = Some((pid, Generator::new(tone, now_ns()), 0, false));
                emit(Event::Started {
                    pid,
                    sample_rate: tone.sample_rate,
                });
            }
            Ok(Command::Stop) => {
                if capture.is_some() {
                    end(&mut capture, epoch, EndReason::Stopped);
                    epoch = epoch.checked_add(1).unwrap_or(1);
                }
            }
            Ok(Command::Changed(_)) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                end(&mut capture, epoch, EndReason::Stopped);
                return;
            }
        }
        if let Some((pid, generator, produced, receiving)) = &mut capture {
            let now = now_ns();
            // Like a device that overran: a stall of over a second skips ahead, which the
            // packetizer sees as a jump in sample time.
            let behind = u64::try_from(now - generator.host_ns(*produced)).unwrap_or(0);
            if behind > 1_000_000_000 {
                *produced += behind * u64::from(tone.sample_rate) / 1_000_000_000 / chunk * chunk;
            }
            while generator.host_ns(*produced + chunk) <= now {
                let first = *produced;
                let stamp = Chunk {
                    epoch,
                    sample_rate: tone.sample_rate,
                    host_ns: Some(generator.host_ns(first)),
                    sample_time: Some(first as f64),
                    arrival_ns: now,
                };
                // SAFETY: this thread is the ring's only producer while it captures; the tap's
                // control thread does not run alongside a synthetic source.
                unsafe {
                    ring.push(stamp, chunk as usize, |dst, offset| {
                        generator.fill(first + offset as u64, dst)
                    })
                };
                *produced += chunk;
                if !*receiving {
                    *receiving = true;
                    emit(Event::Receiving {
                        pid: *pid,
                        after_ms: 0,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::tone_stats;
    use crate::convert::f32_to_s16;

    fn samples(generator: &Generator, first: u64, frames: usize) -> Vec<i16> {
        let mut float = vec![0.0f32; frames * CHANNELS];
        generator.fill(first, &mut float);
        let mut out = vec![0i16; float.len()];
        f32_to_s16(&float, &mut out);
        out
    }

    #[test]
    fn the_tone_has_its_frequency_and_level() {
        let generator = Generator::new(
            Tone {
                amplitude: 0.5,
                ..Tone::default()
            },
            0,
        );
        let stats = tone_stats(&samples(&generator, 0, 48_000), 0, 48_000);
        assert!((stats.frequency_hz - 440.0).abs() < 0.5, "{stats:?}");
        assert!((stats.rms - 0.5 / 2f64.sqrt()).abs() < 0.001, "{stats:?}");
    }

    #[test]
    fn chunks_continue_the_waveform_exactly() {
        let generator = Generator::new(Tone::default(), 0);
        let whole = samples(&generator, 0, 1024);
        let mut pieces = samples(&generator, 0, 300);
        pieces.extend(samples(&generator, 300, 724));
        assert_eq!(whole, pieces);
    }

    #[test]
    fn beeps_sit_on_whole_periods_of_xrtime() {
        let beep = Beep {
            period_ns: 100_000_000,
            on_ns: 20_000_000,
        };
        // Starts 30 ms into a period, so the first beep begins 70 ms (3360 frames) in.
        let generator = Generator::new(
            Tone {
                beep: Some(beep),
                ..Tone::default()
            },
            1_030_000_000,
        );
        let s = samples(&generator, 0, 9_600);
        let loud = |frame: usize| {
            s[frame * CHANNELS..(frame + 48) * CHANNELS]
                .iter()
                .any(|v| v.abs() > 1000)
        };
        assert!(!loud(0) && !loud(3_300));
        assert!(loud(3_360) && loud(3_360 + 900));
        assert!(!loud(3_360 + 960 + 10));
        assert!(loud(3_360 + 4_800));
        assert_eq!(generator.host_ns(3_360), 1_100_000_000);
    }
}
