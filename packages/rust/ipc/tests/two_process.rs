//! The runtime and `openloopd` in two processes: this test is the app, and launchd runs the test
//! service under a throwaway `dev.openloop.test.*` label, which is booted out and deleted however
//! the test ends.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use objc2_metal::{
    MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLSharedEvent, MTLTexture,
    MTLTextureType, MTLTextureUsage,
};
use openloop_ipc::control::{CallError, Client, ConnectOptions, SwapchainDesc};
use openloop_ipc::records::{AccessUnit, Message};
use openloop_ipc::testing::{self, Stats, fill_au_bytes, is_stamped, probe};
use openloop_ipc::{AccessUnits, PushError, ReadError, SessionRegion, Wake, now_ns};

const SIGNING_IDENTIFIER: &str = "dev.openloop.test.ipc-service";

/// A launchd agent for the test service. Dropping it boots the agent out and deletes its files.
struct Agent {
    label: String,
    domain: String,
    dir: PathBuf,
}

impl Agent {
    /// `None` without a GUI login session (over ssh, say), whose launchd domain agents need.
    fn start() -> Option<Agent> {
        let unique = format!("{}.{}", std::process::id(), now_ns());
        let label = format!("dev.openloop.test.ipc.{unique}");
        // SAFETY: no preconditions.
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let printed = Command::new("launchctl")
            .args(["print", &domain])
            .output()
            .unwrap();
        if !printed.status.success() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!("openloop-ipc-test-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let agent = Agent { label, domain, dir };

        // A copy, so the build's own binary keeps its linker signature.
        let binary = agent.dir.join("openloop-ipc-test-service");
        std::fs::copy(env!("CARGO_BIN_EXE_openloop-ipc-test-service"), &binary).unwrap();
        run(
            "codesign",
            &["-s", "-", "-f", "-i", SIGNING_IDENTIFIER, path(&binary)],
        );

        let plist = agent.plist();
        std::fs::write(
            &plist,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{binary}</string><string>{label}</string></array>
  <key>MachServices</key><dict><key>{label}</key><true/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
                label = agent.label,
                binary = path(&binary),
                log = path(&agent.log()),
            ),
        )
        .unwrap();
        run("launchctl", &["bootstrap", &agent.domain, path(&plist)]);
        Some(agent)
    }

    fn plist(&self) -> PathBuf {
        self.dir.join(format!("{}.plist", self.label))
    }

    fn log(&self) -> PathBuf {
        self.dir.join("service.log")
    }

