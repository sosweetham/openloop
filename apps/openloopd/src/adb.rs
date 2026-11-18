//! Keeps the headset's `adb reverse` tunnel up while a headset is attached: the Quest client
//! connects to its own `127.0.0.1:47800`, which adb carries to `openloopd`'s listener
//! (docs/ARCHITECTURE.md § Transports).
//!
//! Devices are followed through the adb server's `host:track-devices` service (adb's
//! `docs/dev/services.md`): the server pushes the device list on every change, so nothing polls
//! while no headset is attached. The reverse itself is set with the SDK's `adb` binary, exactly
//! as `adb reverse` sets it, and re-checked every few seconds while a device is attached,
//! because Android drops it whenever USB or adb resets. With no adb server running, `openloopd`
//! starts one with `adb start-server`.

use std::collections::HashSet;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::log;

const SERVER_PORT: u16 = 5037;
/// adb's own override of [`SERVER_PORT`].
const SERVER_PORT_VAR: &str = "ANDROID_ADB_SERVER_PORT";
/// How often a tunnel is re-checked while its device is attached.
const RECHECK: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// An `adb` invocation that runs this long is stuck on a device that stopped answering.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// What the keeper asks of adb besides the device list.
pub trait Tool: Send + 'static {
    /// `adb -s <serial> reverse tcp:<device_port> tcp:<host_port>`.
    fn reverse(&self, serial: &str, device_port: u16, host_port: u16) -> io::Result<()>;
    /// `adb -s <serial> reverse --list`.
    fn reverse_list(&self, serial: &str) -> io::Result<String>;
    fn start_server(&self) -> io::Result<()>;
}

/// The SDK's `adb` binary.
pub struct Binary(pub PathBuf);

