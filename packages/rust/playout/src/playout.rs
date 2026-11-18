//! Plays received audio at its presentation time on the output's own clock. Each callback asks
//! the output clock when its first frame will be heard and reads the source frame scheduled for
//! that instant through the band-limited interpolator. A PI loop on the timing error trims the
//! read speed by at most ±0.1 %, absorbing the drift between the Mac's audio clock and the
//! headset's DAC; an error beyond `Tuning::resync_ns` jumps instead, with a crossfade. A gap
//! fades out ahead of the hole and back in after it while the schedule keeps running, and no
//! frame is ever played twice. When this output's latency leaves packets too little time, the
//! schedule moves later by a measured extra delay, published as `Snapshot::extra_delay_ns`,
//! rather than going silent. See `docs/ARCHITECTURE.md` § Audio.

use std::cell::UnsafeCell;
use std::f32::consts::FRAC_PI_2;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use openloop_protocol::ClientTime;

use crate::clock::OutputClock;
use crate::interp::{HALF_TAPS, Interpolator, TAPS};
use crate::packet::{Kind, PacketWriter, SLOT_CHANNELS, Slot};
use crate::ring::{AcquireRelease, Consumer, ring};
use crate::stats::{Counter, Gauge, Stats};
use crate::store::{Anchor, Anchors, CAPACITY, Frames};

/// The most extra delay the frame store has room for.
const MAX_EXTRA_NS: i64 = 1_000_000_000;
const WINDOWS: usize = 4;
const WINDOW_NS: i64 = 1_000_000_000;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Packet slots between the network thread and the audio callback, a power of two. Sized
    /// beyond the longest lead (target delay plus extra delay), the slot a full ring evicts is
    /// one already late.
    pub ring_slots: usize,
    pub tuning: Tuning,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ring_slots: 128,
            tuning: Tuning::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// Both closed-loop poles of the PI loop on the timing error, which obeys
    /// `e' = trim - drift`, sit at `natural_hz` with `damping`.
    pub natural_hz: f64,
    pub damping: f64,
    /// Low-pass on the raw error, which carries timestamp and callback jitter.
    pub error_tau_s: f64,
    /// Largest speed trim; 1e-3 is 1.7 cents of pitch.
    pub max_trim: f64,
    /// Errors beyond this jump with a crossfade instead of slewing.
    pub resync_ns: i64,
    /// Consecutive callbacks beyond `resync_ns` before a jump, so one bad estimate cannot.
    pub resync_after: u32,
    /// Fade length for gaps and crossfades, output frames.
    pub fade_frames: usize,
    /// A packet this far off its stream's schedule, or of another stream, starts a new
    /// schedule once the next packet agrees with it; alone it is dropped as stale.
    pub discontinuity_ns: i64,
    /// Most extra delay added for packets the network delivers late, on top of what this
    /// output's own latency forces; lateness beyond it is dropped instead.
    pub max_network_delay_ns: i64,
    /// Kept between a packet's arrival and the last moment a callback can use it.
    pub extra_delay_margin_ns: i64,
    /// The extra delay shrinks only after exceeding the need by this much for
    /// `extra_delay_shrink_after_ns`.
    pub extra_delay_hysteresis_ns: i64,
    pub extra_delay_shrink_after_ns: i64,
    /// Two output timestamps agree when each predicts the other this closely.
    pub timestamp_agree_ns: i64,
    /// A new timestamp this close to the current model is taken without confirmation.
    pub timestamp_step_ns: i64,
    /// Without agreeing timestamps for this long after the output starts, play by the
    /// callback clock instead of staying silent.
    pub timestamp_wait_ns: i64,
    /// A model no new timestamp confirmed for this long hands over to the callback clock.
    pub timestamp_stale_ns: i64,
    /// Output latency beyond the buffer, assumed when no timestamp ever arrived.
    /// UNVERIFIED on Quest 3: measure it there.
    pub assumed_latency_beyond_buffer_ns: i64,
    pub max_output_latency_ns: i64,
    /// A callback this long after the previous one means the output stopped and started again.
    pub restart_gap_ns: i64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            natural_hz: 0.08,
            damping: 1.0,
            error_tau_s: 0.1,
            max_trim: 1e-3,
            resync_ns: 5_000_000,
            resync_after: 3,
            fade_frames: 128,
            discontinuity_ns: 250_000_000,
            max_network_delay_ns: 100_000_000,
            extra_delay_margin_ns: 2_000_000,
            extra_delay_hysteresis_ns: 4_000_000,
            extra_delay_shrink_after_ns: 10_000_000_000,
            timestamp_agree_ns: 1_000_000,
            timestamp_step_ns: 2_000_000,
            timestamp_wait_ns: 500_000_000,
            timestamp_stale_ns: 2_000_000_000,
            assumed_latency_beyond_buffer_ns: 10_000_000,
            max_output_latency_ns: 1_000_000_000,
            restart_gap_ns: 200_000_000,
        }
    }
}

