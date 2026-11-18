//! The app's sound in the headset: an AAudio output whose data callback plays `openloop-playout`,
//! owned by one thread that opens it, grows its buffer after underruns, reopens it after a
//! disconnect and logs its statistics. See `docs/ARCHITECTURE.md` § Audio.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use openloop_playout::{BufferTuner, ClientTime, OutputTiming, Playout, Snapshot, Timestamp};
use openloop_protocol::MAX_AUDIO_CHANNELS;

use crate::aaudio::{Callback, Error, Handler, OutputStream};
use crate::net::now_ns;

/// Stereo: the Mac captures a stereo mixdown.
const CHANNELS: i32 = 2;
const POLL: Duration = Duration::from_millis(50);
const RETRY: Duration = Duration::from_secs(2);
const LOG_EVERY: Duration = Duration::from_secs(10);
/// Startup waits this long for the first open before the device profile reports no audio.
const FIRST_OPEN: Duration = Duration::from_secs(2);

/// The open output's rate and channel count, as AAudio granted them; zeros, which the device
/// profile reports as "plays no audio", while none is open.
#[derive(Default)]
pub struct Granted(AtomicU64);

impl Granted {
    pub fn get(&self) -> (u32, u8) {
        let packed = self.0.load(Ordering::Relaxed);
        ((packed >> 8) as u32, packed as u8)
    }

    fn set(&self, rate: u32, channels: u8) {
        self.0.store(
            u64::from(rate) << 8 | u64::from(channels),
            Ordering::Relaxed,
        );
    }
}

pub struct Audio {
    granted: Arc<Granted>,
    quit: Arc<AtomicBool>,
    owner: Option<JoinHandle<()>>,
}

impl Audio {
    /// Returns once the first open has finished, so the device profile can carry its result.
    pub fn start(playout: Playout) -> Self {
        let granted = Arc::new(Granted::default());
        let quit = Arc::new(AtomicBool::new(false));
        let (opened, first) = mpsc::sync_channel(1);
        let (shared, stop) = (granted.clone(), quit.clone());
        // The stream is not Send, so it is opened and dropped on its owner's thread.
        let owner = thread::Builder::new()
            .name("openloop-audio".into())
            .spawn(move || Owner::new(playout, shared).run(&stop, &opened))
            .map_err(|e| log::error!("audio thread: {e}"))
            .ok();
        if owner.is_some() && first.recv_timeout(FIRST_OPEN).is_err() {
            log::warn!("the audio output has not opened within {FIRST_OPEN:?}");
        }
        Self {
            granted,
            quit,
            owner,
        }
    }

    pub fn granted(&self) -> Arc<Granted> {
        self.granted.clone()
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
        if let Some(owner) = self.owner.take() {
            owner.thread().unpark();
            let _ = owner.join();
        }
    }
}

struct Render {
    playout: Playout,
    /// The stream's last error, 0 for none; the owner closes and reopens the stream.
    error: AtomicI32,
    owner: Thread,
}

impl Handler for Render {
    fn render(&self, stream: &Callback<'_>, out: &mut [f32]) {
        let now = ClientTime(now_ns());
        let timestamp = stream.timestamp().map(|(frame_position, time)| Timestamp {
            frame_position,
            time: ClientTime(time),
        });
        self.playout.render(
            out,
            &OutputTiming {
                sample_rate: stream.sample_rate(),
                channels: stream.channels(),
                now,
                frames_written: stream.frames_written(),
                timestamp,
                buffer_frames: stream.buffer_size(),
            },
        );
    }

    fn error(&self, error: Error) {
        self.error.store(error.0, Ordering::Relaxed);
        self.owner.unpark();
    }
}

struct Output {
    stream: OutputStream<Render>,
    tuner: BufferTuner,
}

impl Output {
    fn open(playout: &Playout) -> Result<Self, Error> {
        let stream = OutputStream::open(
            CHANNELS,
            Render {
                playout: playout.clone(),
                error: AtomicI32::new(0),
                owner: thread::current(),
            },
        )?;
        let tuner = BufferTuner::new(stream.frames_per_burst(), stream.buffer_capacity());
        if let Err(e) = stream.set_buffer_size(tuner.initial_size()) {
            log::warn!("audio output buffer: {e}");
        }
        stream.start()?;
        Ok(Self { stream, tuner })
    }

