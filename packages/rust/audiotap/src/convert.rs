//! Float samples to the wire's signed 16-bit PCM. Full scale is 32768, the scale the Quest
//! client divides by.

/// Converts `src` into the front of `dst`, rounding to nearest. Out-of-range samples saturate
/// and NaN becomes silence (Rust's float-to-int casts saturate). Real-time safe.
pub fn f32_to_s16(src: &[f32], dst: &mut [i16]) {
    for (out, &sample) in dst.iter_mut().zip(src) {
        *out = (sample * 32768.0).round() as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sample: f32) -> i16 {
        let mut out = [0i16];
        f32_to_s16(&[sample], &mut out);
        out[0]
    }

    #[test]
    fn full_scale_and_rounding() {
        assert_eq!(one(0.0), 0);
        assert_eq!(one(-0.0), 0);
        assert_eq!(one(0.5), 16384);
        assert_eq!(one(-0.5), -16384);
        assert_eq!(one(-1.0), -32768);
        assert_eq!(one(1.0), 32767);
        assert_eq!(one(1.0 / 32768.0), 1);
        assert_eq!(one(0.4 / 32768.0), 0);
        assert_eq!(one(0.6 / 32768.0), 1);
        assert_eq!(one(-0.6 / 32768.0), -1);
    }

    #[test]
    fn saturates_and_silences_nan() {
        assert_eq!(one(2.0), i16::MAX);
        assert_eq!(one(-2.0), i16::MIN);
        assert_eq!(one(f32::INFINITY), i16::MAX);
        assert_eq!(one(f32::NEG_INFINITY), i16::MIN);
        assert_eq!(one(f32::NAN), 0);
    }

    #[test]
    fn round_trip_error_is_at_most_half_a_step() {
        for i in -1000..=1000 {
            let sample = i as f32 / 1000.0 * 0.999;
            let back = one(sample) as f32 / 32768.0;
            assert!(
                (back - sample).abs() <= 0.5 / 32768.0 + f32::EPSILON,
                "{sample}"
            );
        }
    }

    #[test]
    fn converts_only_what_fits() {
        let mut dst = [7i16; 2];
        f32_to_s16(&[0.5, 0.5, 0.5], &mut dst);
        assert_eq!(dst, [16384, 16384]);
        let mut dst = [7i16; 3];
        f32_to_s16(&[0.5], &mut dst);
        assert_eq!(dst, [16384, 7, 7]);
    }
}
