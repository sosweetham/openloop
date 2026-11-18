//! Processes and files for one end-to-end run.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

/// Where one run keeps its runtime manifest, logs and the fake headset's report.
pub struct WorkDir {
    pub path: PathBuf,
}

impl WorkDir {
    pub fn create() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("openloop-e2e-{}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn remove(self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A runtime manifest for `XR_RUNTIME_JSON`, so the loader takes `library` without anything
/// being installed.
pub fn write_manifest(path: &Path, library: &Path) -> io::Result<()> {
    let manifest = serde_json::json!({
        "file_format_version": "1.0.0",
        "runtime": { "name": "OpenLoop", "library_path": fs::canonicalize(library)? },
    });
    fs::write(path, serde_json::to_vec_pretty(&manifest)?)
}

/// `openloopd` under a launchd label of its own, which is booted out however the run ends. Its
/// Mach service is named after the label, so nothing reaches it unless told to, and it never
/// touches adb or the real headset's port.
pub struct Agent {
    pub label: String,
    domain: String,
    pub log: PathBuf,
}

impl Agent {
    pub fn start(binary: &Path, dir: &WorkDir) -> Result<Self, String> {
        // SAFETY: no preconditions.
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let printed = Command::new("launchctl").args(["print", &domain]).output();
        if !printed.is_ok_and(|p| p.status.success()) {
            return Err(format!(
                "launchd has no {domain} domain (no GUI login, as over ssh), so no agent can run"
            ));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let label = format!("dev.openloop.test.e2e.{}.{nanos}", std::process::id());
        let log = dir.file("openloopd.log");
        let plist = dir.file(&format!("{label}.plist"));
        let reports = dir.file("sessions");
        let audio_state = dir.file("audio-state");
        let string = |text: &str| format!("<string>{}</string>", xml_escape(text));
        // A synthetic tone stands in for the apps' sound: tests never touch Core Audio, whose
        // tap operations can wedge audio for the whole Mac while a permission prompt is pending.
        let arguments = [
            &binary.to_string_lossy(),
            "--service",
            &label,
            "--usb-port",
            "0",
            "--no-adb",
            "--reports",
            &reports.to_string_lossy(),
            "--audio",
            "synthetic",
            "--audio-state",
            &audio_state.to_string_lossy(),
        ]
        .map(string)
        .join("");
        fs::write(
            &plist,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>{label}
  <key>ProgramArguments</key><array>{arguments}</array>
  <key>MachServices</key><dict><key>{service}</key><true/></dict>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>ThrottleInterval</key><integer>1</integer>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>StandardOutPath</key>{log}
  <key>StandardErrorPath</key>{log}
</dict>
</plist>
"#,
                label = string(&label),
                service = xml_escape(&label),
                log = string(&log.to_string_lossy()),
            ),
        )
        .map_err(|e| format!("writing {}: {e}", plist.display()))?;
        let bootstrapped = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist)
            .output()
            .map_err(|e| format!("launchctl: {e}"))?;
        if !bootstrapped.status.success() {
            return Err(format!(
                "launchctl bootstrap {domain} {}: {}",
                plist.display(),
                String::from_utf8_lossy(&bootstrapped.stderr).trim()
            ));
        }
        Ok(Self { label, domain, log })
    }

    /// The agent's process, as launchd reports it.
    pub fn pid(&self) -> Option<u32> {
        let out = Command::new("launchctl")
            .args(["print", &format!("{}/{}", self.domain, self.label)])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|line| line.trim().strip_prefix("pid = ")?.parse().ok())
    }

    /// The first log line that contains one of `markers`, waiting up to `timeout` for it.
    pub fn wait_for_log(&self, markers: &[&str], timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(line) = lines_with(&self.log, markers).into_iter().next() {
                return Ok(line);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "openloopd logged none of {markers:?} within {timeout:?}:\n{}",
                    tail(&self.log, 20)
                ));
            }
            thread::sleep(POLL);
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{}/{}", self.domain, self.label)])
            .output();
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A child process with its output in the work directory, killed if the run ends first.
pub struct Proc {
    pub name: &'static str,
    child: Child,
    pub stderr: PathBuf,
}

impl Proc {
    pub fn spawn(name: &'static str, mut command: Command, dir: &WorkDir) -> Result<Self, String> {
        let stdout = dir.file(&format!("{name}.out"));
        let stderr = dir.file(&format!("{name}.err"));
        let open = |path: &Path| {
            File::create(path).map_err(|e| format!("creating {}: {e}", path.display()))
        };
        command.stdout(open(&stdout)?).stderr(open(&stderr)?);
        let child = command
            .spawn()
            .map_err(|e| format!("starting {name} ({:?}): {e}", command.get_program()))?;
        Ok(Self {
            name,
            child,
            stderr,
        })
    }

    /// Spawns with stdin on a pipe this process holds; [`Proc::close_stdin`] closes it.
    pub fn spawn_with_stdin(
        name: &'static str,
        mut command: Command,
        dir: &WorkDir,
    ) -> Result<Self, String> {
        command.stdin(Stdio::piped());
        Self::spawn(name, command, dir)
    }

    pub fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// SIGKILL, as a crash would end it: nothing runs in the process after this.
    pub fn kill(&mut self) -> Option<ExitStatus> {
        let _ = self.child.kill();
        self.child.wait().ok()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// `None` if it is still running after `timeout`.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(Some(status)),
                Ok(None) if Instant::now() >= deadline => return Ok(None),
                Ok(None) => thread::sleep(POLL),
                Err(e) => return Err(format!("waiting for {}: {e}", self.name)),
            }
        }
    }

    /// The first stderr line that contains one of `markers`, waiting up to `timeout` for it.
    pub fn wait_for_stderr(
        &mut self,
        markers: &[&str],
        timeout: Duration,
    ) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(line) = lines_with(&self.stderr, markers).into_iter().next() {
                return Ok(line);
            }
            if let Some(status) = self.exited() {
                return Err(format!(
                    "{} exited ({status}) before logging any of {markers:?}:\n{}",
                    self.name,
                    tail(&self.stderr, 20)
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{} logged none of {markers:?} within {timeout:?}",
                    self.name
                ));
            }
            thread::sleep(POLL);
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.exited().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// The lines of a log that contain any of `markers`.
pub fn lines_with(path: &Path, markers: &[&str]) -> Vec<String> {
    read_lossy(path)
        .lines()
        .filter(|l| markers.iter().any(|m| l.contains(m)))
        .map(str::to_owned)
        .collect()
}

/// The last `lines` lines of a log, for a failure message.
pub fn tail(path: &Path, lines: usize) -> String {
    let log = read_lossy(path);
    let all: Vec<&str> = log.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_names_the_library_absolutely() {
        let dir = WorkDir {
            path: std::env::temp_dir().join(format!("openloop-e2e-test-{}", std::process::id())),
        };
        fs::create_dir_all(&dir.path).unwrap();
        let library = dir.file("lib \"quoted\".dylib");
        fs::write(&library, b"").unwrap();
        let manifest = dir.file("manifest.json");
        write_manifest(&manifest, &library).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(parsed["file_format_version"], "1.0.0");
        let path = parsed["runtime"]["library_path"].as_str().unwrap();
        assert!(Path::new(path).is_absolute());
        assert!(path.ends_with("lib \"quoted\".dylib"));
        dir.remove();
    }
}
