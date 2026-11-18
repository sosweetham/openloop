//! The handle tetherd holds: one source, the Core Audio tap or a synthetic tone, running on its
//! own thread, told which process to capture, reporting [`Event`]s, and feeding the
//! [`Packetizer`] returned with it.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::control::{Command, Controller, Settings};
use crate::coreaudio::CoreAudio;
use crate::events::Event;
use crate::latch;
use crate::packetizer::Packetizer;
use crate::ring::{CaptureRing, CaptureStats};
use crate::synthetic::{self, Tone};

pub enum Source {
    /// A Core Audio process tap. Needs the System Audio Recording permission for this binary.
    Tap,
    Synthetic(Tone),
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Frames per packet at the source's rate; 240 is 5 ms at 48 kHz.
    pub frames_per_packet: usize,
    /// Capture ring capacity, rounded up to a power of two.
    pub ring_frames: usize,
    pub first_stream: u16,
    /// How long started IO may go without a callback before capture is blocked.
    pub first_callback_timeout: Duration,
    /// How long one HAL call may take before capture is blocked. The HAL's own RPC timeouts
    /// observed while a prompt was pending were 30 and 60 s.
    pub stall_threshold: Duration,
    /// A file that records a block across restarts; while it exists capture is refused.
    pub block_marker: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            frames_per_packet: 240,
            ring_frames: 1 << 15,
            first_stream: 0,
            first_callback_timeout: Duration::from_secs(5),
            stall_threshold: Duration::from_secs(20),
            block_marker: None,
        }
    }
}

/// One tap control thread per process, so every HAL call is serialized on it.
static TAP_THREAD: AtomicBool = AtomicBool::new(false);

pub struct AudioCapture {
    commands: Sender<Command>,
    events: Receiver<Event>,
    ring: Arc<CaptureRing>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn spawn(source: Source, config: Config) -> io::Result<(AudioCapture, Packetizer)> {
        let ring = Arc::new(CaptureRing::new(config.ring_frames));
        let packetizer =
            Packetizer::new(ring.clone(), config.frames_per_packet, config.first_stream);
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let thread_ring = ring.clone();
        let thread = match source {
            Source::Tap => spawn_tap(thread_ring, event_tx, commands.clone(), command_rx, config)?,
            Source::Synthetic(tone) => thread::Builder::new()
                .name("tether-audio-synthetic".into())
                .spawn(move || synthetic::run(tone, thread_ring, event_tx, command_rx, 1))?,
        };
        Ok((
            AudioCapture {
                commands,
                events,
                ring,
                thread: Some(thread),
            },
            packetizer,
        ))
    }