/// What the output reports at the start of a data callback.
#[derive(Clone, Copy, Debug)]
pub struct OutputTiming {
    pub sample_rate: u32,
    pub channels: usize,
    /// `CLOCK_MONOTONIC` when the callback began.
    pub now: ClientTime,
    /// `AAudioStream_getFramesWritten`: the stream position of this callback's first frame.
    pub frames_written: i64,
    /// `AAudioStream_getTimestamp(CLOCK_MONOTONIC)` when it succeeded. Read it here, in the
    /// data callback: it is the one `AAudioStream_get*` call AAudio does not make thread safe.
    pub timestamp: Option<Timestamp>,
    /// `AAudioStream_getBufferSizeInFrames`, used only while timestamps are missing.
    pub buffer_frames: u32,
}

/// Stream frame `frame_position` was heard at `time`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    pub frame_position: i64,
    pub time: ClientTime,
}

/// Allocates everything playout will use; nothing allocates afterwards.
pub fn new(config: Config) -> (PacketWriter, Playout) {
    let (producer, consumer) = ring::<Slot, AcquireRelease>(config.ring_slots);
    let stats = Arc::new(Stats::default());
    let engine = Engine {
        ring: consumer,
        pending: Box::default(),
        has_pending: false,
        core: Core::new(config.tuning, stats.clone()),
    };
    let playout = Playout {
        shared: Arc::new(Shared {
            rendering: AtomicBool::new(false),
            engine: UnsafeCell::new(engine),
            stats: stats.clone(),
        }),
    };
    (PacketWriter::new(producer, stats), playout)
}

/// The audio callback's side. Clone it into each output stream's data callback; one callback
/// renders at a time, and another that overlaps it gets silence instead of waiting.
#[derive(Clone)]
pub struct Playout {
    shared: Arc<Shared>,
}

struct Shared {
    rendering: AtomicBool,
    engine: UnsafeCell<Engine>,
    stats: Arc<Stats>,
}

// SAFETY: `engine` is touched only between winning `rendering` (Acquire) and handing it back
// (Release), so by one thread at a time.
unsafe impl Sync for Shared {}

impl Playout {
    /// Real-time safe: no allocation, locks or syscalls. Fills `out`, interleaved with
    /// `timing.channels` samples per frame. Returns false, with `out` silent, when another
    /// callback was rendering.
    pub fn render(&self, out: &mut [f32], timing: &OutputTiming) -> bool {
        if self.shared.rendering.swap(true, Ordering::Acquire) {
            out.fill(0.0);
            return false;
        }
        // SAFETY: see `impl Sync for Shared`; this thread just won `rendering`.
        let engine = unsafe { &mut *self.shared.engine.get() };
        engine.render(out, timing);
        self.shared.rendering.store(false, Ordering::Release);
        true
    }

    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }
}

struct Engine {
    ring: Consumer<Slot>,
    /// A packet off the current schedule, kept until the next one confirms or contradicts it.
    pending: Box<Slot>,
    has_pending: bool,
    core: Core,
}

