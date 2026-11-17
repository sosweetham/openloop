//! Asks for the System Audio Recording permission under tetherd's signing identity, or checks a
//! live capture. Start it only through `packages/rust/audiotap/scripts/run-once.sh`, which signs
//! it and runs it once as a launchd job.
//!
//!   tether-audiotap permission|check --out <dir> [--marker <file>]

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use tether_audiotap::probe::{Outcome, ProbeOptions, live_check, request_permission};

    let usage = || {
        eprintln!("usage: tether-audiotap permission|check --out <dir> [--marker <file>]");
        ExitCode::from(64)
    };
    let mut args = std::env::args().skip(1);
    let Some(mode) = args.next() else {
        return usage();
    };
    let (mut out_dir, mut marker) = (None, None);
    while let Some(flag) = args.next() {
        match (flag.as_str(), args.next()) {
            ("--out", Some(dir)) => out_dir = Some(PathBuf::from(dir)),
            ("--marker", Some(file)) => marker = Some(PathBuf::from(file)),
            _ => return usage(),
        }
    }
    let Some(out_dir) = out_dir else {
        return usage();
    };
    // TCC holds a launchd job responsible for itself, and anything else for the app that
    // started it (the terminal), which would then own the grant and the prompt.
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != 1 {
        eprintln!("refusing to run: start this through packages/rust/audiotap/scripts/run-once.sh (a launchd job)");
        return ExitCode::from(3);
    }
    if let Err(error) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {error}", out_dir.display());
        return ExitCode::FAILURE;
    }
    let options = ProbeOptions { out_dir, marker };
    let result = match mode.as_str() {
        "permission" => request_permission(&options),
        "check" => live_check(&options),
        _ => return usage(),
    };
    match result {
        Ok(Outcome::Passed) => {
            println!("OUTCOME passed");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Blocked(reason)) => {
            println!("OUTCOME blocked: {reason}");
            ExitCode::from(2)
        }
        Ok(Outcome::Failed(problems)) => {
            println!("OUTCOME failed");
            for problem in problems {
                println!("  - {problem}");
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            println!("OUTCOME error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
