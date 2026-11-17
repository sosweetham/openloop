//! `tether-fake-headset`: a fake Quest for a Tether runtime on this Mac. See `USAGE`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use tether_fake_headset::{Config, Motion, parse_cue, run};

const USAGE: &str = "\
usage: tether-fake-headset [--port PORT] [--seconds N] [--report FILE] [options]

Connects to Tether on 127.0.0.1:PORT as a Quest 3 would, streams tracking at the refresh rate,
checks the video, audio and haptics that come back, and writes a JSON report.

  --port PORT            the Mac's port, else TETHER_USB_PORT. One of them is required: the
                         default port belongs to the real headset, whose link this would replace.
  --seconds N            how long to stay connected (default 10)
  --report FILE          where the report goes (default stdout)
  --motion still|sway    how the head and controllers move (default sway)
  --at SECONDS:ACTION    a cue, repeatable. ACTION is recenter, keyframe, focus, unfocus,
                         pause:MS, press:HAND:BUTTON[:MS], trigger:HAND:VALUE,
                         squeeze:HAND:VALUE or stick:HAND:X:Y; HAND is left or right; BUTTON is
                         primary, secondary, thumbstick, menu, trigger, squeeze or thumbrest
  --clock-offset-ms MS   the headset clock's offset from CLOCK_MONOTONIC (default 10000)
  --drift-ppm PPM        how much faster the headset clock runs (default 50)
  --eye WxH              recommended per-eye size (default 1680x1760)
  --refresh HZ           refresh and tracking rate (default 90)
  --h264                 announce no HEVC decoder
  --no-audio             announce no audio output
  --no-keyframe-request  do not ask for a keyframe when a stream starts
  --connect-timeout S    how long to retry the first connection (default 10)
  --strict               exit 1 when the report has errors

Exit status: 0 once a session has run and its report is written; 1 when it never connected
(the report says why), the report could not be written, or --strict found errors; 2 for bad
arguments.";

const PORT_VAR: &str = "TETHER_USB_PORT";

#[derive(Debug)]
struct Options {
    config: Config,
    seconds: f64,
    report: Option<PathBuf>,
    strict: bool,
}