impl Engine {
    fn render(&mut self, out: &mut [f32], timing: &OutputTiming) {
        let Self {
            ring,
            pending,
            has_pending,
            core,
        } = self;
        core.stats.add(Counter::Callbacks, 1);
        core.track_output(timing);
        let first_heard = core.clock.update(timing, &core.tuning, &core.stats);
        core.stats.set_clock(core.clock.mode());
        for _ in 0..ring.capacity() {
            let taken = ring.pop_with(|slot| core.accept(slot, pending, has_pending, timing.now.0));
            if taken.is_none() {
                break;
            }
        }
        match first_heard {
            Some(first_heard) => core.play(out, timing, first_heard),
            None => out.fill(0.0),
        }
    }
}

#[derive(Clone, Copy)]
struct Stream {
    id: u16,
    rate: u32,
    epoch: u16,
    /// The wire frame index stored at `origin`.
    base: u64,
    origin: i64,
    last_sequence: u32,
    newest: i64,
    /// The sender stopped the stream after this frame.
    end: Option<i64>,
}

impl Stream {
    fn local(&self, first_frame: u64) -> i64 {
        self.origin
            .wrapping_add(first_frame.wrapping_sub(self.base) as i64)
    }
}

#[derive(Clone, Copy)]
struct Output {
    rate: u32,
    channels: usize,
    written: i64,
    now: i64,
}

struct Core {
    frames: Frames,
    anchors: Anchors,
    interp: Interpolator,
    heads: [Head; 2],
    control: Control,
    clock: OutputClock,
    stream: Option<Stream>,
    epoch: u16,
    /// Past every frame any schedule stored or read; a new schedule starts beyond it, so the
    /// slots a retiring head still reads stay intact.
    frontier: i64,
    /// Frames of the current schedule up to here have been heard and are never read again.
    played_until: i64,
    lateness: Lateness,
    /// Added to every `present_at`, and published: moves toward `extra_target_ns` at half the
    /// trim budget, or at once while nothing plays or for a rise too large to slew.
    extra_ns: i64,
    extra_target_ns: i64,
    playing_since: Option<i64>,
    output: Option<Output>,
    tuning: Tuning,
    stats: Arc<Stats>,
}

impl Core {
    fn new(tuning: Tuning, stats: Arc<Stats>) -> Self {
        Self {
            frames: Frames::new(),
            anchors: Anchors::new(),
            interp: Interpolator::new(),
            heads: [Head::IDLE; 2],
            control: Control::default(),
            clock: OutputClock::new(),
            stream: None,
            epoch: 0,
            frontier: 0,
            played_until: i64::MIN,
            lateness: Lateness::default(),
            extra_ns: 0,
            extra_target_ns: 0,
            playing_since: None,
            output: None,
            tuning,
            stats,
        }
    }

    fn lookahead(&self) -> i64 {
        (HALF_TAPS + self.tuning.fade_frames + 2) as i64
    }

    /// A reopened, paused or reconfigured output starts over: its clock must agree again and
    /// playout picks the schedule up where it stands, never before what was already heard.
    fn track_output(&mut self, timing: &OutputTiming) {
        let output = Output {
            rate: timing.sample_rate,
            channels: timing.channels.max(1),
            written: timing.frames_written,
            now: timing.now.0,
        };
        let restarted = self.output.is_none_or(|last| {
            last.rate != output.rate
                || last.channels != output.channels
                || output.written < last.written
                || output.now - last.now > self.tuning.restart_gap_ns
        });
        if restarted {
            if self.output.is_some() {
                self.stats.add(Counter::OutputRestarts, 1);
            }
            self.clock.reset();
            self.heads = [Head::IDLE; 2];
            self.control.restart(&self.tuning);
            self.playing_since = None;
            self.lateness.forget_usable();
        }
        self.output = Some(output);
    }