impl Binary {
    /// `$ANDROID_HOME`, `$ANDROID_SDK_ROOT` or `~/Library/Android/sdk`, then `PATH`.
    pub fn find() -> Option<Binary> {
        let sdk_roots = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .chain(std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Android/sdk")));
        let in_sdk = sdk_roots.map(|root| root.join("platform-tools/adb"));
        let on_path = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .map(|dir| dir.join("adb"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        in_sdk
            .chain(on_path)
            .find(|path| is_executable(path))
            .map(Binary)
    }

    fn run(&self, args: &[&str]) -> io::Result<String> {
        let mut child = Command::new(&self.0)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    stdout.read_to_string(&mut out)?;
                }
                if let Some(mut stderr) = child.stderr.take() {
                    stderr.read_to_string(&mut err)?;
                }
                return if status.success() {
                    Ok(out)
                } else {
                    Err(io::Error::other(format!(
                        "adb {args:?}: {status}: {}",
                        err.trim()
                    )))
                };
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("adb {args:?} hung"),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl Tool for Binary {
    fn reverse(&self, serial: &str, device_port: u16, host_port: u16) -> io::Result<()> {
        let device = format!("tcp:{device_port}");
        let host = format!("tcp:{host_port}");
        self.run(&["-s", serial, "reverse", &device, &host])
            .map(drop)
    }

    fn reverse_list(&self, serial: &str) -> io::Result<String> {
        self.run(&["-s", serial, "reverse", "--list"])
    }

    fn start_server(&self) -> io::Result<()> {
        self.run(&["start-server"]).map(drop)
    }
}

pub struct Keeper<T: Tool> {
    pub tool: T,
    /// The adb server, usually `127.0.0.1:5037`.
    pub server: SocketAddr,
    /// The port the headset client connects to on its own loopback.
    pub device_port: u16,
    /// `openloopd`'s listener.
    pub host_port: u16,
    pub stop: Arc<AtomicBool>,
}

/// The adb server this Mac's `adb` talks to.
pub fn server_address() -> SocketAddr {
    let port = std::env::var(SERVER_PORT_VAR)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(SERVER_PORT);
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

impl<T: Tool> Keeper<T> {
    pub fn spawn(self) {
        let spawned = thread::Builder::new()
            .name("openloopd-adb".into())
            .spawn(move || self.run());
        if let Err(e) = spawned {
            log!("cannot start the adb tunnel keeper: {e}");
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn run(self) {
        let mut backoff = Duration::from_secs(1);
        let mut announced = false;
        while !self.stopped() {
            match self.follow_devices(&mut announced) {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                    log!("no adb server on {}; starting one", self.server);
                    if let Err(e) = self.tool.start_server() {
                        log!("adb start-server: {e}");
                    }
                }
                Err(e) => log!("adb server: {e}"),
            }
            self.sleep(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    fn sleep(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !self.stopped() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Follows the server's device list until the server goes away.
    fn follow_devices(&self, announced: &mut bool) -> io::Result<()> {
        let mut stream = TcpStream::connect_timeout(&self.server, Duration::from_secs(2))?;
        stream.write_all(&request("host:track-devices"))?;
        status(&mut stream)?;
        if !*announced {
            log!(
                "keeping adb reverse tcp:{} tcp:{} on every attached device",
                self.device_port,
                self.host_port
            );
            *announced = true;
        }
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        let mut pending = Vec::new();
        // A restarted adb server has forgotten every reverse, so each device is set up again.
        let mut tunnelled = HashSet::new();
        let mut attached = Vec::new();
        let mut last_check = Instant::now();
        let mut buf = [0u8; 4096];
        while !self.stopped() {
            match stream.read(&mut buf) {
                Ok(0) => return Err(io::Error::new(ErrorKind::UnexpectedEof, "closed")),
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => return Err(e),
            }
            while let Some(list) = take_message(&mut pending)? {
                attached = ready_devices(&list);
                tunnelled.retain(|serial| attached.contains(serial));
                for serial in &attached {
                    if !tunnelled.contains(serial) && self.tunnel(serial) {
                        tunnelled.insert(serial.clone());
                    }
                }
            }
            if !attached.is_empty() && last_check.elapsed() >= RECHECK {
                last_check = Instant::now();
                for serial in &attached {
                    if !self.has_tunnel(serial) && !self.tunnel(serial) {
                        tunnelled.remove(serial);
                    }
                }
            }
        }
        Ok(())
    }

    fn tunnel(&self, serial: &str) -> bool {
        match self.tool.reverse(serial, self.device_port, self.host_port) {
            Ok(()) => {
                log!(
                    "adb reverse tcp:{} tcp:{} on {serial}",
                    self.device_port,
                    self.host_port
                );
                true
            }
            Err(e) => {
                log!("adb reverse on {serial}: {e}");
                false
            }
        }
    }

    fn has_tunnel(&self, serial: &str) -> bool {
        let wanted = format!("tcp:{} tcp:{}", self.device_port, self.host_port);
        self.tool
            .reverse_list(serial)
            .is_ok_and(|list| list.lines().any(|line| line.trim_end().ends_with(&wanted)))
    }
}

/// A host request: its length in four hex digits, then the request.
fn request(service: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", service.len()).into_bytes();
    out.extend_from_slice(service.as_bytes());
    out
}

/// `OKAY`, or `FAIL` and a length-prefixed reason.
fn status(stream: &mut TcpStream) -> io::Result<()> {
    let mut word = [0u8; 4];
    stream.read_exact(&mut word)?;
    match &word {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let mut len = [0u8; 4];
            stream.read_exact(&mut len)?;
            let len = hex_len(&len)?;
            let mut reason = vec![0u8; len];
            stream.read_exact(&mut reason)?;
            Err(io::Error::other(
                String::from_utf8_lossy(&reason).into_owned(),
            ))
        }
        _ => Err(io::Error::new(ErrorKind::InvalidData, "not an adb server")),
    }
}

fn hex_len(digits: &[u8; 4]) -> io::Result<usize> {
    std::str::from_utf8(digits)
        .ok()
        .and_then(|d| usize::from_str_radix(d, 16).ok())
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "bad adb length"))
}

/// One complete length-prefixed message from the front of `pending`, if it has arrived.
fn take_message(pending: &mut Vec<u8>) -> io::Result<Option<String>> {
    let Some(prefix) = pending.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = hex_len(prefix)?;
    if pending.len() < 4 + len {
        return Ok(None);
    }
    let message = String::from_utf8_lossy(&pending[4..4 + len]).into_owned();
    pending.drain(..4 + len);
    Ok(Some(message))
}

/// Serials of the devices adb can talk to: `<serial>\t<state>` lines in state `device`.
fn ready_devices(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, state)| state.trim() == "device")
        .map(|(serial, _)| serial.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn only_devices_ready_for_adb_are_tunnelled() {
        let list = "1WMHH000000001\tdevice\nemulator-5554\toffline\nABC\tunauthorized\n";
        assert_eq!(ready_devices(list), ["1WMHH000000001"]);
        assert!(ready_devices("").is_empty());
    }

    #[test]
    fn messages_are_taken_whole() {
        let mut pending = b"0005abc".to_vec();
        assert_eq!(take_message(&mut pending).unwrap(), None);
        pending.extend_from_slice(b"de0000");
        assert_eq!(
            take_message(&mut pending).unwrap().as_deref(),
            Some("abcde")
        );
        assert_eq!(take_message(&mut pending).unwrap().as_deref(), Some(""));
        assert!(pending.is_empty());
        assert!(take_message(&mut b"zzzz".to_vec()).is_err());
    }

    #[derive(Clone, Default)]
    struct Recorder {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Tool for Recorder {
        fn reverse(&self, serial: &str, device_port: u16, host_port: u16) -> io::Result<()> {
            let call = format!("reverse {serial} tcp:{device_port} tcp:{host_port}");
            self.calls.lock().unwrap().push(call);
            Ok(())
        }
        fn reverse_list(&self, serial: &str) -> io::Result<String> {
            Ok(format!("{serial} tcp:47800 tcp:51000\n"))
        }
        fn start_server(&self) -> io::Result<()> {
            self.calls.lock().unwrap().push("start-server".into());
            Ok(())
        }
    }

    impl Recorder {
        fn wait_for(&self, count: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let calls = self.calls.lock().unwrap().clone();
                if calls.len() >= count || Instant::now() > deadline {
                    return calls;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// A fake adb server: accepts track-devices subscribers and pushes the lists it is given.
    fn fake_server() -> (SocketAddr, mpsc::Sender<Option<&'static str>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (lists, receive) = mpsc::channel::<Option<&'static str>>();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut header = [0u8; 4];
                stream.read_exact(&mut header).unwrap();
                let mut service = vec![0u8; hex_len(&header).unwrap()];
                stream.read_exact(&mut service).unwrap();
                assert_eq!(service, b"host:track-devices");
                stream.write_all(b"OKAY").unwrap();
                // `None` drops this subscriber, as a restarting server would.
                while let Ok(Some(list)) = receive.recv() {
                    stream
                        .write_all(format!("{:04x}{list}", list.len()).as_bytes())
                        .unwrap();
                }
            }
        });
        (address, lists)
    }

    #[test]
    fn tunnels_each_attached_device_again_after_the_server_restarts() {
        let (server, lists) = fake_server();
        let tool = Recorder::default();
        let stop = Arc::new(AtomicBool::new(false));
        let keeper = Keeper {
            tool: tool.clone(),
            server,
            device_port: 47800,
            host_port: 51000,
            stop: stop.clone(),
        };
        let keeper = thread::spawn(move || keeper.run());

        lists.send(Some("")).unwrap();
        lists.send(Some("SERIAL1\toffline\n")).unwrap();
        lists.send(Some("SERIAL1\tdevice\n")).unwrap();
        assert_eq!(tool.wait_for(1), ["reverse SERIAL1 tcp:47800 tcp:51000"]);
        lists.send(Some("SERIAL1\tdevice\n")).unwrap();
        lists.send(None).unwrap();
        lists
            .send(Some("SERIAL1\tdevice\nSERIAL2\tunauthorized\n"))
            .unwrap();
        assert_eq!(
            tool.wait_for(2),
            [
                "reverse SERIAL1 tcp:47800 tcp:51000",
                "reverse SERIAL1 tcp:47800 tcp:51000"
            ],
            "the same list again sets nothing up; a reconnected server gets every device again"
        );
        stop.store(true, Ordering::Relaxed);
        keeper.join().unwrap();
    }

    #[test]
    fn starts_a_server_when_none_answers() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = closed.local_addr().unwrap();
        drop(closed);
        let tool = Recorder::default();
        let stop = Arc::new(AtomicBool::new(false));
        let keeper = Keeper {
            tool: tool.clone(),
            server,
            device_port: 47800,
            host_port: 47800,
            stop: stop.clone(),
        };
        let keeper = thread::spawn(move || keeper.run());
        assert_eq!(tool.wait_for(1), ["start-server"]);
        stop.store(true, Ordering::Relaxed);
        keeper.join().unwrap();
    }
}
