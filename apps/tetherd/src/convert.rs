//! Puts captured audio into the headset's format: its rate and channel count, which are the only
//! ones the protocol carries, so the headset's own resampler only trims clock drift
//! (docs/ARCHITECTURE.md § Audio). A rate change goes through a Kaiser-windowed sinc band-limited
//! to the lower rate. Output frames count from the stream's first frame, gaps scaled with it.

use std::f64::consts::PI;

use tether_protocol::MAX_AUDIO_FRAMES;

/// The tap's stereo mixdown.
const IN_CHANNELS: usize = 2;
/// Sinc zero crossings on each side of the interpolated point.
const ZERO_CROSSINGS: f64 = 16.0;
/// Kernel table points per input frame.
const RESOLUTION: f64 = 256.0;
const KAISER_BETA: f64 = 8.0;
/// The passband edge, as a fraction of the lower rate's Nyquist frequency.
const PASSBAND: f64 = 0.92;

/// One packet in the headset's format.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Converted<'a> {
    /// Counted at the output rate from the stream's first frame.
    pub first_frame: u64,
    /// `XrTime` at which that frame was captured.
    pub host_time_ns: i64,
    /// Interleaved, the output's channel count per frame.
    pub samples: &'a [i16],
}

pub struct Converter {
    channels: usize,
    /// The source frame output frame 0 is.
    origin: u64,
    resampler: Option<Resampler>,
    mixed: Vec<i16>,
}

impl Converter {
    /// For a stream whose first packet starts at source frame `first_frame`. `out_channels` is
    /// 1 or 2; `frames_per_packet` is the source's packet size, kept in duration.
    pub fn new(
        in_rate: u32,
        out_rate: u32,
        out_channels: u8,
        frames_per_packet: usize,
        first_frame: u64,
    ) -> Converter {
        let channels = usize::from(out_channels.clamp(1, 2));
        let resampler = (in_rate != out_rate).then(|| {
            let frames = (frames_per_packet as f64 * f64::from(out_rate) / f64::from(in_rate))
                .round()
                .clamp(1.0, MAX_AUDIO_FRAMES as f64) as usize;
            Resampler::new(in_rate, out_rate, channels, frames, first_frame)
        });
        Converter {
            channels,
            origin: first_frame,
            resampler,
            mixed: Vec::with_capacity(MAX_AUDIO_FRAMES * channels),
        }
    }

