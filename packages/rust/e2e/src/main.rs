//! End-to-end check of a Tether build: Khronos's `hello_xr` renders on the runtime under test
//! while `tether-fake-headset` stands in for the Quest on a loopback port the kernel picks. Run
//! through `scripts/e2e.sh`; see `docs/ARCHITECTURE.md` § Testing.
//!
//! In-process mode runs the headset link inside `hello_xr` (`TETHER_INPROCESS=1`). Tetherd mode
//! runs `tetherd` under a throwaway launchd label, crashes one `hello_xr` mid-stream and starts
//! another on the same headset link.

mod harness;
mod report;

use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use harness::{Agent, Proc, WorkDir};
use report::Check;

const USAGE: &str = "usage: tether-e2e --runtime <libtether_runtime.dylib> \
--fake-headset <tether-fake-headset> --app <hello_xr> [--tetherd <signed tetherd>] \
[--seconds <n>] [--keep]";

/// What the runtime, or tetherd, logs once its listener is bound; the port follows.
const LISTENING: &str = "headset listener on 127.0.0.1:";
const NOT_LISTENING: &str = "USB listener unavailable";
/// Loader, Metal device and instance setup before the runtime binds its listener.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(20);
/// How long past its `--seconds` the fake headset may take to write its report.
const REPORT_GRACE: Duration = Duration::from_secs(15);
/// hello_xr quits at its next frame once stdin closes.
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);
/// tetherd's log line when an app's session begins; the session id and app follow.
const BEGAN: &str = "began; its video goes to the headset";
/// How long the crashed app's successor waits after the crash before it starts.
const RESTART_GAP: Duration = Duration::from_secs(1);
/// tetherd must have ended the crashed app's stream this soon after the kill.
const STREAM_END_LIMIT_S: f64 = 2.0;

struct Options {
    runtime: PathBuf,
    fake_headset: PathBuf,
    app: PathBuf,
    tetherd: Option<PathBuf>,
    seconds: u64,
    keep: bool,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let (mut runtime, mut fake_headset, mut app, mut tetherd) = (None, None, None, None);
        let mut seconds = 10;
        let mut keep = false;
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or(format!("{arg} needs a value"));
            match arg.as_str() {
                "--runtime" => runtime = Some(PathBuf::from(value()?)),
                "--fake-headset" => fake_headset = Some(PathBuf::from(value()?)),
                "--app" => app = Some(PathBuf::from(value()?)),
                "--tetherd" => tetherd = Some(PathBuf::from(value()?)),
                "--seconds" => {
                    seconds = value()?
                        .parse()
                        .ok()
                        .filter(|&s| s > 0)
                        .ok_or("--seconds needs a positive whole number")?
                }
                "--keep" => keep = true,
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        let exists = |path: Option<PathBuf>, what: &str| {
            let path = path.ok_or(format!("{what} is required"))?;
            if path.is_file() {
                Ok(path)
            } else {
                Err(format!("{what} {} does not exist", path.display()))
            }
        };
        Ok(Self {
            runtime: exists(runtime, "--runtime")?,
            fake_headset: exists(fake_headset, "--fake-headset")?,
            app: exists(app, "--app")?,
            tetherd: tetherd.map(|t| exists(Some(t), "--tetherd")).transpose()?,
            seconds,
            keep,
        })
    }
}

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("tether-e2e: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let dir = match WorkDir::create() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("tether-e2e: work directory: {e}");
            return ExitCode::from(2);
        }
    };
    let (mode, result) = match &options.tetherd {
        None => ("in-process", in_process(&options, &dir)),
        Some(tetherd) => ("tetherd", through_tetherd(&options, tetherd, &dir)),
    };
    let checks = result.unwrap_or_else(|e| vec![Check::fail("the run", e)]);
    for check in &checks {
        println!("{check}");
    }
    let passed = checks.iter().all(|c| c.ok);
    println!("e2e {mode}: {}", if passed { "PASS" } else { "FAIL" });
    if passed && !options.keep {
        dir.remove();
    } else {
        println!("logs and report: {}", dir.path.display());
    }
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn hello_xr(options: &Options, manifest: &std::path::Path) -> Command {
    let mut app = Command::new(&options.app);
    app.args(["-g", "Metal"]).env("XR_RUNTIME_JSON", manifest);
    app
}

fn fake_headset(options: &Options, port: u16, seconds: u64, report: &std::path::Path) -> Command {
    let mut headset = Command::new(&options.fake_headset);
    headset
        .args(["--port", &port.to_string()])
        .args(["--seconds", &seconds.to_string()])
        .arg("--report")
        .arg(report);
    headset
}

