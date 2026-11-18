//! End-to-end simulations at 48 kHz. The Mac captures on its own audio clock and stamps each
//! packet with the headset time its first frame is due; the link adds jitter, stalls, loss and
//! reordering; an AAudio-like output plays on the DAC's clock and reports presentation
//! timestamps on the headset's clock.
//!
//! The audio is a timecode of the true capture frame: left and right are its sine and cosine
//! over `PERIOD` frames. The frame heard is recovered from the output samples alone and compared
//! with when its packet said it was due, so the measurement never trusts the playout's own
//! numbers, and a frame heard twice shows up as a replay.

use std::collections::BTreeSet;
use std::f64::consts::TAU;

use openloop_playout::{ClientTime, ClockMode, Config, OutputTiming, Snapshot, Timestamp, Tuning};
use openloop_protocol::{AudioFormat, AudioHeader};

const RATE: f64 = 48_000.0;
const PERIOD: f64 = 32_768.0;
const AMPLITUDE: f64 = 0.9;
const HEADSET_EPOCH_NS: i64 = 9_000_000_000_000;
const CAPTURE_START: f64 = 0.050;
const HEAR_START: f64 = 0.020;

fn headset(t: f64) -> i64 {
    HEADSET_EPOCH_NS + (t * 1e9).round() as i64
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn symmetric(&mut self) -> f64 {
        self.uniform() * 2.0 - 1.0
    }
    fn exp(&mut self, mean: f64) -> f64 {
        -mean * self.uniform().ln()
    }
    fn chance(&mut self, p: f64) -> bool {
        self.uniform() < p
    }
}

/// What the Mac's capture does at a given capture time.
#[derive(Clone, Copy, Debug)]
enum Restart {
    /// A new stream id, frames from 0.
    NewId,
    /// The same stream id, frames and sequence numbers from 0 again.
    SameIdFromZero,
    /// The same stream, its frame index jumping by this many frames.
    Jump(i64),
}

#[derive(Clone, Debug)]
struct Scenario {
    seconds: f64,
    seed: u64,
    source_ppm: f64,
    dac_ppm: f64,
    link_ms: f64,
    jitter_ms: f64,
    /// TCP: delivery in order.
    ordered: bool,
    stalls: Vec<(f64, f64)>,
    stall_every: Option<(f64, f64)>,
    /// `(every_ms, hold_ms)`: what is sent during the first `hold_ms` of each period waits
    /// until its end, as audio queued behind video on one TCP stream.
    head_of_line: Option<(f64, f64)>,
    loss: f64,
    loss_runs: Vec<(u64, u64)>,
    late: Vec<(u64, f64)>,
    /// Chance that a datagram is held back this long, landing behind later ones.
    reorder: Option<(f64, f64)>,
    delay_ms: f64,
    packet_frames: usize,
    /// Cycled through, one per callback.
    callback_frames: Vec<usize>,
    /// From a callback starting to its first frame being heard.
    output_latency_ms: f64,
    callback_jitter_ms: f64,
    timestamp_every_ms: f64,
    timestamp_jitter_us: f64,
    /// After each output start, no timestamp for this long.
    timestamps_after_s: f64,
    /// No timestamps from this time on.
    timestamps_until_s: f64,
    /// Every `n`th timestamp is off by `ms`.
    timestamp_outliers: Option<(u64, f64)>,
    mapping_steps: Vec<(f64, f64)>,
    restarts: Vec<(f64, Restart)>,
    /// The Mac stops the stream at the first time and starts a new one at the second.
    stop: Option<(f64, f64)>,
    /// The output stops at this time and is open again, counting from frame 0, this much later.
    output_restart: Option<(f64, f64)>,
    /// The DAC underruns at this time, for this long: later frames are heard that much later.
    underrun: Option<(f64, f64)>,
    ring_slots: usize,
    settle_s: f64,
    tuning: Tuning,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            seconds: 30.0,
            seed: 1,
            source_ppm: 0.0,
            dac_ppm: 0.0,
            link_ms: 1.0,
            jitter_ms: 0.3,
            ordered: true,
            stalls: vec![],
            stall_every: None,
            head_of_line: None,
            loss: 0.0,
            loss_runs: vec![],
            late: vec![],
            reorder: None,
            delay_ms: 40.0,
            packet_frames: 240,
            callback_frames: vec![192],
            output_latency_ms: 8.0,
            callback_jitter_ms: 0.5,
            timestamp_every_ms: 10.0,
            timestamp_jitter_us: 20.0,
            timestamps_after_s: 0.0,
            timestamps_until_s: f64::INFINITY,
            timestamp_outliers: None,
            mapping_steps: vec![],
            restarts: vec![],
            stop: None,
            output_restart: None,
            underrun: None,
            ring_slots: 128,
            settle_s: 5.0,
            tuning: Tuning::default(),
        }
    }
}

