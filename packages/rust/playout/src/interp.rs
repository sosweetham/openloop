//! Band-limited reads at fractional frame positions, for the small speed changes that absorb
//! clock drift: a Kaiser-windowed sinc tabulated at `PHASES` sub-frame offsets and blended
//! linearly between neighbouring offsets. Not a general rate converter: below 1:1 it does not
//! narrow its passband, which is why `openloopd` sends audio at the headset's own rate.

pub(crate) const TAPS: usize = 32;
pub(crate) const HALF_TAPS: usize = TAPS / 2;
const PHASES: usize = 512;
const KAISER_BETA: f64 = 8.5;
/// Fraction of the Nyquist frequency passed; the rest is the window's transition band.
const CUTOFF: f64 = 0.92;

pub(crate) struct Interpolator {
    /// Row `p` reads at offset `p / PHASES`; `PHASES + 1` rows so every blend has a neighbour.
    table: Box<[[f32; TAPS]]>,
}

impl Interpolator {
    pub(crate) fn new() -> Self {
        let table = (0..=PHASES)
            .map(|phase| {
                let offset = phase as f64 / PHASES as f64;
                let mut row = [0.0f64; TAPS];
                for (k, c) in row.iter_mut().enumerate() {
                    let x = k as f64 - (HALF_TAPS as f64 - 1.0) - offset;
                    *c = CUTOFF * sinc(CUTOFF * x) * kaiser(x / HALF_TAPS as f64);
                }
                // Unity gain at DC for every offset, so a constant signal never ripples as the
                // read position slides between frames.
                let sum: f64 = row.iter().sum();
                row.map(|c| (c / sum) as f32)
            })
            .collect();
        Self { table }
    }

    /// `window[k]` is the frame at `base + k - (HALF_TAPS - 1)`; returns the value at
    /// `base + offset` for `0 <= offset < 1`.
    #[inline]
    pub(crate) fn read(&self, window: &[f32; TAPS], offset: f64) -> f32 {
        let scaled = offset * PHASES as f64;
        let phase = (scaled as usize).min(PHASES - 1);
        let blend = (scaled - phase as f64) as f32;
        let (a, b) = (&self.table[phase], &self.table[phase + 1]);
        let (mut ya, mut yb) = (0.0f32, 0.0f32);
        for k in 0..TAPS {
            ya += window[k] * a[k];
            yb += window[k] * b[k];
        }
        ya + blend * (yb - ya)
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

fn kaiser(t: f64) -> f64 {
    if t.abs() >= 1.0 {
        return 0.0;
    }
    bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / bessel_i0(KAISER_BETA)
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

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Signal-to-error of a tone read at the speed a 200 ppm drift asks for: the read phase
    /// sweeps every sub-frame offset, which is where a cheap interpolator modulates the treble.
    fn snr_db(hz: f64) -> f64 {
        const RATE: f64 = 48_000.0;
        const RATIO: f64 = 1.0002;
        let interp = Interpolator::new();
        let source: Vec<f32> = (0..60_000)
            .map(|n| (std::f64::consts::TAU * hz * n as f64 / RATE).sin() as f32 * 0.5)
            .collect();
        let (mut signal, mut noise) = (0.0f64, 0.0f64);
        for i in 0..40_000 {
            let position = 5_000.0 + i as f64 * RATIO;
            let base = position.floor() as usize;
            let window: &[f32; TAPS] = source[base + 1 - HALF_TAPS..][..TAPS].try_into().unwrap();
            let got = f64::from(interp.read(window, position - base as f64));
            let want = (std::f64::consts::TAU * hz * position / RATE).sin() * 0.5;
            signal += want * want;
            noise += (got - want) * (got - want);
        }
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn stays_near_sixteen_bit_quality_to_eighteen_kilohertz() {
        for (hz, floor) in [
            (1_000.0, 90.0),
            (5_000.0, 90.0),
            (10_000.0, 90.0),
            (15_000.0, 88.0),
            (18_000.0, 85.0),
        ] {
            let snr = snr_db(hz);
            assert!(snr > floor, "{hz} Hz: {snr:.1} dB");
        }
    }
}
