//! What the video path asks of apps and of the encoder: the size apps render each eye at, the
//! size each eye is encoded at, and the bitrate. The measurements behind every number are in
//! docs/ARCHITECTURE.md § Video quality.

use std::ffi::OsStr;

/// Apps render each eye this many times the headset's recommended size per axis, and the
/// compositor filters it down to the encoded size: supersampling, which antialiases whatever the
/// app draws. At 1.4 an M3 Max renders Kenbu in about 1.7 ms more GPU time per frame.
pub const DEFAULT_RENDER_SCALE: f32 = 1.4;
/// Replaces [`DEFAULT_RENDER_SCALE`] for the app it is set for, e.g. `TETHER_RENDER_SCALE=1.0`.
pub const RENDER_SCALE_VAR: &str = "TETHER_RENDER_SCALE";
const RENDER_SCALES: std::ops::RangeInclusive<f32> = 0.5..=2.0;

/// Pixels per second, both eyes together, that VideoToolbox encodes with its p95 latency inside
/// one frame period with room to spare: on an M3 Max with the production settings, 20 s at
/// 90 Hz with the Mac busy, 3552x1856 (592 Mpx/s) gave p95 9.7-9.9 ms against the 11.1 ms
/// period, and 3680x1936 (641 Mpx/s) 10.9-11.0 ms.
pub const ENCODE_PIXEL_RATE: f64 = 600e6;

/// Over USB the link carries about 2,000 Mbps; the Quest 3's HEVC decoder is rated to 160.
pub const USB_BITRATE: u32 = 150_000_000;

/// Every encoded and recommended eye dimension is a multiple of this, so HEVC codes whole
/// blocks and the headset's decoder hands back exactly the picture, with no conformance crop.
pub const ALIGN: u32 = 16;

/// The render scale `var` asks for, if it parses and lies in 0.5..=2.0; otherwise the default.
pub fn render_scale(var: Option<&OsStr>) -> f32 {
    var.and_then(OsStr::to_str)
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|s| RENDER_SCALES.contains(s))
        .unwrap_or(DEFAULT_RENDER_SCALE)
}

/// The size to recommend apps render each eye at: the headset's recommended size times `scale`,
/// rounded to [`ALIGN`].
pub fn render_eye(recommended: (u32, u32), scale: f32) -> (u32, u32) {
    let round = |v: u32| {
        let scaled = (f64::from(v) * f64::from(scale) / f64::from(ALIGN)).round() as u32;
        scaled.max(1) * ALIGN
    };
    (round(recommended.0), round(recommended.1))
}

/// The size each eye is encoded at when the app renders `app`: its own size, or, when that is
/// more than the encoder sustains at `refresh_hz`, the largest size with the same aspect that it
/// does. Never larger than the app's eye, and rounded down to [`ALIGN`].
pub fn encode_eye(app: (u32, u32), refresh_hz: f32) -> (u32, u32) {
    let per_eye = ENCODE_PIXEL_RATE / 2.0 / f64::from(refresh_hz.max(1.0));
    let (w, h) = (f64::from(app.0.max(1)), f64::from(app.1.max(1)));
    let fit = (per_eye / (w * h)).sqrt().min(1.0);
    let down = |v: f64| ((v * fit) as u32 / ALIGN).max(1) * ALIGN;
    (down(w), down(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUEST3: (u32, u32) = (1680, 1760);

    #[test]
    fn apps_render_at_the_default_scale_unless_told_otherwise() {
        assert_eq!(render_scale(None), DEFAULT_RENDER_SCALE);
        assert_eq!(render_scale(Some(OsStr::new("1.0"))), 1.0);
        assert_eq!(render_scale(Some(OsStr::new(" 1.25 "))), 1.25);
        for bad in ["", "big", "0.2", "3", "NaN", "inf"] {
            assert_eq!(
                render_scale(Some(OsStr::new(bad))),
                DEFAULT_RENDER_SCALE,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_recommended_size_is_scaled_and_aligned() {
        assert_eq!(render_eye(QUEST3, 1.0), QUEST3);
        assert_eq!(render_eye(QUEST3, 1.4), (2352, 2464));
        assert_eq!(render_eye(QUEST3, 1.25), (2096, 2208));
        assert_eq!(render_eye((1, 1), 1.0), (ALIGN, ALIGN));
    }

    #[test]
    fn the_supersampled_quest3_eye_is_encoded_within_the_frame_budget() {
        let app = render_eye(QUEST3, DEFAULT_RENDER_SCALE);
        let eye = encode_eye(app, 90.0);
        assert_eq!(eye, (1776, 1856));
        let rate = f64::from(eye.0 * 2 * eye.1) * 90.0;
        assert!(rate <= ENCODE_PIXEL_RATE, "{rate}");
    }

    #[test]
    fn an_eye_the_encoder_sustains_is_encoded_as_rendered() {
        assert_eq!(encode_eye(QUEST3, 90.0), QUEST3);
        assert_eq!(encode_eye((1680, 1762), 90.0), QUEST3);
    }

    #[test]
    fn a_faster_headset_gets_a_smaller_encode() {
        let app = render_eye(QUEST3, DEFAULT_RENDER_SCALE);
        let (w90, h90) = encode_eye(app, 90.0);
        let (w120, h120) = encode_eye(app, 120.0);
        assert!(w120 < w90 && h120 < h90);
        assert!(f64::from(w120 * 2 * h120) * 120.0 <= ENCODE_PIXEL_RATE);
        let (w72, h72) = encode_eye(app, 72.0);
        assert!(w72 > w90 && h72 > h90);
    }

    #[test]
    fn every_encoded_size_is_aligned_and_never_larger_than_the_app() {
        for app in [
            (17, 9),
            (1000, 3000),
            (4096, 4096),
            (2352, 2464),
            (2100, 2200),
        ] {
            for hz in [60.0, 72.0, 90.0, 120.0, 0.0] {
                let (w, h) = encode_eye(app, hz);
                assert_eq!((w % ALIGN, h % ALIGN), (0, 0), "{app:?} at {hz}");
                assert!(w >= ALIGN && h >= ALIGN, "{app:?} at {hz}");
                assert!(
                    (w <= app.0.max(ALIGN)) && (h <= app.1.max(ALIGN)),
                    "{app:?} at {hz}"
                );
            }
        }
    }
}