    fn accept(&mut self, slot: &Slot, pending: &mut Slot, has_pending: &mut bool, now: i64) {
        if slot.kind == Kind::Stop {
            if *has_pending && pending.stream == slot.stream {
                *has_pending = false;
                self.stats.add(Counter::Stale, 1);
            }
            if let Some(stream) = &mut self.stream
                && stream.id == slot.stream
            {
                stream.end.get_or_insert(stream.newest);
            }
            return;
        }
        self.stats.add(Counter::Packets, 1);
        if self.continues_stream(slot) {
            if std::mem::take(has_pending) {
                self.stats.add(Counter::Stale, 1);
            }
            self.place(slot, now);
        } else if *has_pending && agrees(pending, slot, self.tuning.discontinuity_ns) {
            *has_pending = false;
            self.start_stream(pending);
            self.place(pending, now);
            self.place(slot, now);
        } else {
            if *has_pending {
                self.stats.add(Counter::Stale, 1);
            }
            pending.clone_from(slot);
            *has_pending = true;
        }
    }

    fn continues_stream(&self, slot: &Slot) -> bool {
        let Some(stream) = &self.stream else {
            return false;
        };
        if stream.end.is_some() || stream.id != slot.stream || stream.rate != slot.sample_rate {
            return false;
        }
        let local = stream.local(slot.first_frame) as f64;
        self.anchors
            .time_at(local, f64::from(stream.rate))
            .is_none_or(|expected| {
                (slot.present_at as f64 - expected).abs() <= self.tuning.discontinuity_ns as f64
            })
    }

    /// A new stream, or a restart or index jump within one: a new identity for its frames, so
    /// nothing stored under the old schedule is read as its audio. The old head plays on until
    /// the new one is heard or its own frames run out.
    fn start_stream(&mut self, first: &Slot) {
        let mut top = self.frontier;
        for head in self.heads.iter().filter(|h| h.live) {
            top = top.max(head.position.ceil() as i64 + HALF_TAPS as i64);
        }
        let origin = top + CAPACITY as i64 / 2;
        self.frontier = origin;
        self.epoch = self.epoch.wrapping_add(1);
        if self.heads[0].live {
            self.heads[1] = Head {
                retiring: true,
                ..self.heads[0]
            };
        }
        self.heads[0] = Head::IDLE;
        self.anchors.clear();
        self.played_until = i64::MIN;
        self.control.restart(&self.tuning);
        self.stream = Some(Stream {
            id: first.stream,
            rate: first.sample_rate,
            epoch: self.epoch,
            base: first.first_frame,
            origin,
            last_sequence: first.sequence.wrapping_sub(1),
            newest: origin,
            end: None,
        });
        self.stats.add(Counter::Streams, 1);
    }

    fn place(&mut self, slot: &Slot, now: i64) {
        let Some(stream) = &mut self.stream else {
            return;
        };
        let local = stream.local(slot.first_frame);
        let end = local + i64::from(slot.frames);
        let gap = slot.sequence.wrapping_sub(stream.last_sequence);
        if gap > u32::MAX / 2 {
            self.stats.add(Counter::Reordered, 1);
        } else if gap > 0 {
            self.stats.add(Counter::Lost, u64::from(gap - 1));
            stream.last_sequence = slot.sequence;
        }
        self.lateness
            .record_lead(now, slot.present_at.saturating_sub(slot.arrived));
        let head = &self.heads[0];
        let reading = head.live && head.epoch == stream.epoch;
        if local < 0 || (reading && (end as f64) < head.position - HALF_TAPS as f64) {
            self.stats.add(Counter::Late, 1);
            return;
        }
        if reading && end as f64 - head.position > (CAPACITY - TAPS) as f64 {
            self.stats.add(Counter::Overflow, 1);
            return;
        }
        self.frames.store(
            stream.epoch,
            local,
            usize::from(slot.channels.max(1)),
            slot.pcm(),
        );
        self.anchors.insert(Anchor {
            frame: local,
            time: slot.present_at,
        });
        stream.newest = stream.newest.max(end);
        self.frontier = self.frontier.max(end);
    }