    fn tune(&mut self) {
        let size = self.stream.buffer_size();
        let Some(frames) = self
            .tuner
            .poll(ClientTime(now_ns()), self.stream.xrun_count(), size)
        else {
            return;
        };
        match self.stream.set_buffer_size(frames) {
            Ok(granted) => log::info!("audio output underran: buffer {size} -> {granted} frames"),
            Err(e) => log::warn!("audio output buffer: {e}"),
        }
    }
}

struct Owner {
    playout: Playout,
    granted: Arc<Granted>,
    output: Option<Output>,
    retry_at: Instant,
    logged: Snapshot,
    log_at: Instant,
}

impl Owner {
    fn new(playout: Playout, granted: Arc<Granted>) -> Self {
        let now = Instant::now();
        Self {
            playout,
            granted,
            output: None,
            retry_at: now,
            logged: Snapshot::default(),
            log_at: now + LOG_EVERY,
        }
    }

    fn run(mut self, quit: &AtomicBool, opened: &mpsc::SyncSender<()>) {
        self.poll();
        let _ = opened.send(());
        while !quit.load(Ordering::Acquire) {
            thread::park_timeout(POLL);
            self.poll();
        }
    }

    fn poll(&mut self) {
        let now = Instant::now();
        let error = self.output.as_ref().map_or(0, |output| {
            output.stream.handler().error.swap(0, Ordering::Relaxed)
        });
        if error != 0 {
            log::warn!("audio output: {}; reopening it", Error(error));
            // Closing joins the stream's callbacks, so the next stream renders alone.
            self.output = None;
            self.retry_at = now;
        }
        match &mut self.output {
            Some(output) => output.tune(),
            None if now >= self.retry_at => self.open(now),
            None => {}
        }
        if now >= self.log_at {
            self.log_at = now + LOG_EVERY;
            self.report();
        }
    }

    fn open(&mut self, now: Instant) {
        match Output::open(&self.playout) {
            Ok(output) => {
                let stream = &output.stream;
                let rate = u32::try_from(stream.sample_rate()).unwrap_or(0);
                let channels = u8::try_from(stream.channels())
                    .unwrap_or(0)
                    .min(MAX_AUDIO_CHANNELS);
                log::info!(
                    "audio output: {rate} Hz, {channels} channels, {}, {}, burst {} frames, \
                     buffer {} of {} frames",
                    stream.performance_mode(),
                    stream.sharing_mode(),
                    stream.frames_per_burst(),
                    stream.buffer_size(),
                    stream.buffer_capacity(),
                );
                self.granted.set(rate, channels);
                self.output = Some(output);
            }
            Err(e) => {
                log::warn!("audio output: {e}; retrying in {} s", RETRY.as_secs());
                self.granted.set(0, 0);
                self.retry_at = now + RETRY;
            }
        }
    }

    /// Quiet while no audio arrives.
    fn report(&mut self) {
        let s = self.playout.stats().snapshot();
        if s.packets == self.logged.packets {
            return;
        }
        let (xruns, buffer) = self.output.as_ref().map_or((0, 0), |output| {
            (output.stream.xrun_count(), output.stream.buffer_size())
        });
        let ms = |ns: i64| ns as f64 / 1e6;
        log::info!(
            "audio packets={} lost={} reordered={} late={} stale={} evicted={} dropped={} \
             rejected={} overflow={} dropouts={} silent_frames={} resyncs={} streams={} \
             output_restarts={} timestamps_rejected={} clock={:?} latency_ms={:.1} \
             extra_delay_ms={:.1} capped={} error_us={:+.0} trim_ppm={:+.1} buffered_ms={:.1} \
             xruns={xruns} buffer_frames={buffer}",
            s.packets,
            s.lost,
            s.reordered,
            s.late,
            s.stale,
            s.evicted,
            s.dropped,
            s.rejected,
            s.overflow,
            s.dropouts,
            s.silent_frames,
            s.resyncs,
            s.streams,
            s.output_restarts,
            s.timestamps_rejected,
            s.clock,
            ms(s.output_latency_ns),
            ms(s.extra_delay_ns),
            s.network_delay_capped,
            s.error_ns as f64 / 1e3,
            s.trim_ppb as f64 / 1e3,
            ms(s.buffered_ns),
        );
        self.logged = s;
    }
}
