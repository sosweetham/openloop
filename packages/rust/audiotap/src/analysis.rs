//! Measurements of captured 16-bit PCM, for the live capture check and the tests.

use crate::ring::CHANNELS;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ToneStats {
    pub frames: usize,
    /// Full scale is 1.0.
    pub rms: f64,
    pub peak: f64,
    /// From interpolated rising zero crossings; 0 with fewer than two.
    pub frequency_hz: f64,
}

/// Measures one channel of interleaved stereo.
pub fn tone_stats(samples: &[i16], channel: usize, sample_rate: u32) -> ToneStats {
    let values: Vec<f64> = samples
        .chunks_exact(CHANNELS)
        .map(|frame| f64::from(frame[channel.min(CHANNELS - 1)]) / 32768.0)
        .collect();
    if values.is_empty() {
        return ToneStats::default();
    }
    let rms = (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt();
    let peak = values.iter().fold(0.0f64, |peak, v| peak.max(v.abs()));
    let crossings: Vec<f64> = values
        .windows(2)
        .enumerate()
        .filter(|(_, pair)| pair[0] < 0.0 && pair[1] >= 0.0)
        .map(|(i, pair)| i as f64 + pair[0] / (pair[0] - pair[1]))
        .collect();
    let frequency_hz = match (crossings.first(), crossings.last()) {
        (Some(first), Some(last)) if crossings.len() >= 2 => {
            (crossings.len() - 1) as f64 * f64::from(sample_rate) / (last - first)
        }
        _ => 0.0,
    };
    ToneStats {
        frames: values.len(),
        rms,
        peak,
        frequency_hz,
    }
}

/// The first frame with a sample on either channel above `threshold` (full scale 1.0).
pub fn first_loud_frame(samples: &[i16], threshold: f64) -> Option<usize> {
    let limit = threshold * 32768.0;
    samples
        .chunks_exact(CHANNELS)
        .position(|frame| frame.iter().any(|&s| f64::from(s).abs() > limit))
}

/// A least-squares line through `(frame index, XrTime)` pairs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockFit {
    /// The source's rate measured on the Mac clock.
    pub rate_hz: f64,
    /// How far the furthest point lies from the line.
    pub max_residual_ns: f64,
}

pub fn fit_clock(points: &[(u64, i64)]) -> Option<ClockFit> {
    let &(x0, y0) = points.first()?;
    let n = points.len() as f64;
    let centered: Vec<(f64, f64)> = points
        .iter()
        .map(|&(x, y)| ((x - x0) as f64, (y - y0) as f64))
        .collect();
    let (mean_x, mean_y) = centered
        .iter()
        .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x / n, sy + y / n));
    let (sxx, sxy) = centered.iter().fold((0.0, 0.0), |(sxx, sxy), &(x, y)| {
        (
            sxx + (x - mean_x) * (x - mean_x),
            sxy + (x - mean_x) * (y - mean_y),
        )
    });
    if sxx == 0.0 {
        return None;
    }
    let ns_per_frame = sxy / sxx;
    let max_residual_ns = centered
        .iter()
        .map(|&(x, y)| (y - mean_y - ns_per_frame * (x - mean_x)).abs())
        .fold(0.0, f64::max);
    Some(ClockFit {
        rate_hz: 1e9 / ns_per_frame,
        max_residual_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f64, amplitude: f64, rate: u32, frames: usize) -> Vec<i16> {
        (0..frames)
            .flat_map(|n| {
                let v = (amplitude
                    * (n as f64 * frequency / f64::from(rate) * std::f64::consts::TAU).sin()
                    * 32768.0)
                    .round() as i16;
                [v, -v]
            })
            .collect()
    }

    #[test]
    fn measures_a_known_tone() {
        let stats = tone_stats(&sine(997.0, 0.25, 48_000, 48_000), 0, 48_000);
        assert_eq!(stats.frames, 48_000);
        assert!((stats.frequency_hz - 997.0).abs() < 0.05, "{stats:?}");
        assert!((stats.rms - 0.25 / 2f64.sqrt()).abs() < 1e-4, "{stats:?}");
        assert!((stats.peak - 0.25).abs() < 1e-3, "{stats:?}");
    }

    #[test]
    fn silence_has_no_frequency() {
        let stats = tone_stats(&[0; 200], 1, 48_000);
        assert_eq!((stats.rms, stats.frequency_hz), (0.0, 0.0));
        assert_eq!(tone_stats(&[], 0, 48_000), ToneStats::default());
    }

    #[test]
    fn finds_the_onset() {
        let mut samples = vec![0i16; 2 * 100];
        samples.extend(sine(440.0, 0.5, 48_000, 100));
        assert_eq!(first_loud_frame(&samples, 0.01), Some(101));
        assert_eq!(first_loud_frame(&samples[..200], 0.01), None);
    }

    #[test]
    fn fits_rate_and_jitter() {
        let points: Vec<(u64, i64)> = (0..100u64)
            .map(|i| {
                let frame = 1_000 + i * 512;
                let jitter = if i == 50 { 3_000 } else { 0 };
                (
                    frame,
                    5_000_000_000 + (frame as f64 * 1e9 / 48_048.0) as i64 + jitter,
                )
            })
            .collect();
        let fit = fit_clock(&points).unwrap();
        assert!((fit.rate_hz - 48_048.0).abs() < 0.5, "{fit:?}");
        assert!((fit.max_residual_ns - 3_000.0).abs() < 100.0, "{fit:?}");
        assert_eq!(fit_clock(&[(5, 5)]), None);
        assert_eq!(fit_clock(&[]), None);
    }
}