    fn play(&mut self, out: &mut [f32], timing: &OutputTiming, first_heard: i64) {
        let channels = timing.channels.max(1);
        let count = out.len() / channels;
        let out_rate = f64::from(timing.sample_rate.max(1));
        let now = timing.now.0;
        // The last moment a packet can arrive and still be used: this callback writes `count`
        // frames and probes the lookahead beyond them before the first is heard.
        let usable = (first_heard - now)
            + ((count as i64 + self.lookahead()) as f64 * 1e9 / out_rate) as i64
            + self.tuning.extra_delay_margin_ns;
        let dt = count as f64 / out_rate;
        self.lateness.record_usable(now, usable);
        self.adapt_extra_delay(now);
        let slew = self.move_extra_delay(dt);
        self.end_finished_stream(first_heard);
        self.steer(first_heard, now, dt, out_rate);
        self.mix(out, channels, slew);
        if self.heads[0].live {
            self.anchors
                .retire_before(self.heads[0].position - HALF_TAPS as f64);
        }
        self.publish(first_heard - now);
    }

    /// While playback is new, covers whatever deficit was seen, since an output's latency
    /// settles after it starts. Afterwards rises only for a deficit seen in two windows, so one
    /// stall is one dropout rather than a lasting delay, and falls only after being comfortably
    /// more than needed for a while. What this output's latency forces (even the earliest packet
    /// is late) is always covered; network lateness beyond that only up to the cap.
    fn adapt_extra_delay(&mut self, now: i64) {
        let warming = self
            .playing_since
            .is_none_or(|since| now - since < WINDOWS as i64 * WINDOW_NS);
        let Some((worst, floor)) = self.lateness.deficits(warming) else {
            return;
        };
        let allowed = floor
            .max(0)
            .saturating_add(self.tuning.max_network_delay_ns.max(0));
        let needed = worst.min(allowed).clamp(0, MAX_EXTRA_NS);
        self.stats.set_network_delay_capped(worst > allowed);
        let headroom = self.tuning.extra_delay_hysteresis_ns / 2;
        if needed > self.extra_target_ns {
            self.extra_target_ns = (needed + headroom).min(MAX_EXTRA_NS);
            self.lateness.comfortable_since = None;
        } else if self.extra_target_ns - needed > self.tuning.extra_delay_hysteresis_ns {
            let since = *self.lateness.comfortable_since.get_or_insert(now);
            if now - since >= self.tuning.extra_delay_shrink_after_ns {
                self.extra_target_ns = needed + headroom;
                self.lateness.comfortable_since = None;
            }
        } else {
            self.lateness.comfortable_since = None;
        }
    }

    /// Moves the published extra delay toward its target. While it slews, returns the speed
    /// trim that keeps the playing head on the moving schedule, so the PI loop never sees the
    /// move. A rise too large to slew before packets would be missed jumps the head at once.
    fn move_extra_delay(&mut self, dt: f64) -> f64 {
        let delta = self.extra_target_ns - self.extra_ns;
        if delta == 0 || dt <= 0.0 {
            return 0.0;
        }
        if !self.heads[0].live || delta > self.tuning.resync_ns {
            self.extra_ns = self.extra_target_ns;
            if self.heads[0].live
                && !self.heads[1].live
                && let Some(stream) = self.stream
            {
                self.resync(delta as f64 * 1e-9 * f64::from(stream.rate));
            }
            return 0.0;
        }
        let rate = self.tuning.max_trim / 2.0;
        let most = ((rate * dt * 1e9) as i64).max(1);
        let step = delta.clamp(-most, most);
        self.extra_ns += step;
        -(step as f64) / (dt * 1e9)
    }

    fn end_finished_stream(&mut self, first_heard: i64) {
        let Some(stream) = self.stream else { return };
        let Some(end) = stream.end else { return };
        let position = match self.heads[0].live {
            true => Some(self.heads[0].position),
            false => self
                .anchors
                .frame_at(first_heard - self.extra_ns, f64::from(stream.rate)),
        };
        if position.is_none_or(|p| p - HALF_TAPS as f64 >= end as f64) {
            self.heads[0] = Head::IDLE;
            self.stream = None;
            self.anchors.clear();
        }
    }

