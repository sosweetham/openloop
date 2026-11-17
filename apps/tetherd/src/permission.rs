//! `tetherd audio-permission` and `tetherd audio-check`: the one-shot runs a person makes at the
//! Mac (docs/ARCHITECTURE.md § Audio). They run tetherd's own binary, as a launchd job, because
//! TCC was seen keying a bare binary's grants on its path and code requirement, and holds a
//! process started from a shell to the shell's app. `scripts/tetherd.sh audio-permission`
//! and `audio-check` start them. Either one that receives audio writes the grant file the agent
//! waits for before it taps any app.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use tether_audiotap::probe::{Outcome, ProbeOptions, live_check, request_permission};

use crate::audio::Permission;
use crate::log::local_time;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Permission,
    Check,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "audio-permission" => Some(Mode::Permission),
            "audio-check" => Some(Mode::Check),
            _ => None,
        }
    }
}

/// Exit status: 0 passed, 1 failed, 2 capture blocked, 3 not a launchd job, 64 bad arguments.
pub fn run(mode: Mode, mut args: impl Iterator<Item = String>, usage: &str) -> ExitCode {
    let (mut out_dir, mut state) = (None, Permission::default_dir());
    while let Some(flag) = args.next() {
        match (flag.as_str(), args.next()) {
            ("--out", Some(dir)) => out_dir = Some(PathBuf::from(dir)),
            ("--audio-state", Some(dir)) => state = PathBuf::from(dir),
            _ => {
                eprintln!("{usage}");
                return ExitCode::from(64);
            }
        }
    }
    let Some(out_dir) = out_dir else {
        eprintln!("{usage}");
        return ExitCode::from(64);
    };
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != 1 {
        eprintln!(
            "refusing to run: start this through scripts/tetherd.sh audio-permission or \
             audio-check, as a launchd job, or the prompt and the grant go to this terminal"
        );
        return ExitCode::from(3);
    }
    for dir in [&out_dir, &state] {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("cannot create {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    }
    let permission = Permission::in_dir(state);
    let options = ProbeOptions {
        out_dir,
        marker: Some(permission.blocked.clone()),
    };
    let result = match mode {
        Mode::Permission => request_permission(&options),
        Mode::Check => live_check(&options),
    };
    if tether_audiotap::permission_proven() {
        let binary = std::env::current_exe().unwrap_or_default();
        let note = format!("{} audio arrived in {}\n", local_time(), binary.display());
        if let Err(e) = fs::write(&permission.granted, note) {
            eprintln!("cannot write {}: {e}", permission.granted.display());
        }
    }
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
        Err(e) => {
            println!("OUTCOME error: {e}");
            ExitCode::FAILURE
        }
    }
}