/// Absolute values in 0.5 µs bins up to 50 ms, so ten minutes of frames fit.
struct Summary {
    count: u64,
    sum: f64,
    max: f64,
    bins: Vec<u64>,
}

impl Summary {
    const BIN_US: f64 = 0.5;

    fn new() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            max: 0.0,
            bins: vec![0; 100_001],
        }
    }
    fn push(&mut self, us: f64) {
        self.count += 1;
        self.sum += us;
        self.max = self.max.max(us.abs());
        let bin = ((us.abs() / Self::BIN_US) as usize).min(self.bins.len() - 1);
        self.bins[bin] += 1;
    }
    fn mean(&self) -> f64 {
        self.sum / self.count.max(1) as f64
    }
    fn p(&self, q: f64) -> f64 {
        let target = (self.count as f64 * q).ceil() as u64;
        let mut seen = 0;
        for (bin, n) in self.bins.iter().enumerate() {
            seen += n;
            if seen >= target.max(1) {
                return (bin + 1) as f64 * Self::BIN_US;
            }
        }
        f64::NAN
    }
}

struct Report {
    measured: u64,
    /// Heard minus due (its packet's `present_at` plus the published extra delay),
    /// microseconds, after `settle_s`.
    schedule_us: Summary,
    first_audible_s: Option<f64>,
    first_audible_error_us: Option<f64>,
    muted_after_start: u64,
    longest_mute_ms: f64,
    muted_in_last_5_s: u64,
    /// Frames heard behind a frame already heard.
    replayed: u64,
    trim_ppm_mean: f64,
    trim_ppm_max: f64,
    extra_ms_max: f64,
    extra_ms_final: f64,
    clocks: BTreeSet<u8>,
    stats: Snapshot,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = &self.stats;
        writeln!(
            f,
            "  measured {}  first sound {:?} s ({:?} us off)  muted after start {} (longest {:.1} ms, last 5 s {})  replayed {}",
            self.measured,
            self.first_audible_s.map(|t| (t * 1e3).round() / 1e3),
            self.first_audible_error_us.map(|e| e.round()),
            self.muted_after_start,
            self.longest_mute_ms,
            self.muted_in_last_5_s,
            self.replayed
        )?;
        writeln!(
            f,
            "  schedule error us: mean {:+.2}  p50 {:.1}  p99 {:.1}  max {:.1} | trim mean {:+.1} ppm max {:.1} | extra delay final {:.2} ms max {:.2} ms | clock {:?}",
            self.schedule_us.mean(),
            self.schedule_us.p(0.5),
            self.schedule_us.p(0.99),
            self.schedule_us.max,
            self.trim_ppm_mean,
            self.trim_ppm_max,
            self.extra_ms_final,
            self.extra_ms_max,
            self.clocks
        )?;
        write!(
            f,
            "  packets {} late {} lost {} reordered {} stale {} evicted {} overflow {} dropouts {} silent {} resyncs {} streams {} output restarts {} timestamps rejected {} capped {}",
            s.packets,
            s.late,
            s.lost,
            s.reordered,
            s.stale,
            s.evicted,
            s.overflow,
            s.dropouts,
            s.silent_frames,
            s.resyncs,
            s.streams,
            s.output_restarts,
            s.timestamps_rejected,
            s.network_delay_capped
        )
    }
}

/// The output's DAC: frame `w` of the current open stream is heard at `heard(w)`.
struct Dac {
    start: f64,
    rate: f64,
    underrun: Option<(f64, f64)>,
}

impl Dac {
    fn heard(&self, w: f64) -> f64 {
        let shift = match self.underrun {
            Some((from, s)) if w >= from => s,
            _ => 0.0,
        };
        self.start + w / self.rate + shift
    }

    fn position_at(&self, t: f64) -> f64 {
        let p = (t - self.start) * self.rate;
        match self.underrun {
            Some((from, s)) if p >= from => {
                let stalled_until = self.start + from / self.rate + s;
                if t < stalled_until {
                    from
                } else {
                    (t - self.start - s) * self.rate
                }
            }
            _ => p,
        }
    }
}

enum Item {
    Audio(AudioHeader, Vec<i16>),
    Stop(u16),
}

struct InFlight {
    arrival: f64,
    order: u64,
    item: Item,
}

struct Wire {
    id: u16,
    /// Wire frame index minus capture frame index.
    frame_offset: i64,
    /// Wire sequence number minus packet number.
    sequence_offset: i64,
}