    fn steer(&mut self, first_heard: i64, now: i64, dt: f64, out_rate: f64) {
        let Some(stream) = self.stream else { return };
        let rate = f64::from(stream.rate);
        if !self.heads[0].live {
            if let Some(position) = self.anchors.frame_at(first_heard - self.extra_ns, rate) {
                let mut head = Head::start(position, stream.epoch, rate / out_rate);
                head.missing = head.missing.max(self.played_until);
                self.heads[0] = head;
                self.playing_since.get_or_insert(now);
            }
            return;
        }
        let Some(scheduled) = self.anchors.time_at(self.heads[0].position, rate) else {
            return;
        };
        let error_s = (scheduled + self.extra_ns as f64 - first_heard as f64) * 1e-9;
        if let Some(jump_s) = self.control.update(error_s, dt, &self.tuning)
            && !self.heads[1].live
        {
            self.resync(jump_s * rate);
        }
    }

    fn resync(&mut self, jump_frames: f64) {
        let old = self.heads[0];
        let mut new = Head::start(old.position - jump_frames, old.epoch, old.step);
        // Never play a frame twice: nothing the old head played, or will play while it fades
        // out, is read again; the new head fades in past it, still on schedule.
        let fade_reach = (old.step * self.tuning.fade_frames as f64).ceil() as i64 + 1;
        let old_last = old.position.floor() as i64 + HALF_TAPS as i64 + fade_reach;
        new.missing = new.missing.max(self.played_until).max(old_last);
        self.heads[1] = Head {
            fading: true,
            retiring: false,
            ..old
        };
        self.heads[0] = new;
        self.stats.add(Counter::Resyncs, 1);
    }

    fn mix(&mut self, out: &mut [f32], channels: usize, slew: f64) {
        let lookahead = self.lookahead();
        let step = 1.0 / self.tuning.fade_frames.max(1) as f32;
        let max_trim = self.tuning.max_trim;
        let speed = 1.0 + (self.control.trim + slew).clamp(-max_trim, max_trim);
        let epoch = self.stream.map(|s| s.epoch);
        let end = self.stream.and_then(|s| s.end);
        let (mut silent, mut dropouts) = (0u64, 0u64);
        for frame in out.chunks_exact_mut(channels) {
            if self.heads[1].retiring && self.heads[0].live && self.heads[0].gate > 0.0 {
                self.heads[1].retiring = false;
                self.heads[1].fading = true;
            }
            let mut mix = [0.0f32; SLOT_CHANNELS];
            for index in 0..self.heads.len() {
                let head = &mut self.heads[index];
                if !head.live {
                    continue;
                }
                let complete = head.advance_gate(&self.frames, lookahead, step);
                if index == 0 && head.audible && !complete && end.is_none_or(|e| head.missing < e) {
                    silent += 1;
                    dropouts += u64::from(head.was_complete);
                }
                head.was_complete = complete;
                let weight = head.weight();
                if weight > 0.0 {
                    let base = head.position.floor();
                    let offset = head.position - base;
                    let base = base as i64;
                    for (c, m) in mix.iter_mut().enumerate() {
                        *m += weight * self.interp.read(self.frames.window(c, base), offset);
                    }
                    if epoch == Some(head.epoch) {
                        self.played_until = self.played_until.max(base + HALF_TAPS as i64);
                    }
                }
                head.position += head.step * speed;
                if head.retiring && head.gate <= 0.0 {
                    *head = Head::IDLE;
                } else {
                    head.fade_out(step);
                }
            }
            match frame {
                [mono] => *mono = 0.5 * (mix[0] + mix[1]),
                [left, right, rest @ ..] => {
                    *left = mix[0];
                    *right = mix[1];
                    rest.fill(0.0);
                }
                [] => {}
            }
        }
        self.stats.add(Counter::SilentFrames, silent);
        self.stats.add(Counter::Dropouts, dropouts);
    }