fn parse(
    args: impl IntoIterator<Item = String>,
    port_var: Option<OsString>,
) -> Result<Options, String> {
    let mut port = None;
    let mut config = Config::new(0);
    let mut seconds = 10.0;
    let mut report = None;
    let mut strict = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        let number = |text: String, what: &str| {
            text.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or(format!("{what} needs a number, not {text:?}"))
        };
        match arg.as_str() {
            "--port" => {
                let text = value()?;
                port = Some(parse_port(&text).ok_or(format!("--port needs a port, not {text:?}"))?);
            }
            "--seconds" => seconds = number(value()?, "--seconds")?.max(0.0),
            "--report" => report = Some(PathBuf::from(value()?)),
            "--motion" => {
                config.script.motion = match value()?.as_str() {
                    "still" => Motion::Still,
                    "sway" => Motion::Sway,
                    other => return Err(format!("--motion is still or sway, not {other:?}")),
                }
            }
            "--at" => config.script.cues.push(parse_cue(&value()?)?),
            "--clock-offset-ms" => {
                config.clock.offset_ns = (number(value()?, "--clock-offset-ms")? * 1e6) as i64;
            }
            "--drift-ppm" => config.clock.drift_ppm = number(value()?, "--drift-ppm")?,
            "--eye" => {
                let text = value()?;
                let (w, h) = text
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .ok_or(format!("--eye is WIDTHxHEIGHT, not {text:?}"))?;
                (config.profile.eye_width, config.profile.eye_height) = (w, h);
            }
            "--refresh" => {
                let hz = number(value()?, "--refresh")? as f32;
                if hz < 1.0 {
                    return Err("--refresh needs at least 1 Hz".into());
                }
                config.profile.refresh_rate = hz;
                if !config.profile.refresh_rates.contains(&hz) {
                    config.profile.refresh_rates.push(hz);
                }
            }
            "--h264" => config.profile.supports_hevc = false,
            "--no-audio" => {
                (
                    config.profile.audio_sample_rate,
                    config.profile.audio_channels,
                ) = (0, 0)
            }
            "--no-keyframe-request" => config.keyframe_request_on_stream_start = false,
            "--connect-timeout" => {
                config.connect_timeout =
                    Duration::from_secs_f64(number(value()?, "--connect-timeout")?.max(0.0));
            }
            "--strict" => strict = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    config.port = match (port, port_var.filter(|v| !v.is_empty())) {
        (Some(port), _) => port,
        (None, Some(var)) => var
            .to_str()
            .and_then(parse_port)
            .ok_or(format!("{PORT_VAR}={var:?} is not a port"))?,
        (None, None) => return Err(format!("pass --port or set {PORT_VAR}")),
    };
    Ok(Options {
        config,
        seconds,
        report,
        strict,
    })
}

fn parse_port(text: &str) -> Option<u16> {
    text.parse().ok().filter(|&port| port != 0)
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1), std::env::var_os(PORT_VAR)) {
        Ok(options) => options,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("tether-fake-headset: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let port = options.config.port;
    let report = run(options.config, Duration::from_secs_f64(options.seconds));
    let json = report.to_json();
    let written = match &options.report {
        Some(path) => std::fs::write(path, format!("{json}\n"))
            .map_err(|e| eprintln!("tether-fake-headset: writing {}: {e}", path.display())),
        None => {
            println!("{json}");
            Ok(())
        }
    };
    eprintln!(
        "tether-fake-headset: 127.0.0.1:{port}, {:.1} s, {} frame(s) ({} keyframe(s)), {} audio packet(s), {} error(s), {} warning(s)",
        report.duration_s,
        report.video.frames,
        report.video.keyframes,
        report.audio.packets,
        report.error_count,
        report.warning_count
    );
    for error in &report.errors {
        eprintln!("  error {error}");
    }
    let connected = report.link.connects > 0;
    if written.is_err() || !connected || (options.strict && !report.passed) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use tether_fake_headset::ClockModel;

    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn the_port_comes_from_the_argument_then_the_environment_and_is_never_assumed() {
        let var = |s: &str| Some(OsString::from(s));
        assert_eq!(
            parse(args("--port 5001"), var("6001")).unwrap().config.port,
            5001
        );
        assert_eq!(parse(args(""), var("6001")).unwrap().config.port, 6001);
        assert!(parse(args(""), None).unwrap_err().contains(PORT_VAR));
        assert!(
            parse(args(""), var("")).is_err(),
            "an empty variable is unset"
        );
        assert!(
            parse(args(""), var("port"))
                .unwrap_err()
                .contains("not a port")
        );
        assert!(parse(args("--port 70000"), None).is_err());
        assert!(parse(args("--port 0"), None).is_err());
        assert!(parse(args(""), var("0")).is_err());
    }

    #[test]
    fn the_e2e_invocation_parses() {
        let options = parse(args("--port 49200 --seconds 10 --report /tmp/r.json"), None).unwrap();
        assert_eq!(options.config.port, 49200);
        assert_eq!(options.seconds, 10.0);
        assert_eq!(options.report, Some(PathBuf::from("/tmp/r.json")));
        assert!(!options.strict);
    }

    #[test]
    fn options_shape_the_profile_clock_and_script() {
        let options = parse(
            args(
                "--port 1 --motion still --at 1.5:press:left:menu --at 2:recenter --clock-offset-ms -250 \
                 --drift-ppm 0 --eye 2064x2208 --refresh 120 --h264 --no-audio --no-keyframe-request --strict",
            ),
            None,
        )
        .unwrap();
        let c = &options.config;
        assert_eq!(c.script.motion, Motion::Still);
        assert_eq!(c.script.cues.len(), 2);
        assert_eq!(
            c.clock,
            ClockModel {
                offset_ns: -250_000_000,
                drift_ppm: 0.0
            }
        );
        assert_eq!((c.profile.eye_width, c.profile.eye_height), (2064, 2208));
        assert_eq!(c.profile.refresh_rate, 120.0);
        assert!(!c.profile.supports_hevc && c.profile.audio_sample_rate == 0);
        assert!(!c.keyframe_request_on_stream_start && options.strict);
    }

    #[test]
    fn bad_arguments_are_refused() {
        for bad in [
            "--port",
            "--port 1 --motion fly",
            "--port 1 --eye 10",
            "--port 1 --at x",
            "--port 1 --what",
        ] {
            assert!(parse(args(bad), None).is_err(), "{bad:?}");
        }
    }
}