fn run(s: &Scenario) -> Report {
    let mut rng = Rng(s.seed);
    let source_rate = RATE * (1.0 + s.source_ppm * 1e-6);
    let pf = s.packet_frames;
    let captured = |g: f64| CAPTURE_START + g / source_rate;
    let step_ms = |t: f64| -> f64 {
        s.mapping_steps
            .iter()
            .filter(|&&(at, _)| t >= at)
            .map(|&(_, ms)| ms)
            .sum()
    };

    let (mut writer, playout) = openloop_playout::new(Config {
        ring_slots: s.ring_slots,
        tuning: s.tuning,
    });

    let mut restarts = s.restarts.clone();
    restarts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut next_restart = 0;
    let mut wire = Wire {
        id: 1,
        frame_offset: 0,
        sequence_offset: 0,
    };
    let mut stopped = false;
    let mut schedules: Vec<Option<i64>> = Vec::new();
    let mut in_flight: Vec<InFlight> = Vec::new();
    let mut order = 0u64;
    let mut last_arrival = 0.0f64;

    let mut dac = Dac {
        start: HEAR_START,
        rate: RATE * (1.0 + s.dac_ppm * 1e-6),
        underrun: None,
    };
    let mut output_started = HEAR_START - s.output_latency_ms * 1e-3;
    let mut output_restarted = false;
    let mut written = 0i64;
    let mut callback = 0usize;
    let mut timestamps_made = 0u64;
    let largest = s.callback_frames.iter().copied().max().unwrap();
    let mut out = vec![0.0f32; largest * 2];

    let mut report = Report {
        measured: 0,
        schedule_us: Summary::new(),
        first_audible_s: None,
        first_audible_error_us: None,
        muted_after_start: 0,
        longest_mute_ms: 0.0,
        muted_in_last_5_s: 0,
        replayed: 0,
        trim_ppm_mean: 0.0,
        trim_ppm_max: 0.0,
        extra_ms_max: 0.0,
        extra_ms_final: 0.0,
        clocks: BTreeSet::new(),
        stats: Snapshot::default(),
    };
    let (mut mute_run, mut max_g) = (0u64, f64::NEG_INFINITY);
    let (mut trim_sum, mut trim_n) = (0.0f64, 0u64);

    loop {
        let n = s.callback_frames[callback % s.callback_frames.len()];
        let mut first = dac.heard(written as f64);
        if let Some((at, ms)) = s.underrun
            && dac.underrun.is_none()
            && first >= at
        {
            dac.underrun = Some((written as f64, ms * 1e-3));
            first = dac.heard(written as f64);
        }
        if first > s.seconds {
            break;
        }
        let jitter = rng.symmetric() * s.callback_jitter_ms * 1e-3;
        let now = (first - s.output_latency_ms * 1e-3 + jitter).min(first - 1e-4);
        if let Some((at, gap_ms)) = s.output_restart
            && !output_restarted
            && now >= at
        {
            output_restarted = true;
            dac.start = first + gap_ms * 1e-3;
            dac.underrun = None;
            output_started = dac.start - s.output_latency_ms * 1e-3;
            written = 0;
            continue;
        }

        // The Mac sends each packet once its last frame is captured.
        loop {
            let k = schedules.len() as u64;
            let g0 = k * pf as u64;
            let at = captured(g0 as f64);
            let sent = captured((g0 + pf as u64) as f64) + 0.0003;
            if sent > now + 0.001 {
                break;
            }
            while let Some(&(t, kind)) = restarts.get(next_restart)
                && at >= t
            {
                next_restart += 1;
                match kind {
                    Restart::NewId => {
                        wire.id += 1;
                        wire.frame_offset = -(g0 as i64);
                        wire.sequence_offset = -(k as i64);
                    }
                    Restart::SameIdFromZero => {
                        wire.frame_offset = -(g0 as i64);
                        wire.sequence_offset = -(k as i64);
                    }
                    Restart::Jump(frames) => wire.frame_offset += frames,
                }
            }
            if let Some((stop_at, resume_at)) = s.stop
                && at >= stop_at
            {
                if at < resume_at {
                    if !stopped {
                        stopped = true;
                        in_flight.push(InFlight {
                            arrival: sent + s.link_ms * 1e-3,
                            order,
                            item: Item::Stop(wire.id),
                        });
                        order += 1;
                    }
                    schedules.push(None);
                    continue;
                }
                if stopped {
                    stopped = false;
                    wire.id += 1;
                    wire.frame_offset = -(g0 as i64);
                    wire.sequence_offset = -(k as i64);
                }
            }
            let present = headset(at) + ((s.delay_ms + step_ms(at)) * 1e6).round() as i64;
            schedules.push(Some(present));
            let sequence = (k as i64 + wire.sequence_offset) as u64;
            let header = AudioHeader {
                format: AudioFormat {
                    stream: wire.id,
                    sample_rate: 48_000,
                    channels: 2,
                },
                sequence: sequence as u32,
                first_frame: (g0 as i64 + wire.frame_offset) as u64,
                present_at: ClientTime(present),
            };
            if (s.loss > 0.0 && rng.chance(s.loss))
                || s.loss_runs
                    .iter()
                    .any(|&(from, n)| k >= from && k < from + n)
            {
                continue;
            }
            let mut arrival = sent + (s.link_ms + rng.exp(s.jitter_ms)) * 1e-3;
            let periodic = s
                .stall_every
                .map(|(every, ms)| ((sent / every).floor() * every, ms));
            for (start, ms) in s.stalls.iter().copied().chain(periodic) {
                if sent >= start && sent < start + ms * 1e-3 {
                    arrival = arrival.max(start + ms * 1e-3 + s.link_ms * 1e-3);
                }
            }
            if let Some((every_ms, hold_ms)) = s.head_of_line {
                let period = every_ms * 1e-3;
                let start = (sent / period).floor() * period;
                if sent < start + hold_ms * 1e-3 {
                    arrival = arrival.max(start + hold_ms * 1e-3);
                }
            }
            if let Some(&(_, ms)) = s.late.iter().find(|&&(q, _)| q == k) {
                arrival += ms * 1e-3;
            }
            if let Some((chance, ms)) = s.reorder
                && rng.chance(chance)
            {
                arrival += ms * 1e-3;
            }
            if s.ordered {
                arrival = arrival.max(last_arrival);
                last_arrival = arrival;
            }
            let samples = (0..pf)
                .flat_map(|j| {
                    let phase = TAU * (g0 + j as u64) as f64 / PERIOD;
                    [
                        (AMPLITUDE * phase.sin() * 32767.0).round() as i16,
                        (AMPLITUDE * phase.cos() * 32767.0).round() as i16,
                    ]
                })
                .collect();
            in_flight.push(InFlight {
                arrival,
                order,
                item: Item::Audio(header, samples),
            });
            order += 1;
        }

        in_flight.sort_by(|a, b| a.arrival.total_cmp(&b.arrival).then(a.order.cmp(&b.order)));
        let ready = in_flight.partition_point(|p| p.arrival <= now);
        for p in in_flight.drain(..ready) {
            let arrived = ClientTime(headset(p.arrival));
            match p.item {
                Item::Audio(header, samples) => {
                    writer.push(&header, samples.into_iter(), arrived);
                }
                Item::Stop(id) => {
                    writer.push_stop(id, arrived);
                }
            }
        }

        // The output's latest presentation timestamp, refreshed every `timestamp_every_ms`.
        let every = s.timestamp_every_ms * 1e-3;
        let refreshed = dac.start + ((now - dac.start) / every).floor() * every;
        let position = dac.position_at(refreshed).floor();
        let available = now - output_started >= s.timestamps_after_s
            && now < s.timestamps_until_s
            && refreshed >= dac.start
            && position >= 0.0
            && position as i64 <= written;
        let timestamp = available.then(|| {
            timestamps_made += 1;
            let mut time = headset(dac.heard(position))
                + (rng.symmetric() * s.timestamp_jitter_us * 1e3) as i64;
            if let Some((every, ms)) = s.timestamp_outliers
                && timestamps_made.is_multiple_of(every)
            {
                time += (ms * 1e6) as i64;
            }
            Timestamp {
                frame_position: position as i64,
                time: ClientTime(time),
            }
        });

        let out = &mut out[..n * 2];
        playout.render(
            out,
            &OutputTiming {
                sample_rate: 48_000,
                channels: 2,
                now: ClientTime(headset(now)),
                frames_written: written,
                timestamp,
                buffer_frames: (s.output_latency_ms * 48.0) as u32,
            },
        );
        let stats = playout.stats().snapshot();
        report.clocks.insert(stats.clock as u8);
        let extra_ms = stats.extra_delay_ns as f64 / 1e6;
        report.extra_ms_max = report.extra_ms_max.max(extra_ms);
        report.extra_ms_final = extra_ms;

        for (i, frame) in out.chunks_exact(2).enumerate() {
            let t = dac.heard((written + i as i64) as f64);
            let (l, r) = (f64::from(frame[0]), f64::from(frame[1]));
            let amplitude = (l * l + r * r).sqrt() / AMPLITUDE;
            let due = t - s.delay_ms * 1e-3 - extra_ms * 1e-3;
            let expected = (due - step_ms(due) * 1e-3 - CAPTURE_START) * source_rate;
            let wrapped = |x: f64| x - PERIOD * (x / PERIOD).round();
            let angle = l.atan2(r) / TAU * PERIOD;
            let g = expected + wrapped(angle - wrapped(expected));
            // A fading frame still carries its phase, so a replay hidden in a fade counts too.
            if amplitude > 0.05 {
                if g < max_g - 2.0 {
                    report.replayed += 1;
                }
                max_g = max_g.max(g);
            }
            if amplitude < 0.999 {
                if report.first_audible_s.is_some() {
                    report.muted_after_start += 1;
                    mute_run += 1;
                    report.longest_mute_ms = report.longest_mute_ms.max(mute_run as f64 / 48.0);
                    if t >= s.seconds - 5.0 {
                        report.muted_in_last_5_s += 1;
                    }
                }
                continue;
            }
            mute_run = 0;
            let k = (g / pf as f64).floor();
            let Some(&Some(present)) = (k >= 0.0).then(|| schedules.get(k as usize)).flatten()
            else {
                continue;
            };
            let scheduled =
                present as f64 + (g - k * pf as f64) * 1e9 / RATE + stats.extra_delay_ns as f64;
            let error_us = (headset(t) as f64 - scheduled) / 1e3;
            if report.first_audible_s.is_none() {
                report.first_audible_s = Some(t);
                report.first_audible_error_us = Some(error_us);
            }
            report.measured += 1;
            if t >= s.settle_s {
                report.schedule_us.push(error_us);
            }
        }
        if dac.heard(written as f64) >= s.settle_s {
            let trim = stats.trim_ppb as f64 / 1e3;
            trim_sum += trim;
            trim_n += 1;
            report.trim_ppm_max = report.trim_ppm_max.max(trim.abs());
        }
        written += n as i64;
        callback += 1;
    }
    report.trim_ppm_mean = trim_sum / trim_n.max(1) as f64;
    report.stats = playout.stats().snapshot();
    report
}