    fn publish(&self, output_latency_ns: i64) {
        self.stats
            .set(Gauge::ErrorNs, (self.control.filtered * 1e9) as i64);
        self.stats
            .set(Gauge::TrimPpb, (self.control.trim * 1e9) as i64);
        self.stats.set(Gauge::ExtraDelayNs, self.extra_ns);
        self.stats.set(Gauge::OutputLatencyNs, output_latency_ns);
        if let Some(stream) = self.stream
            && self.heads[0].live
        {
            let ahead = stream.newest as f64 - self.heads[0].position;
            self.stats.set(
                Gauge::BufferedNs,
                (ahead / f64::from(stream.rate) * 1e9) as i64,
            );
        }
    }
}

/// Two packets of one schedule: same stream and rate, and times that agree with their frames.
fn agrees(earlier: &Slot, later: &Slot, tolerance_ns: i64) -> bool {
    if earlier.stream != later.stream
        || earlier.sample_rate != later.sample_rate
        || earlier.first_frame == later.first_frame
    {
        return false;
    }
    let frames = later.first_frame.wrapping_sub(earlier.first_frame) as i64 as f64;
    let expected = earlier.present_at as f64 + frames * 1e9 / f64::from(earlier.sample_rate);
    (later.present_at as f64 - expected).abs() <= tolerance_ns as f64
}

#[derive(Clone, Copy)]
struct Head {
    live: bool,
    epoch: u16,
    position: f64,
    /// Source frames per output frame, before the trim.
    step: f64,
    /// Ramps toward 1 while the read window and lookahead are complete, toward 0 otherwise.
    gate: f32,
    /// Crossfade weight, falling to 0 once `fading`.
    fade: f32,
    fading: bool,
    /// Plays a replaced schedule on until the new head is heard or its frames run out.
    retiring: bool,
    probed: i64,
    /// The newest frame the lookahead found missing.
    missing: i64,
    /// Has been fully audible; later gaps count as dropouts.
    audible: bool,
    was_complete: bool,
}

impl Head {
    const IDLE: Self = Self {
        live: false,
        epoch: 0,
        position: 0.0,
        step: 1.0,
        gate: 0.0,
        fade: 1.0,
        fading: false,
        retiring: false,
        probed: 0,
        missing: 0,
        audible: false,
        was_complete: false,
    };

    fn start(position: f64, epoch: u16, step: f64) -> Self {
        let base = position.floor() as i64;
        Self {
            live: true,
            epoch,
            position,
            step,
            probed: base - HALF_TAPS as i64,
            missing: base - HALF_TAPS as i64,
            was_complete: true,
            ..Self::IDLE
        }
    }

    /// Probes the frames newly reached by the lookahead and moves the gate; true while the
    /// read window is complete.
    fn advance_gate(&mut self, frames: &Frames, lookahead: i64, step: f32) -> bool {
        let base = self.position.floor() as i64;
        while self.probed < base + lookahead {
            self.probed += 1;
            if !frames.has(self.epoch, self.probed) {
                self.missing = self.probed;
            }
        }
        let complete = base - (HALF_TAPS as i64 - 1) > self.missing;
        self.gate = if complete {
            (self.gate + step).min(1.0)
        } else {
            (self.gate - step).max(0.0)
        };
        if self.gate >= 1.0 && !self.fading {
            self.audible = true;
        }
        complete
    }

    /// Equal-power weight from the gate and crossfade ramps.
    fn weight(&self) -> f32 {
        let curve = |x: f32| if x >= 1.0 { 1.0 } else { (FRAC_PI_2 * x).sin() };
        curve(self.gate) * curve(self.fade)
    }

    fn fade_out(&mut self, step: f32) {
        if self.fading {
            self.fade -= step;
            if self.fade <= 0.0 {
                *self = Self::IDLE;
            }
        }
    }
}

#[derive(Default)]
struct Control {
    filtered: f64,
    integral: f64,
    over: u32,
    trim: f64,
}

