//! `tetherd`: the per-user launchd agent that owns the headset. Every OpenXR app's runtime
//! reaches it over NSXPC and a shared-memory region (docs/ARCHITECTURE.md § Runtime ↔ tetherd);
//! `scripts/tetherd.sh` builds, signs and installs it.

#[cfg(target_os = "macos")]
mod adb;
#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod convert;
#[cfg(target_os = "macos")]
mod daemon;
#[cfg(target_os = "macos")]
mod log;
#[cfg(target_os = "macos")]
mod permission;
#[cfg(target_os = "macos")]
mod report;
#[cfg(target_os = "macos")]
mod session;
#[cfg(target_os = "macos")]
mod stage;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    macos::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;

    use tether_ipc::control::Listener;
    use tether_protocol::USB_PORT;
    use tether_stream::link::usb_port;
    use tether_stream::{Headset, UsbTransport};

    use crate::adb::{self, Keeper};
    use crate::audio::{self, Audio, Permission};
    use crate::daemon::{Daemon, Hub};
    use crate::log;
    use crate::permission;
    use crate::stage::Stage;

    const USAGE: &str = "\
usage: tetherd [--service NAME] [--usb-port PORT] [--no-adb] [--reports DIR]
               [--audio tap|synthetic|off] [--audio-state DIR]
       tetherd audio-permission|audio-check --out DIR [--audio-state DIR]

Owns the headset for every OpenXR app on this Mac. launchd runs it from
~/Library/LaunchAgents/dev.tether.tetherd.plist (scripts/tetherd.sh install).

  --service NAME     the Mach service to check in for (default dev.tether.tetherd); it must be
                     one of the launchd plist's MachServices
  --usb-port PORT    the loopback port the headset's adb reverse tunnel lands on (default
                     TETHER_USB_PORT, else 47800); 0 picks a free one
  --no-adb           leave adb alone; on any port but 47800 it is left alone anyway, so a test
                     instance never takes the real headset's tunnel
  --reports DIR      where session reports go (default ~/Library/Logs/Tether/sessions)
  --audio SOURCE     the app's sound in the headset (default TETHER_AUDIO, else tap): tap
                     captures the app in front with Core Audio once audio-permission has
                     passed; synthetic sends a test tone instead; off leaves sound on the Mac
  --audio-state DIR  where the permission's grant and block files live (default
                     ~/Library/Application Support/Tether)

audio-permission asks once for the System Audio Recording permission by tapping a silent
afplay; audio-check captures a known tone and measures it. Both run only as a one-shot launchd
job (scripts/tetherd.sh audio-permission | audio-check), so the grant is tetherd's own.";

    /// Overrides the default `--audio` source, as a launchd plist's environment can.
    const AUDIO_VAR: &str = "TETHER_AUDIO";

    struct Options {
        service: String,
        port: u16,
        adb: bool,
        reports: PathBuf,
        audio: audio::Source,
        audio_state: PathBuf,
    }

    fn audio_source(text: &str) -> Result<audio::Source, String> {
        audio::Source::parse(text).ok_or(format!(
            "the audio source is tap, synthetic or off, not {text:?}"
        ))
    }

    fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
        let audio = match std::env::var(AUDIO_VAR).ok().filter(|v| !v.is_empty()) {
            Some(text) => audio_source(&text).map_err(|e| format!("{AUDIO_VAR}: {e}"))?,
            None => audio::Source::Tap,
        };
        let mut options = Options {
            service: tether_ipc::SERVICE_NAME.to_owned(),
            port: usb_port().map_err(|e| e.to_string())?,
            adb: true,
            reports: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join("Library/Logs/Tether/sessions"),
            audio,
            audio_state: Permission::default_dir(),
        };
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or(format!("{arg} needs a value"));
            match arg.as_str() {
                "--service" => options.service = value()?,
                "--usb-port" => {
                    let text = value()?;
                    options.port = text
                        .parse()
                        .map_err(|_| format!("--usb-port needs a port, not {text:?}"))?;
                }
                "--no-adb" => options.adb = false,
                "--reports" => options.reports = PathBuf::from(value()?),
                "--audio" => options.audio = audio_source(&value()?)?,
                "--audio-state" => options.audio_state = PathBuf::from(value()?),
                "-h" | "--help" => return Err(String::new()),
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        options.adb &= options.port == USB_PORT;
        Ok(options)
    }

    pub fn main() -> ExitCode {
        let mut args = std::env::args().skip(1).peekable();
        if let Some(mode) = args.peek().and_then(|a| permission::Mode::parse(a)) {
            args.next();
            return permission::run(mode, args, USAGE);
        }
        let options = match parse(args) {
            Ok(options) => options,
            Err(e) => {
                if !e.is_empty() {
                    eprintln!("tetherd: {e}");
                }
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        };
        log!(
            "tetherd {} (pid {}) serving {}, audio {}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            options.service,
            options.audio
        );

        let hub = Arc::new(Hub::default());
        let headset = Headset::observed(Some(hub.clone()));
        let stage = Arc::new(Stage::new(headset.clone(), hub.clone()));
        let audio = Audio::start(
            options.audio,
            stage.clone(),
            Permission::in_dir(options.audio_state.clone()),
        );
        let daemon = Arc::new(Daemon::new(
            hub.clone(),
            stage,
            audio,
            options.reports.clone(),
        ));
        hub.start_heartbeat();
        listen(headset, options.port, options.adb);
        let _listener = Listener::mach_service(&options.service, daemon);
        loop {
            thread::park();
        }
    }

    /// Binds the headset's port, waiting out an app on an older runtime that still holds it.
    fn listen(headset: Arc<Headset>, port: u16, keep_adb: bool) {
        let spawned = thread::Builder::new()
            .name("tetherd-listen".into())
            .spawn(move || {
                let mut complained = false;
                let transport = loop {
                    match UsbTransport::bind_port(port) {
                        Ok(transport) => break transport,
                        Err(e) => {
                            if !complained {
                                log!(
                                    "cannot listen on 127.0.0.1:{port} ({e}); an app on an older \
                                     Tether runtime may hold it. Retrying every second."
                                );
                                complained = true;
                            }
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                };
                let bound = transport.port();
                // tether-e2e reads the port from this line.
                log!("headset listener on 127.0.0.1:{bound}");
                if keep_adb {
                    match adb::Binary::find() {
                        Some(binary) => Keeper {
                            tool: binary,
                            server: adb::server_address(),
                            device_port: USB_PORT,
                            host_port: bound,
                            stop: Arc::new(AtomicBool::new(false)),
                        }
                        .spawn(),
                        None => log!(
                            "no adb in $ANDROID_HOME, ~/Library/Android/sdk or PATH; the headset \
                             needs `adb reverse tcp:{USB_PORT} tcp:{bound}` set by hand"
                        ),
                    }
                }
                headset.accept(Box::new(transport));
            });
        if let Err(e) = spawned {
            log!("cannot start the listener thread: {e}");
        }
    }
}