fn show(name: &str, s: &Scenario) -> Report {
    let report = run(s);
    println!("{name} ({} s simulated)\n{report}", s.seconds);
    report
}

fn clock(mode: ClockMode) -> u8 {
    mode as u8
}

#[test]
fn steady_state_locks_to_the_schedule() {
    let r = show("steady state", &Scenario::default());
    let first = r.first_audible_error_us.expect("audio started");
    assert!(first.abs() < 50.0, "first sound {first:.1} us off schedule");
    assert!(r.first_audible_s.unwrap() < 0.1, "{:?}", r.first_audible_s);
    assert_eq!(r.muted_after_start, 0);
    assert_eq!(r.replayed, 0);
    let s = r.stats;
    assert_eq!(
        (s.resyncs, s.dropouts, s.late, s.stale, s.evicted),
        (0, 0, 0, 0, 0)
    );
    assert_eq!(s.streams, 1);
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
    assert_eq!(r.extra_ms_max, 0.0, "the requested schedule was feasible");
    assert!(r.clocks.contains(&clock(ClockMode::Timestamped)));
}

fn drift(source_ppm: f64, dac_ppm: f64, seed: u64) -> Scenario {
    Scenario {
        seconds: 600.0,
        source_ppm,
        dac_ppm,
        seed,
        settle_s: 60.0,
        ..Scenario::default()
    }
}