/// hello_xr streams to the fake headset through the runtime in hello_xr's own process.
fn in_process(options: &Options, dir: &WorkDir) -> Result<Vec<Check>, String> {
    let manifest = dir.file("tether_runtime.json");
    harness::write_manifest(&manifest, &options.runtime)
        .map_err(|e| format!("writing {}: {e}", manifest.display()))?;

    let mut app = hello_xr(options, &manifest);
    app.env("TETHER_INPROCESS", "1").env("TETHER_USB_PORT", "0");
    // hello_xr quits when `getchar` returns, so its stdin stays open until the run is over.
    let mut app = Proc::spawn_with_stdin("hello_xr", app, dir)?;

    let line = app.wait_for_stderr(&[LISTENING, NOT_LISTENING], LISTEN_TIMEOUT)?;
    let port = port_in(&line)?;

    let report = dir.file("report.json");
    let mut headset = Proc::spawn(
        "fake-headset",
        fake_headset(options, port, options.seconds, &report),
        dir,
    )?;
    let headset_status =
        headset.wait_timeout(Duration::from_secs(options.seconds) + REPORT_GRACE)?;

    let ran_throughout = app.exited().is_none();
    app.close_stdin();
    let app_status = app.wait_timeout(QUIT_TIMEOUT)?;

    let mut checks = vec![
        Check::exited(
            "tether-fake-headset exits cleanly",
            headset_status,
            &headset.stderr,
        ),
        if ran_throughout {
            Check::exited("hello_xr quits cleanly when asked", app_status, &app.stderr)
        } else {
            Check::fail(
                "hello_xr runs for the whole session",
                format!("{app_status:?}\n{}", harness::tail(&app.stderr, 20)),
            )
        },
        Check::no_errors_logged("hello_xr", &app.stderr),
    ];
    checks.extend(report::judge_file(&report, None, None));
    Ok(checks)
}

/// tetherd owns the fake headset's link while one hello_xr streams, is killed mid-stream, and a
/// second one takes over the same link.
fn through_tetherd(
    options: &Options,
    tetherd: &std::path::Path,
    dir: &WorkDir,
) -> Result<Vec<Check>, String> {
    let manifest = dir.file("tether_runtime.json");
    harness::write_manifest(&manifest, &options.runtime)
        .map_err(|e| format!("writing {}: {e}", manifest.display()))?;
    let agent = Agent::start(tetherd, dir)?;
    let port = port_in(&agent.wait_for_log(&[LISTENING], LISTEN_TIMEOUT)?)?;
    let tetherd_pid = agent.pid();

    // The first app streams for half the run before it is killed; the second streams for a
    // whole run after it.
    let first_for = Duration::from_secs(options.seconds.div_ceil(2).max(4));
    let headset_seconds = first_for.as_secs() + RESTART_GAP.as_secs() + options.seconds + 4;
    let report = dir.file("report.json");
    let started = Instant::now();
    let mut headset = Proc::spawn(
        "fake-headset",
        fake_headset(options, port, headset_seconds, &report),
        dir,
    )?;

    let app = |name| {
        let mut command = hello_xr(options, &manifest);
        command.env("TETHER_SERVICE", &agent.label);
        Proc::spawn_with_stdin(name, command, dir)
    };
    let mut first = app("hello_xr-crashed")?;
    agent.wait_for_log(&[BEGAN], LISTEN_TIMEOUT)?;
    thread::sleep(first_for);
    let first_pid = first.pid();
    let first_alive = first.exited().is_none();
    first.kill();
    let killed_at_s = started.elapsed().as_secs_f64();

    thread::sleep(RESTART_GAP);
    let mut second = app("hello_xr")?;
    let headset_status =
        headset.wait_timeout(Duration::from_secs(headset_seconds) + REPORT_GRACE)?;
    let ran_throughout = second.exited().is_none();
    second.close_stdin();
    let second_status = second.wait_timeout(QUIT_TIMEOUT)?;
    // tetherd ends the second stream when hello_xr ends its session.
    thread::sleep(Duration::from_millis(500));

    let cleaned_up = harness::lines_with(&agent.log, &[&format!("(pid {first_pid}) disconnected")]);
    let panics = harness::lines_with(&agent.log, &["panicked at"]);
    let reports = std::fs::read_dir(dir.file("sessions"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    let mut checks = vec![
        Check::new(
            first_alive,
            "the first hello_xr streams until it is killed",
            harness::tail(&first.stderr, 20),
        ),
        Check::new(
            tetherd_pid.is_some() && agent.pid() == tetherd_pid && panics.is_empty(),
            "tetherd survives the app's crash",
            format!(
                "pid {tetherd_pid:?} before, {:?} after\n{}",
                agent.pid(),
                panics.join("\n")
            ),
        ),
        Check::new(
            !cleaned_up.is_empty(),
            "tetherd drops the crashed app's session",
            harness::tail(&agent.log, 20),
        ),
        Check::exited(
            "tether-fake-headset exits cleanly",
            headset_status,
            &headset.stderr,
        ),
        if ran_throughout {
            Check::exited(
                "the next hello_xr quits cleanly",
                second_status,
                &second.stderr,
            )
        } else {
            Check::fail(
                "the next hello_xr runs for the rest of the session",
                format!("{second_status:?}\n{}", harness::tail(&second.stderr, 20)),
            )
        },
        Check::no_errors_logged("hello_xr", &second.stderr),
        Check::no_errors_logged("the crashed hello_xr", &first.stderr),
        Check::new(
            reports >= 2,
            format!("tetherd writes a report per session ({reports})"),
            harness::tail(&agent.log, 10),
        ),
    ];
    checks.extend(report::judge_file(
        &report,
        Some(report::Handover {
            killed_at_s,
            limit_s: STREAM_END_LIMIT_S,
        }),
        Some(report::Sound::synthetic()),
    ));
    Ok(checks)
}

fn port_in(line: &str) -> Result<u16, String> {
    line.split_once(LISTENING)
        .and_then(|(_, port)| port.trim().parse().ok())
        .ok_or(format!("no headset listener: {line}"))
}