    /// Captures `pid` from now on, ending any current capture first. A process that has not
    /// used audio yet is waited for.
    pub fn capture(&self, pid: i32) {
        let _ = self.commands.send(Command::Capture(pid));
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    pub fn try_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    pub fn stats(&self) -> CaptureStats {
        self.ring.stats()
    }

    /// Ends any capture and the source's thread, waiting up to `timeout`. A thread still inside
    /// a stalled HAL call is left to finish by itself. Returns whether the thread ended.
    pub fn shutdown(mut self, timeout: Duration) -> bool {
        self.finish(timeout)
    }

    fn finish(&mut self, timeout: Duration) -> bool {
        let _ = self.commands.send(Command::Shutdown);
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = thread.join();
        true
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.finish(Duration::from_secs(2));
    }
}

fn spawn_tap(
    ring: Arc<CaptureRing>,
    events: Sender<Event>,
    notify: Sender<Command>,
    commands: Receiver<Command>,
    config: Config,
) -> io::Result<JoinHandle<()>> {
    // A tap operation issued while a capture prompt is pending wedges coreaudiod; unit tests
    // use the fake HAL and the synthetic source only.
    if cfg!(test) {
        return Err(io::Error::other(
            "unit tests never touch the Core Audio HAL",
        ));
    }
    // TCC holds a launchd job responsible for itself, and any other process for the app that
    // started it (a terminal, an IDE), which would get the prompt, and the grant, instead.
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a tap is made only by a launchd job; use Source::Synthetic elsewhere",
        ));
    }
    if TAP_THREAD.swap(true, Ordering::AcqRel) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a tap control thread is already running in this process",
        ));
    }
    let settings = Settings {
        first_callback_timeout: config.first_callback_timeout,
        stall_threshold: config.stall_threshold,
        slow_call: Duration::from_millis(500),
        marker: config.block_marker,
    };
    let spawned = thread::Builder::new()
        .name("tether-audio-control".into())
        .spawn(move || {
            struct Release;
            impl Drop for Release {
                fn drop(&mut self) {
                    TAP_THREAD.store(false, Ordering::Release);
                }
            }
            let _release = Release;
            Controller::new(
                CoreAudio::new(notify),
                ring,
                events,
                &latch::PROCESS,
                settings,
                1,
            )
            .run(commands);
        });
    if spawned.is_err() {
        TAP_THREAD.store(false, Ordering::Release);
    }
    spawned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EndReason;
    use crate::packetizer::Item;
    use crate::packetizer::tests::{Owned, drain};
    use crate::time::now_ns;

    #[test]
    fn the_synthetic_source_streams_in_real_time_through_the_packetizer() {
        let tone = Tone {
            chunk_frames: 480,
            ..Tone::default()
        };
        let (capture, mut packetizer) = AudioCapture::spawn(
            Source::Synthetic(tone),
            Config {
                first_stream: 9,
                ..Config::default()
            },
        )
        .unwrap();
        assert!(
            capture.next_event(Duration::from_millis(100)).is_none(),
            "idle until told"
        );

        let started = now_ns();
        capture.capture(1234);
        assert_eq!(
            capture.next_event(Duration::from_secs(1)),
            Some(Event::Started {
                pid: 1234,
                sample_rate: 48_000
            })
        );

        let mut starts = Vec::new();
        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            while let Some(item) = packetizer.poll() {
                match item {
                    Item::Start(format) => starts.push(format),
                    Item::Packet(p) => {
                        assert!(
                            p.host_time_ns >= started - 1_000_000 && p.host_time_ns <= now_ns()
                        );
                        assert!(p.arrival_ns >= p.host_time_ns);
                        frames.push((p.first_frame, p.host_time_ns, p.frames()));
                    }
                    Item::End { .. } => panic!("not stopped yet"),
                }
            }
            packetizer.wait(Duration::from_millis(20));
        }
        assert_eq!(
            capture.next_event(Duration::from_secs(1)),
            Some(Event::Receiving {
                pid: 1234,
                after_ms: 0
            })
        );
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].stream, 9);

        let produced = frames.iter().map(|f| f.2).sum::<usize>();
        assert!(
            (9_600..=24_000).contains(&produced),
            "up to 400 ms of audio: {produced}"
        );
        for pair in frames.windows(2) {
            assert_eq!(pair[1].0, pair[0].0 + pair[0].2 as u64, "contiguous");
        }
        let fit =
            crate::analysis::fit_clock(&frames.iter().map(|f| (f.0, f.1)).collect::<Vec<_>>())
                .unwrap();
        assert!((fit.rate_hz - 48_000.0).abs() < 1.0, "{fit:?}");

        capture.stop();
        assert_eq!(
            capture.next_event(Duration::from_secs(1)),
            Some(Event::Ended {
                pid: 1234,
                reason: EndReason::Stopped
            })
        );
        let mut ended = false;
        let deadline = Instant::now() + Duration::from_secs(1);
        while !ended && Instant::now() < deadline {
            while let Some(item) = packetizer.poll() {
                ended |= item == Item::End { stream: 9 };
            }
            packetizer.wait(Duration::from_millis(20));
        }
        assert!(ended);
        assert!(capture.shutdown(Duration::from_secs(1)));
    }

    fn wait_for(capture: &AudioCapture, wanted: &Event, seen: &mut Vec<Event>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(event) = capture.next_event(Duration::from_millis(50)) {
                let done = event == *wanted;
                seen.push(event);
                if done {
                    return;
                }
            }
        }
        panic!("no {wanted:?} in {seen:?}");
    }

    #[test]
    fn a_second_capture_replaces_the_first_with_a_new_stream() {
        let (capture, mut packetizer) =
            AudioCapture::spawn(Source::Synthetic(Tone::default()), Config::default()).unwrap();
        let mut events = Vec::new();
        capture.capture(1);
        wait_for(
            &capture,
            &Event::Receiving {
                pid: 1,
                after_ms: 0,
            },
            &mut events,
        );
        capture.capture(2);
        wait_for(
            &capture,
            &Event::Receiving {
                pid: 2,
                after_ms: 0,
            },
            &mut events,
        );
        capture.stop();
        wait_for(
            &capture,
            &Event::Ended {
                pid: 2,
                reason: EndReason::Stopped,
            },
            &mut events,
        );
        assert!(events.contains(&Event::Ended {
            pid: 1,
            reason: EndReason::Replaced
        }));

        let items: Vec<Owned> = drain(&mut packetizer)
            .into_iter()
            .filter(|item| !matches!(item, Owned::Packet { .. }))
            .collect();
        let format = |stream| crate::StreamFormat {
            stream,
            sample_rate: 48_000,
            channels: 2,
        };
        assert_eq!(
            items,
            vec![
                Owned::Start(format(0)),
                Owned::End(0),
                Owned::Start(format(1)),
                Owned::End(1)
            ]
        );
        assert!(capture.shutdown(Duration::from_secs(1)));
    }
}