#[test]
fn audio_clocks_200_ppm_apart_for_ten_minutes() {
    let r = show(
        "source +200 ppm against the DAC, 10 min",
        &drift(200.0, 0.0, 2),
    );
    assert_eq!((r.muted_after_start, r.replayed), (0, 0));
    assert_eq!((r.stats.resyncs, r.stats.dropouts), (0, 0));
    assert!(
        (r.trim_ppm_mean - 200.0).abs() < 2.0,
        "trim {:.1}",
        r.trim_ppm_mean
    );
    assert!(r.trim_ppm_max <= 1_000.0);
    assert!(r.schedule_us.p(0.99) < 50.0 && r.schedule_us.max < 100.0);
}

#[test]
fn audio_clocks_minus_200_ppm_apart_for_ten_minutes() {
    let r = show(
        "source -100 ppm, DAC +100 ppm, 10 min",
        &drift(-100.0, 100.0, 3),
    );
    assert_eq!((r.muted_after_start, r.replayed), (0, 0));
    assert_eq!((r.stats.resyncs, r.stats.dropouts), (0, 0));
    assert!(
        (r.trim_ppm_mean + 200.0).abs() < 2.0,
        "trim {:.1}",
        r.trim_ppm_mean
    );
    assert!(r.schedule_us.p(0.99) < 50.0 && r.schedule_us.max < 100.0);
}