    /// Converts one source packet of stereo frames, emitting every output packet it completes.
    pub fn push(
        &mut self,
        first_frame: u64,
        host_time_ns: i64,
        samples: &[i16],
        emit: &mut impl FnMut(Converted<'_>),
    ) {
        match &mut self.resampler {
            Some(resampler) => resampler.push(first_frame, host_time_ns, samples, emit),
            None => {
                let first_frame = first_frame.saturating_sub(self.origin);
                let samples = if self.channels == IN_CHANNELS {
                    samples
                } else {
                    self.mixed.clear();
                    self.mixed
                        .extend(samples.chunks_exact(IN_CHANNELS).map(downmix));
                    &self.mixed
                };
                emit(Converted {
                    first_frame,
                    host_time_ns,
                    samples,
                });
            }
        }
    }

    /// Emits what the stream's last packets left, as if silence followed them.
    pub fn finish(&mut self, emit: &mut impl FnMut(Converted<'_>)) {
        if let Some(resampler) = &mut self.resampler {
            resampler.finish(emit);
        }
    }
}

fn downmix(frame: &[i16]) -> i16 {
    ((i32::from(frame[0]) + i32::from(frame[1])) / 2) as i16
}

struct Resampler {
    in_rate: f64,
    /// Input frames per output frame.
    step: f64,
    channels: usize,
    half_width: i64,
    /// The kernel from 0 to `half_width` input frames, [`RESOLUTION`] points per frame.
    kernel: Vec<f64>,
    /// Input frames from source frame `base` on.
    input: Vec<[f32; IN_CHANNELS]>,
    base: i64,
    /// One past the last source frame received.
    end: i64,
    /// Output frame `origin_out` falls on source frame `origin_in`.
    origin_in: i64,
    origin_out: u64,
    /// The stream's first source frame, output frame 0.
    first: i64,
    next_out: u64,
    /// `(source frame, XrTime)` of the latest packet's first frame.
    anchor: (i64, i64),
    packet_frames: usize,
    pending: Vec<i16>,
    pending_first: u64,
    pending_host_ns: i64,
}

impl Resampler {
    fn new(
        in_rate: u32,
        out_rate: u32,
        channels: usize,
        packet_frames: usize,
        first_frame: u64,
    ) -> Resampler {
        let (in_rate, out_rate) = (f64::from(in_rate.max(1)), f64::from(out_rate.max(1)));
        // Cycles per input frame.
        let cutoff = 0.5 * PASSBAND * (out_rate / in_rate).min(1.0);
        let half_width = (ZERO_CROSSINGS / (2.0 * cutoff)).ceil();
        let points = (half_width * RESOLUTION) as usize + 2;
        let kernel = (0..points)
            .map(|i| {
                let t = i as f64 / RESOLUTION;
                if t >= half_width {
                    return 0.0;
                }
                let x = 2.0 * cutoff * t;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (PI * x).sin() / (PI * x)
                };
                2.0 * cutoff * sinc * kaiser(t / half_width)
            })
            .collect();
        let first = first_frame as i64;
        let mut resampler = Resampler {
            in_rate,
            step: in_rate / out_rate,
            channels,
            half_width: half_width as i64,
            kernel,
            input: Vec::new(),
            base: first,
            end: first,
            origin_in: first,
            origin_out: 0,
            first,
            next_out: 0,
            anchor: (first, 0),
            packet_frames,
            pending: Vec::with_capacity(packet_frames * channels),
            pending_first: 0,
            pending_host_ns: 0,
        };
        resampler.restart(first);
        resampler
    }

    /// Silence before `frame`, so its first outputs have a whole kernel to read.
    fn restart(&mut self, frame: i64) {
        self.input.clear();
        self.input
            .resize(self.half_width as usize, [0.0; IN_CHANNELS]);
        self.base = frame - self.half_width;
        self.end = frame;
        self.origin_in = frame;
        let scaled = ((frame - self.first) as f64 / self.step).round().max(0.0) as u64;
        self.origin_out = scaled.max(self.next_out);
        self.next_out = self.origin_out;
    }

    fn push(
        &mut self,
        first_frame: u64,
        host_time_ns: i64,
        samples: &[i16],
        emit: &mut impl FnMut(Converted<'_>),
    ) {
        let first = first_frame as i64;
        if first != self.end {
            // A gap: finish the frames before it, then start again at this packet.
            self.produce(true, emit);
            self.flush(emit);
            self.restart(first);
        }
        self.anchor = (first, host_time_ns);
        self.input.extend(
            samples
                .chunks_exact(IN_CHANNELS)
                .map(|f| [f32::from(f[0]) / 32768.0, f32::from(f[1]) / 32768.0]),
        );
        self.end = first + (samples.len() / IN_CHANNELS) as i64;
        self.produce(false, emit);
    }

    fn finish(&mut self, emit: &mut impl FnMut(Converted<'_>)) {
        self.produce(true, emit);
        self.flush(emit);
    }

    fn position(&self, out: u64) -> f64 {
        self.origin_in as f64 + (out - self.origin_out) as f64 * self.step
    }

    /// Interpolates every output frame whose kernel the input covers; `draining` reads silence
    /// past the input's end instead of waiting for more.
    fn produce(&mut self, draining: bool, emit: &mut impl FnMut(Converted<'_>)) {
        loop {
            let position = self.position(self.next_out);
            let center = position.floor() as i64;
            let ready = if draining {
                center < self.end
            } else {
                center + self.half_width < self.end
            };
            if !ready {
                break;
            }
            let frame = self.interpolate(position);
            if self.pending.is_empty() {
                self.pending_first = self.next_out;
                let since_anchor = position - self.anchor.0 as f64;
                self.pending_host_ns =
                    self.anchor.1 + (since_anchor * 1e9 / self.in_rate).round() as i64;
            }
            if self.channels == IN_CHANNELS {
                self.pending.extend(frame.map(to_s16));
            } else {
                self.pending.push(to_s16((frame[0] + frame[1]) / 2.0));
            }
            self.next_out += 1;
            if self.pending.len() == self.packet_frames * self.channels {
                self.flush(emit);
            }
        }
        // Keep what the next output's kernel reaches back to.
        let keep_from = self.position(self.next_out).floor() as i64 - self.half_width;
        let drop = (keep_from - self.base).clamp(0, self.input.len() as i64) as usize;
        if drop > 0 {
            self.input.drain(..drop);
            self.base += drop as i64;
        }
    }

    fn flush(&mut self, emit: &mut impl FnMut(Converted<'_>)) {
        if !self.pending.is_empty() {
            emit(Converted {
                first_frame: self.pending_first,
                host_time_ns: self.pending_host_ns,
                samples: &self.pending,
            });
            self.pending.clear();
        }
    }

    fn interpolate(&self, position: f64) -> [f64; IN_CHANNELS] {
        let center = position.floor() as i64;
        let (mut left, mut right, mut weights) = (0.0, 0.0, 0.0);
        for frame in center - self.half_width + 1..=center + self.half_width {
            let weight = self.weight((position - frame as f64).abs());
            weights += weight;
            let index = frame - self.base;
            if let Some(&[l, r]) = usize::try_from(index).ok().and_then(|i| self.input.get(i)) {
                left += weight * f64::from(l);
                right += weight * f64::from(r);
            }
        }
        // Normalising keeps the gain at exactly 1 whatever the fractional position.
        let gain = if weights.abs() > 1e-9 {
            1.0 / weights
        } else {
            0.0
        };
        [left * gain, right * gain]
    }

    fn weight(&self, distance: f64) -> f64 {
        let at = distance * RESOLUTION;
        let i = at.floor() as usize;
        let frac = at - i as f64;
        match (self.kernel.get(i), self.kernel.get(i + 1)) {
            (Some(a), Some(b)) => a + (b - a) * frac,
            _ => 0.0,
        }
    }
}

fn to_s16(value: f64) -> i16 {
    (value * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// The Kaiser window at `r` in [0, 1] from its centre.
fn kaiser(r: f64) -> f64 {
    bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(KAISER_BETA)
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, half) = (1.0, 1.0, x / 2.0);
    for k in 1..64 {
        term *= half / k as f64;
        sum += term * term;
        if term * term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use tether_audiotap::analysis::tone_stats;

    /// Stereo frames of a tone, the same on both channels.
    fn tone(rate: u32, hz: f64, amplitude: f64, frames: usize) -> Vec<i16> {
        (0..frames)
            .flat_map(|n| {
                let v = (amplitude * (n as f64 * hz / f64::from(rate) * 2.0 * PI).sin() * 32768.0)
                    .round() as i16;
                [v, v]
            })
            .collect()
    }

    /// Output packets as `(first_frame, host_time_ns, samples)`.
    fn convert(
        converter: &mut Converter,
        packets: &[(u64, i64, Vec<i16>)],
    ) -> Vec<(u64, i64, Vec<i16>)> {
        let mut out = Vec::new();
        let mut emit =
            |c: Converted<'_>| out.push((c.first_frame, c.host_time_ns, c.samples.to_vec()));
        for (first, host, samples) in packets {
            converter.push(*first, *host, samples, &mut emit);
        }
        converter.finish(&mut emit);
        out
    }

    /// `frames` per packet of `samples`, stamped as captured from `start_ns` at `rate`.
    fn packets(
        samples: &[i16],
        frames: usize,
        rate: u32,
        start_ns: i64,
    ) -> Vec<(u64, i64, Vec<i16>)> {
        samples
            .chunks(frames * 2)
            .enumerate()
            .map(|(i, chunk)| {
                let first = (i * frames) as u64;
                let host = start_ns + (first as f64 * 1e9 / f64::from(rate)).round() as i64;
                (first, host, chunk.to_vec())
            })
            .collect()
    }

    fn joined(out: &[(u64, i64, Vec<i16>)]) -> Vec<i16> {
        out.iter().flat_map(|p| p.2.iter().copied()).collect()
    }

    #[test]
    fn the_same_rate_passes_through_untouched() {
        let input = tone(48_000, 440.0, 0.25, 960);
        let mut converter = Converter::new(48_000, 48_000, 2, 240, 0);
        let out = convert(&mut converter, &packets(&input, 240, 48_000, 7));
        assert_eq!(out.len(), 4);
        assert_eq!(joined(&out), input);
        assert_eq!(out[1].0, 240);
        assert_eq!(out[1].1, 7 + 5_000_000);
    }

    #[test]
    fn a_mono_headset_gets_the_average_of_both_channels() {
        let mut converter = Converter::new(48_000, 48_000, 1, 240, 480);
        let out = convert(&mut converter, &[(480, 0, vec![100, 300, -5, -7])]);
        assert_eq!(out, vec![(0, 0, vec![200, -6])]);
    }

    #[test]
    fn upsampling_keeps_the_tone_its_level_and_its_timing() {
        let input = tone(44_100, 1_000.0, 0.5, 44_100);
        let mut converter = Converter::new(44_100, 48_000, 2, 240, 0);
        let start_ns = 1_000_000_000;
        let out = convert(&mut converter, &packets(&input, 240, 44_100, start_ns));
        let samples = joined(&out);
        let frames = samples.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames} frames");
        let stats = tone_stats(&samples[4_800..samples.len() - 4_800], 0, 48_000);
        assert!((stats.frequency_hz - 1_000.0).abs() < 0.5, "{stats:?}");
        assert!((stats.rms - 0.5 / 2f64.sqrt()).abs() < 0.002, "{stats:?}");
        for (first, host, samples) in &out[..out.len() - 1] {
            assert_eq!(
                samples.len(),
                261 * 2,
                "the source's 240-frame packets, in duration"
            );
            let expected = start_ns + (*first as f64 * 1e9 / 48_000.0).round() as i64;
            assert!(
                (host - expected).abs() <= 1,
                "frame {first}: {host} vs {expected}"
            );
        }
        for pair in out.windows(2) {
            assert_eq!(
                pair[1].0,
                pair[0].0 + (pair[0].2.len() / 2) as u64,
                "contiguous"
            );
        }
    }

    #[test]
    fn downsampling_removes_what_the_lower_rate_cannot_carry() {
        // 30 kHz cannot exist at 48 kHz; unfiltered it would fold down to 18 kHz.
        let input = tone(96_000, 30_000.0, 0.5, 48_000);
        let mut converter = Converter::new(96_000, 48_000, 2, 480, 0);
        let out = convert(&mut converter, &packets(&input, 480, 96_000, 0));
        let samples = joined(&out);
        let stats = tone_stats(&samples[2_000..samples.len() - 2_000], 0, 48_000);
        assert!(stats.rms < 0.005, "{stats:?}");

        let input = tone(96_000, 1_000.0, 0.5, 48_000);
        let mut converter = Converter::new(96_000, 48_000, 2, 480, 0);
        let samples = joined(&convert(&mut converter, &packets(&input, 480, 96_000, 0)));
        let stats = tone_stats(&samples[2_000..samples.len() - 2_000], 0, 48_000);
        assert!((stats.rms - 0.5 / 2f64.sqrt()).abs() < 0.002, "{stats:?}");
    }

    #[test]
    fn a_gap_in_the_source_is_a_gap_in_the_output() {
        let input = tone(44_100, 440.0, 0.25, 4_410);
        let mut source = packets(&input, 441, 44_100, 0);
        // Lose the fourth packet (10 ms).
        source.remove(3);
        let mut converter = Converter::new(44_100, 48_000, 2, 441, 0);
        let out = convert(&mut converter, &source);
        let jump = out
            .windows(2)
            .map(|pair| pair[1].0 - (pair[0].0 + (pair[0].2.len() / 2) as u64))
            .find(|&skipped| skipped > 0);
        assert_eq!(
            jump,
            Some(480),
            "{:?}",
            out.iter().map(|p| p.0).collect::<Vec<_>>()
        );
    }
}