    fn service(&self) -> &str {
        &self.label
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let target = format!("{}/{}", self.domain, self.label);
        let status = Command::new("launchctl")
            .args(["bootout", &target])
            .status();
        eprintln!("launchctl bootout {target}: {status:?}");
        if let Ok(log) = std::fs::read_to_string(self.log()) {
            eprintln!("service log:\n{log}");
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn options(requirement: &str) -> ConnectOptions {
    ConnectOptions {
        code_signing_requirement: Some(requirement.to_owned()),
        timeout: testing::PATIENCE,
        ..ConnectOptions::default()
    }
}

/// Sends `message` and waits on the app's doorbell for the service's answer.
fn ask(region: &SessionRegion, message: Message) -> Message {
    let mut producer = region.to_openloopd.producer();
    let mut consumer = region.to_app.consumer();
    let seen = region.app_bell.rings();
    region
        .to_openloopd
        .push(&mut producer, &message.into())
        .unwrap();
    region.openloopd_bell.ring();
    let deadline = now_ns() + testing::PATIENCE.as_nanos() as i64;
    let mut seen = seen;
    loop {
        if let Some(event) = region.to_app.pop(&mut consumer).unwrap() {
            return event.decode().unwrap();
        }
        assert_eq!(
            region.app_bell.wait(seen, Some(deadline)).unwrap(),
            Wake::Rung,
            "no answer"
        );
        seen = region.app_bell.rings();
    }
}

#[test]
fn a_session_shared_between_two_processes() {
    let Some(agent) = Agent::start() else {
        eprintln!("skipped: no GUI launchd domain for this user");
        return;
    };
    let pinned = format!(r#"identifier "{SIGNING_IDENTIFIER}""#);
    let client = Client::connect(agent.service(), options(&pinned));

    let started = Instant::now();
    let welcome = client.hello("two_process", "none", 1).unwrap();
    eprintln!(
        "hello, including launchd starting the service: {:?}",
        started.elapsed()
    );
    let device = MTLCreateSystemDefaultDevice();
    if let Some(device) = &device {
        assert_eq!(
            welcome.registry_id,
            device.registryID(),
            "the service is on another GPU"
        );
    }
    assert_eq!(welcome.headset, Some(testing::headset_info()));
    let peer = client.peer();
    assert_ne!(peer.pid, std::process::id() as i32);
    // SAFETY: no preconditions.
    assert_eq!(peer.euid, unsafe { libc::geteuid() });
    eprintln!("service peer: {peer:?}");

    let mut calls = Stats::with_capacity(200);
    for _ in 0..200 {
        let start = now_ns();
        client.headset().unwrap();
        calls.push((now_ns() - start) as u64);
    }
    eprintln!("{}", calls.summary("NSXPC call round trip (headset)"));

    let session = client.create_session().unwrap();
    client.begin_session(session.session).unwrap();
    let region = &session.region;
    assert_eq!(region.info().boot_id, welcome.boot_id);

    // Records the service published, and the events it queued, before answering beginSession.
    assert_eq!(
        region.headset.latest().unwrap().value,
        testing::headset_info()
    );
    assert_eq!(
        region.clock.latest().unwrap().value,
        testing::clock_mapping()
    );
    assert_eq!(
        region.timing.latest().unwrap().value,
        testing::frame_timing()
    );
    assert_eq!(
        region.state.latest().unwrap().value,
        testing::session_state()
    );
    let mut events = region.to_app.consumer();
    let mut queued = Vec::new();
    while let Some(event) = region.to_app.pop(&mut events).unwrap() {
        queued.push(event.decode().unwrap());
    }
    assert_eq!(
        queued,
        [
            Message::Focus {
                focused: true,
                at_mac_ns: 5
            },
            Message::Recenter {
                count: 1,
                at_mac_ns: 4
            }
        ]
    );

    ping_pong(region);
    access_units(region);
    reads_under_a_flooding_writer(region);

    if let (Some(device), Some(handle)) = (&device, &session.app_ready) {
        let app_ready = device.newSharedEventWithHandle(handle).unwrap();
        app_ready.setSignaledValue(41);
        assert_eq!(
            ask(
                region,
                Message::Echo {
                    value: probe::APP_READY,
                    sent_mac_ns: 0
                }
            ),
            Message::Echo {
                value: 41,
                sent_mac_ns: 0
            }
        );
        swapchain(device.as_ref(), &client, session.session);
    }

    client.end_session(session.session).unwrap();
    client.destroy_session(session.session).unwrap();
    drop(client);

    let unpinned = Client::connect(
        agent.service(),
        options(r#"identifier "dev.openloop.not-openloopd""#),
    );
    let refused = unpinned.hello("two_process", "none", 1);
    eprintln!("wrong code-signing requirement: {refused:?}");
    assert!(
        matches!(refused, Err(CallError::Connection { code: 4102, .. })),
        "{refused:?}"
    );
}

fn ping_pong(region: &SessionRegion) {
    const ROUNDS: usize = 20_000;
    let mut producer = region.to_openloopd.producer();
    let mut consumer = region.to_app.consumer();
    let (mut wake, mut round_trip) = (Stats::with_capacity(ROUNDS), Stats::with_capacity(ROUNDS));
    for _ in 0..ROUNDS {
        let seen = region.app_bell.rings();
        let sent = now_ns();
        region
            .to_openloopd
            .push(
                &mut producer,
                &Message::Echo {
                    value: 0,
                    sent_mac_ns: sent,
                }
                .into(),
            )
            .unwrap();
        region.openloopd_bell.ring();
        let deadline = sent + testing::PATIENCE.as_nanos() as i64;
        let mut seen = seen;
        let answer = loop {
            if let Some(event) = region.to_app.pop(&mut consumer).unwrap() {
                break event.decode().unwrap();
            }
            assert_eq!(
                region.app_bell.wait(seen, Some(deadline)).unwrap(),
                Wake::Rung
            );
            seen = region.app_bell.rings();
        };
        let back = now_ns();
        let Message::Echo {
            value: woke,
            sent_mac_ns,
        } = answer
        else {
            panic!("{answer:?}")
        };
        assert_eq!(sent_mac_ns, sent);
        wake.push((woke as i64 - sent) as u64);
        round_trip.push((back - sent) as u64);
    }
    eprintln!(
        "{}",
        wake.summary("cross-process push + ring -> service thread running")
    );
    eprintln!(
        "{}",
        round_trip.summary("cross-process event round trip through both doorbells")
    );
}

fn access_units(region: &SessionRegion) {
    const UNITS: u64 = 600;
    let mut producer = region.access_units.producer();
    let mut bytes = vec![0; 2 << 20];
    let mut total = 0;
    let started = Instant::now();
    for frame in 0..UNITS {
        // Mostly delta-frame sizes, a 2 MiB keyframe every 60, and sizes that end mid-slot.
        let keyframe = frame % 60 == 0;
        let len = if keyframe {
            2 << 20
        } else {
            1 + (frame as usize * 7919) % 300_000
        };
        fill_au_bytes(frame, &mut bytes[..len]);
        let unit = AccessUnit {
            frame_index: frame,
            flags: if keyframe { AccessUnit::KEYFRAME } else { 0 },
            ..AccessUnit::default()
        };
        loop {
            match region.access_units.reserve(&mut producer, len) {
                Ok(mut writer) => {
                    writer.write(&bytes[..len]).unwrap();
                    writer.commit(&unit);
                    break;
                }
                Err(PushError::Full) => std::thread::yield_now(),
                Err(error) => panic!("{error:?}"),
            }
        }
        region.openloopd_bell.ring();
        total += len;
    }
    let answer = ask(
        region,
        Message::Echo {
            value: probe::ACCESS_UNITS,
            sent_mac_ns: 0,
        },
    );
    let elapsed = started.elapsed();
    eprintln!(
        "access units: {UNITS} ({} MiB, the ring {} MiB) in {elapsed:?}, {:.0} MiB/s; service says {answer:?}",
        total >> 20,
        AccessUnits::MAX_BYTES >> 20,
        total as f64 / (1 << 20) as f64 / elapsed.as_secs_f64()
    );
    assert_eq!(
        answer,
        Message::Echo {
            value: UNITS,
            sent_mac_ns: 0
        },
        "verified, corrupt"
    );
}

fn reads_under_a_flooding_writer(region: &SessionRegion) {
    ask(
        region,
        Message::Echo {
            value: probe::FLOOD_START,
            sent_mac_ns: 0,
        },
    );
    while region.tracking.generation() < 1000 {
        std::hint::spin_loop();
    }
    let (mut reads, mut contended, mut torn) = (0u64, 0u64, 0u64);
    let mut latency = Stats::with_capacity(1 << 20);
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        let start = now_ns();
        let read = region.tracking.latest();
        let took = (now_ns() - start) as u64;
        if latency.len() < 1 << 20 {
            latency.push(took);
        }
        reads += 1;
        match read {
            Ok(sample) if !is_stamped(&sample.value) => torn += 1,
            Ok(_) => {}
            Err(ReadError::Contended) => contended += 1,
            Err(error) => panic!("{error:?}"),
        }
    }
    let Message::Echo {
        value: published, ..
    } = ask(
        region,
        Message::Echo {
            value: probe::FLOOD_STOP,
            sent_mac_ns: 0,
        },
    )
    else {
        panic!("no count")
    };
    eprintln!(
        "tracking reads while the other process publishes without pause: {reads} reads, \
         {contended} contended, {torn} torn, {published} published; {}",
        latency.summary("read")
    );
    assert_eq!(torn, 0);
    assert_eq!(contended, 0);
    assert!(published > reads / 10, "the writer barely ran");
}

fn swapchain(
    device: &objc2::runtime::ProtocolObject<dyn MTLDevice>,
    client: &Client,
    session: u64,
) {
    let desc = SwapchainDesc {
        pixel_format: MTLPixelFormat::RGBA8Unorm_sRGB.0 as u64,
        width: 2064,
        height: 2208,
        array_length: 2,
        mipmap_level_count: 1,
        usage: (MTLTextureUsage::ShaderRead
            | MTLTextureUsage::RenderTarget
            | MTLTextureUsage::PixelFormatView)
            .0 as u64,
    };
    let started = Instant::now();
    let (_, images) = client.create_swapchain(session, &desc).unwrap();
    let mut opened = 0;
    for handle in &images {
        let texture = device.newSharedTextureWithHandle(handle).unwrap();
        assert_eq!(texture.textureType(), MTLTextureType::Type2DArray);
        assert_eq!(
            (texture.width(), texture.height(), texture.arrayLength()),
            (2064, 2208, 2)
        );
        opened += 1;
    }
    eprintln!(
        "swapchain of {opened} shared 2D-array textures made in the service and opened here: {:?}",
        started.elapsed()
    );
    client.destroy_swapchain(session, 1).unwrap();
}