#[test]
fn bursts_behind_video_within_the_slack_are_inaudible() {
    let r = show(
        "audio held 8 ms of every 11 ms behind video",
        &Scenario {
            head_of_line: Some((11.0, 8.0)),
            ..Scenario::default()
        },
    );
    assert_eq!(r.muted_after_start, 0);
    assert_eq!((r.stats.resyncs, r.stats.dropouts, r.stats.late), (0, 0, 0));
    assert!(r.schedule_us.max < 100.0);
}

#[test]
fn a_burst_larger_than_the_ring_keeps_the_newest_packets() {
    let r = show(
        "one 500 ms stall at 10 s, 64-slot ring",
        &Scenario {
            stalls: vec![(10.0, 500.0)],
            ring_slots: 64,
            ..Scenario::default()
        },
    );
    let s = r.stats;
    assert!(s.evicted > 0, "the burst overflowed the ring");
    assert_eq!(s.dropouts, 1);
    assert_eq!((s.resyncs, r.replayed), (0, 0));
    // 500 ms stall less the slack (40 ms delay - 5 ms packet - ~1.5 ms link - 8 ms output
    // - 3 ms lookahead), plus a fade each side: the newest packets came through on time.
    assert!(
        (460.0..=490.0).contains(&r.longest_mute_ms),
        "{:.1} ms muted",
        r.longest_mute_ms
    );
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
    assert_eq!(r.extra_ms_max, 0.0, "one stall is not a lasting delay");
}