impl Control {
    fn gains(tuning: &Tuning) -> (f64, f64) {
        let omega = std::f64::consts::TAU * tuning.natural_hz;
        (2.0 * tuning.damping * omega, omega * omega)
    }

    /// Folds in one callback's timing error (seconds, positive when the frame would be heard
    /// early); returns a jump in seconds when the error is too large to slew.
    fn update(&mut self, error_s: f64, dt: f64, tuning: &Tuning) -> Option<f64> {
        if error_s.abs() * 1e9 > tuning.resync_ns as f64 {
            self.over += 1;
            if self.over >= tuning.resync_after {
                self.over = 0;
                self.filtered = 0.0;
                return Some(error_s);
            }
            return None;
        }
        self.over = 0;
        self.filtered += (1.0 - (-dt / tuning.error_tau_s).exp()) * (error_s - self.filtered);
        let (kp, ki) = Self::gains(tuning);
        let limit = tuning.max_trim / ki;
        self.integral = (self.integral + self.filtered * dt).clamp(-limit, limit);
        self.trim =
            (-(kp * self.filtered + ki * self.integral)).clamp(-tuning.max_trim, tuning.max_trim);
        None
    }

    /// Keeps the integral, which has learned the drift between the two audio clocks.
    fn restart(&mut self, tuning: &Tuning) {
        self.filtered = 0.0;
        self.over = 0;
        let (_, ki) = Self::gains(tuning);
        self.trim = (-(ki * self.integral)).clamp(-tuning.max_trim, tuning.max_trim);
    }
}

#[derive(Clone, Copy)]
struct Window {
    min_lead: i64,
    max_lead: i64,
    usable: i64,
}

impl Window {
    const EMPTY: Self = Self {
        min_lead: i64::MAX,
        max_lead: i64::MIN,
        usable: i64::MIN,
    };
}

/// How much notice packets give (presentation time minus arrival) against how much this output
/// needs, per one-second window over the last few seconds.
struct Lateness {
    windows: [Window; WINDOWS],
    current: usize,
    window_start: Option<i64>,
    comfortable_since: Option<i64>,
}

impl Default for Lateness {
    fn default() -> Self {
        Self {
            windows: [Window::EMPTY; WINDOWS],
            current: 0,
            window_start: None,
            comfortable_since: None,
        }
    }
}

impl Lateness {
    fn window(&mut self, now: i64) -> &mut Window {
        let start = *self.window_start.get_or_insert(now);
        if now - start >= WINDOW_NS {
            self.current = (self.current + 1) % WINDOWS;
            self.windows[self.current] = Window::EMPTY;
            self.window_start = Some(now);
        }
        &mut self.windows[self.current]
    }

    fn record_lead(&mut self, now: i64, lead: i64) {
        let window = self.window(now);
        window.min_lead = window.min_lead.min(lead);
        window.max_lead = window.max_lead.max(lead);
    }

    fn record_usable(&mut self, now: i64, usable: i64) {
        let window = self.window(now);
        window.usable = window.usable.max(usable);
    }

    fn forget_usable(&mut self) {
        for window in &mut self.windows {
            window.usable = i64::MIN;
        }
    }

    /// How late the latest packet was for this output, worst over the windows while `warming`
    /// and otherwise as bad in at least two of them, and how late even the earliest packet of
    /// the last few seconds was.
    fn deficits(&self, warming: bool) -> Option<(i64, i64)> {
        let usable = self
            .windows
            .iter()
            .map(|w| w.usable)
            .max()
            .filter(|&u| u != i64::MIN)?;
        let earliest = self
            .windows
            .iter()
            .map(|w| w.max_lead)
            .max()
            .filter(|&l| l != i64::MIN)?;
        let mut worst = self.windows.map(|w| match w.min_lead {
            i64::MAX => i64::MIN,
            lead => usable.saturating_sub(lead),
        });
        worst.sort_unstable();
        let worst = worst[if warming { WINDOWS - 1 } else { WINDOWS - 2 }];
        Some((worst, usable.saturating_sub(earliest)))
    }
}