#[test]
fn a_long_stall_fades_out_and_back_in_on_schedule() {
    let r = show(
        "one 120 ms stall at 10 s",
        &Scenario {
            stalls: vec![(10.0, 120.0)],
            ..Scenario::default()
        },
    );
    assert_eq!(r.stats.resyncs, 0, "a dropout never moves the schedule");
    assert_eq!(r.stats.dropouts, 1);
    assert!(r.stats.late > 0, "frames due during the stall are dropped");
    assert!(
        (80.0..=110.0).contains(&r.longest_mute_ms),
        "{:.1} ms muted",
        r.longest_mute_ms
    );
    assert_eq!(r.replayed, 0);
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn datagram_loss_fades_without_moving_the_schedule() {
    let r = show(
        "UDP: 1 % random loss and one 5-packet gap",
        &Scenario {
            ordered: false,
            loss: 0.01,
            loss_runs: vec![(2_000, 5)],
            ..Scenario::default()
        },
    );
    let s = r.stats;
    assert_eq!(s.resyncs, 0);
    assert!(s.lost >= 5);
    assert!(s.dropouts >= 1 && s.dropouts <= s.lost, "{s:?}");
    assert_eq!(r.replayed, 0);
    assert!(r.schedule_us.max < 100.0);
}

#[test]
fn late_datagrams_are_dropped_not_played_late() {
    let r = show(
        "UDP: three packets 60 ms late",
        &Scenario {
            ordered: false,
            late: vec![(1_000, 60.0), (3_000, 60.0), (5_000, 60.0)],
            ..Scenario::default()
        },
    );
    let s = r.stats;
    assert_eq!((s.late, s.dropouts, s.resyncs), (3, 3, 0));
    assert_eq!(r.replayed, 0);
    assert_eq!(r.extra_ms_max, 0.0);
    assert!(r.schedule_us.max < 100.0);
}

#[test]
fn reordered_datagrams_within_the_lead_play_in_place() {
    let r = show(
        "UDP: 5 % of packets held back 12 ms behind later ones",
        &Scenario {
            ordered: false,
            reorder: Some((0.05, 12.0)),
            ..Scenario::default()
        },
    );
    let s = r.stats;
    assert!(s.reordered > 100, "{s:?}");
    assert_eq!((s.late, s.dropouts, s.resyncs, s.stale), (0, 0, 0, 0));
    assert_eq!(r.muted_after_start, 0);
    assert!(r.schedule_us.max < 100.0);
}

#[test]
fn a_new_stream_starts_on_its_own_schedule() {
    let r = show(
        "the Mac starts a new stream at 10 s",
        &Scenario {
            restarts: vec![(10.0, Restart::NewId)],
            ..Scenario::default()
        },
    );
    assert_eq!(r.stats.streams, 2);
    assert_one_seam(&r);
}

/// A new schedule fades the old one out ahead of its last frame and the new one in once its
/// first window is whole: one short dip, never lasting silence, never a replay.
fn assert_one_seam(r: &Report) {
    assert!(r.longest_mute_ms < 7.0, "{:.1} ms muted", r.longest_mute_ms);
    assert!(r.muted_after_start < 7 * 48, "{}", r.muted_after_start);
    assert_eq!((r.muted_in_last_5_s, r.replayed), (0, 0));
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn a_restarted_capture_with_the_same_stream_id_plays_on() {
    let r = show(
        "the capture restarts at 10 s under the same stream id, frames and sequence from 0",
        &Scenario {
            restarts: vec![(10.0, Restart::SameIdFromZero)],
            ..Scenario::default()
        },
    );
    assert_eq!(r.stats.streams, 2);
    assert_one_seam(&r);
}

#[test]
fn a_frame_index_that_jumps_back_restarts_the_schedule() {
    let r = show(
        "the frame index jumps back 1 s at 10 s, times still correct",
        &Scenario {
            restarts: vec![(10.0, Restart::Jump(-48_000))],
            ..Scenario::default()
        },
    );
    assert_eq!(r.stats.streams, 2);
    assert_one_seam(&r);
}

#[test]
fn a_frame_index_that_jumps_ahead_restarts_the_schedule() {
    let r = show(
        "the frame index jumps ahead 5 s at 10 s, times still correct",
        &Scenario {
            restarts: vec![(10.0, Restart::Jump(240_000))],
            ..Scenario::default()
        },
    );
    assert_eq!((r.stats.streams, r.stats.overflow), (2, 0));
    assert_one_seam(&r);
}

#[test]
fn an_output_slower_than_the_cap_plays_late_and_reports_it() {
    for output_ms in [110.0, 180.0, 250.0] {
        let r = show(
            &format!("output latency {output_ms} ms against a 40 ms schedule, 100 ms cap"),
            &Scenario {
                output_latency_ms: output_ms,
                ..Scenario::default()
            },
        );
        assert!(r.first_audible_s.unwrap() < 0.6, "{:?}", r.first_audible_s);
        assert_eq!((r.muted_after_start, r.replayed), (0, 0));
        assert_eq!((r.stats.dropouts, r.stats.late), (0, 0));
        assert!(!r.stats.network_delay_capped);
        // The output's latency + 4 ms callback + 3 ms lookahead + 2 ms margin + 2 ms
        // headroom, less the 33.7 ms notice packets give, plus up to ~3 ms of jitter tails.
        let least = output_ms + 4.0 + 3.04 + 2.0 + 2.0 - 33.7;
        assert!(
            (least..least + 3.5).contains(&r.extra_ms_final),
            "extra delay {:.2} ms, expected {least:.1} plus jitter",
            r.extra_ms_final
        );
        assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
    }
}

#[test]
fn repeated_stalls_beyond_the_slack_raise_the_delay_once() {
    let r = show(
        "25 ms link stall every second",
        &Scenario {
            stall_every: Some((1.0, 25.0)),
            ..Scenario::default()
        },
    );
    assert!(
        r.stats.dropouts <= 3,
        "stalls after the delay rose were heard: {}",
        r.stats.dropouts
    );
    assert!(
        (5.0..=15.0).contains(&r.extra_ms_final),
        "{:.2}",
        r.extra_ms_final
    );
    assert_eq!(r.replayed, 0);
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn network_stalls_beyond_the_cap_are_dropped_not_delayed() {
    let r = show(
        "300 ms link stall every 2 s, 100 ms cap",
        &Scenario {
            stall_every: Some((2.0, 300.0)),
            ..Scenario::default()
        },
    );
    assert!(r.stats.network_delay_capped);
    assert!(
        r.extra_ms_max <= 103.0,
        "network lateness capped: {:.1} ms",
        r.extra_ms_max
    );
    assert_eq!(r.replayed, 0);
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn late_first_timestamps_do_not_inflate_the_delay() {
    for after_s in [0.12, 0.3] {
        let r = show(
            &format!("first output timestamp {after_s} s after the output starts"),
            &Scenario {
                timestamps_after_s: after_s,
                output_latency_ms: 20.0,
                ..Scenario::default()
            },
        );
        assert!(r.extra_ms_max < 3.0, "{:.1} ms", r.extra_ms_max);
        assert_eq!(r.stats.resyncs, 0);
        assert!(r.first_audible_s.unwrap() < after_s + 0.06);
        assert_eq!(r.muted_after_start, 0);
        assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
    }
}

#[test]
fn outlying_timestamps_are_ignored() {
    let r = show(
        "every 37th output timestamp 15 ms off",
        &Scenario {
            timestamp_outliers: Some((37, 15.0)),
            ..Scenario::default()
        },
    );
    assert!(r.stats.timestamps_rejected > 50);
    assert_eq!(
        (r.stats.resyncs, r.stats.dropouts, r.muted_after_start),
        (0, 0, 0)
    );
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn without_timestamps_playout_falls_back_to_the_callback_clock() {
    let r = show(
        "the output never reports a timestamp",
        &Scenario {
            timestamps_until_s: 0.0,
            ..Scenario::default()
        },
    );
    assert_eq!(
        r.clocks,
        BTreeSet::from([clock(ClockMode::Waiting), clock(ClockMode::Estimated)])
    );
    assert!(r.first_audible_s.unwrap() < 0.6, "{:?}", r.first_audible_s);
    assert_eq!(
        (r.muted_after_start, r.replayed, r.stats.dropouts),
        (0, 0, 0)
    );
    // The fallback assumes 10 ms beyond the 8 ms buffer; this output has none, so sound comes
    // a steady 10 ms early.
    let offset = r.schedule_us.mean() / 1e3;
    assert!((offset + 10.0).abs() < 0.3, "{offset:.2} ms");
    assert!(r.schedule_us.p(0.99) < 10_500.0 && r.schedule_us.max < 11_500.0);
}

#[test]
fn timestamps_that_stop_hand_over_without_a_jump() {
    let r = show(
        "output timestamps stop at 10 s",
        &Scenario {
            timestamps_until_s: 10.0,
            ..Scenario::default()
        },
    );
    assert!(r.clocks.contains(&clock(ClockMode::Estimated)));
    assert_eq!(
        (r.stats.resyncs, r.muted_after_start, r.replayed),
        (0, 0, 0)
    );
    assert!(r.schedule_us.max < 1_000.0, "{}", r.schedule_us.max);
}

#[test]
fn an_underrun_moves_the_output_clock_and_playout_follows() {
    let r = show(
        "the DAC underruns for 12 ms at 10 s",
        &Scenario {
            underrun: Some((10.0, 12.0)),
            settle_s: 11.0,
            ..Scenario::default()
        },
    );
    assert_eq!(r.stats.resyncs, 1);
    assert_eq!(r.replayed, 0);
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn a_reopened_output_resumes_on_schedule() {
    let r = show(
        "the output closes at 10 s and reopens 300 ms later",
        &Scenario {
            output_restart: Some((10.0, 300.0)),
            settle_s: 11.0,
            ..Scenario::default()
        },
    );
    assert_eq!(r.stats.output_restarts, 1);
    assert_eq!((r.replayed, r.stats.resyncs), (0, 0));
    // Silent until two of the reopened output's timestamps agree: 10 ms apart, the first
    // one frame period after the first frame is heard.
    assert!(r.longest_mute_ms < 30.0, "{:.1}", r.longest_mute_ms);
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn mapping_steps_are_slewed_or_jumped_never_replayed() {
    for (step_ms, resyncs) in [(2.0, 0), (30.0, 1), (100.0, 1), (-30.0, 2)] {
        let r = show(
            &format!("{step_ms:+} ms step in the clock mapping at 10 s"),
            &Scenario {
                mapping_steps: vec![(10.0, step_ms)],
                settle_s: 20.0,
                ..Scenario::default()
            },
        );
        assert_eq!(r.stats.resyncs, resyncs, "{step_ms} ms");
        assert_eq!(r.replayed, 0, "{step_ms} ms");
        if step_ms < 0.0 {
            // 30 ms less notice leaves 3.7 ms where this output needs about 17: the jump to the
            // earlier schedule is followed by one to a published extra delay.
            assert!(
                (13.0..=18.0).contains(&r.extra_ms_final),
                "{:.2}",
                r.extra_ms_final
            );
        }
        assert!(r.trim_ppm_max <= 1_000.0);
        assert!(
            r.schedule_us.max < 100.0,
            "{step_ms} ms: {}",
            r.schedule_us.max
        );
    }
}

#[test]
fn a_stopped_stream_ends_without_a_dropout() {
    let r = show(
        "the Mac stops the stream at 10 s and starts another at 12 s",
        &Scenario {
            stop: Some((10.0, 12.0)),
            ..Scenario::default()
        },
    );
    assert_eq!(r.stats.streams, 2);
    assert_eq!(
        (r.stats.dropouts, r.stats.silent_frames, r.replayed),
        (0, 0, 0)
    );
    assert!(
        (1_990.0..=2_010.0).contains(&r.longest_mute_ms),
        "{:.1}",
        r.longest_mute_ms
    );
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}

#[test]
fn varying_callback_sizes_keep_the_schedule() {
    let r = show(
        "callback sizes cycling 96, 288, 192, 480, 64, 240, 37",
        &Scenario {
            callback_frames: vec![96, 288, 192, 480, 64, 240, 37],
            ..Scenario::default()
        },
    );
    assert_eq!(
        (r.muted_after_start, r.stats.dropouts, r.stats.resyncs),
        (0, 0, 0)
    );
    assert!(r.schedule_us.max < 100.0, "{}", r.schedule_us.max);
}
