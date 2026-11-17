//! The fake headset against a scripted Mac on an ephemeral loopback port.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tether_fake_headset::clock::monotonic_ns;
use tether_fake_headset::report::KeyframeReason;
use tether_fake_headset::{
    Action, ClockModel, Config, Cue, FakeHeadset, Hand, Motion, Script, quest_3, run,
};
use tether_protocol::{
    AudioFormat, AudioHeader, AudioPacket, ClientHello, ClientTime, Haptics, Message,
    PROTOCOL_VERSION, ServerHello, StreamDecoder, Tracking, VideoFrame, button, encode_framed,
};

const MS: i64 = 1_000_000;
const AUDIO: AudioFormat = AudioFormat {
    stream: 1,
    sample_rate: 48_000,
    channels: 2,
};

fn listen() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

type Received = Arc<Mutex<Vec<(i64, Message)>>>;

/// The Mac's end of one link. A reader thread keeps everything the headset sends, stamped
/// with its arrival.
struct Mac {
    stream: TcpStream,
    received: Received,
    reader: Option<JoinHandle<()>>,
    /// How far `wait_for` has looked through `received`.
    seen: usize,
}

impl Mac {
    fn accept(listener: &TcpListener) -> Self {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("no headset connected: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_nodelay(true).unwrap();
        let received = Received::default();
        let reader = {
            let (mut stream, received) = (stream.try_clone().unwrap(), received.clone());
            thread::spawn(move || {
                let mut decoder = StreamDecoder::default();
                let mut buffer = vec![0u8; 64 << 10];
                while let Ok(n @ 1..) = stream.read(&mut buffer) {
                    let arrival = monotonic_ns();
                    decoder.push(&buffer[..n]);
                    while let Some(m) = decoder.next_message().expect("the headset's framing") {
                        received.lock().unwrap().push((arrival, m));
                    }
                }
            })
        };
        Self {
            stream,
            received,
            reader: Some(reader),
            seen: 0,
        }
    }

    fn send(&mut self, message: &Message) {
        let mut bytes = Vec::new();
        encode_framed(message, &mut bytes);
        self.stream.write_all(&bytes).unwrap();
    }

    /// The next message not yet waited for that matches, and when it arrived.
    fn wait_for(&mut self, what: &str, matches: impl Fn(&Message) -> bool) -> (i64, Message) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            {
                let received = self.received.lock().unwrap();
                while self.seen < received.len() {
                    self.seen += 1;
                    let (at, m) = &received[self.seen - 1];
                    if matches(m) {
                        return (*at, m.clone());
                    }
                }
            }
            assert!(Instant::now() < deadline, "no {what} from the headset");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn tracking(&self) -> Vec<(i64, Tracking)> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(at, m)| match m {
                Message::Tracking(t) => Some((*at, (**t).clone())),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Access units shaped like Tether's Annex-B HEVC output.
fn access_unit(keyframe: bool) -> Vec<u8> {
    let mut au = Vec::new();
    let units: &[(u8, &[u8])] = if keyframe {
        &[
            (32, &[0x0c]),
            (33, &[0x01, 0x60]),
            (34, &[0xc1]),
            (19, &[0xaf, 0x80]),
        ]
    } else {
        &[(1, &[0xd0, 0x40])]
    };
    for (nal, body) in units {
        au.extend_from_slice(&[0, 0, 0, 1, nal << 1, 1]);
        au.extend_from_slice(body);
    }
    au
}

fn frame(index: u64, keyframe: bool, display_time_ns: i64) -> Message {
    Message::VideoFrame(VideoFrame {
        frame_index: index,
        display_time_ns,
        keyframe,
        views: tether_fake_headset::script::eyes(),
        data: access_unit(keyframe),
    })
}

fn audio(format: AudioFormat, sequence: u32, present_at: i64) -> Message {
    Message::Audio(AudioPacket {
        header: AudioHeader {
            format,
            sequence,
            first_frame: u64::from(sequence) * 240,
            present_at: ClientTime(present_at),
        },
        samples: vec![0; 480],
    })
}

fn hello(width: u32) -> Message {
    Message::ServerHello(ServerHello {
        version: PROTOCOL_VERSION,
        video_width: width,
        video_height: 1760,
    })
}

fn steady(offset_ns: i64) -> Config {
    Config {
        clock: ClockModel {
            offset_ns,
            drift_ppm: 0.0,
        },
        ..Config::new(0)
    }
}

fn connect(listener_port: u16, config: Config) -> FakeHeadset {
    FakeHeadset::connect(Config {
        port: listener_port,
        ..config
    })
    .unwrap()
}

#[test]
fn a_clean_session_passes_and_accounts_for_everything() {
    let (listener, port) = listen();
    let offset = 5_000 * MS;
    let headset = connect(port, steady(offset));
    let mut mac = Mac::accept(&listener);
    let client_now = || monotonic_ns() + offset;

    let (_, Message::ClientHello(ClientHello { version, profile })) =
        mac.wait_for("hello", |m| matches!(m, Message::ClientHello(_)))
    else {
        unreachable!()
    };
    assert_eq!((version, profile), (PROTOCOL_VERSION, quest_3()));

    let mac_ns = monotonic_ns();
    mac.send(&Message::ClockPing { mac_ns });
    let (
        _,
        Message::ClockPong {
            mac_ns: echoed,
            client_ns,
        },
    ) = mac.wait_for("pong", |m| matches!(m, Message::ClockPong { .. }))
    else {
        unreachable!()
    };
    assert_eq!(echoed, mac_ns);
    assert!(
        (0..1_000 * MS).contains(&(client_ns - mac_ns - offset)),
        "pongs are stamped on the headset clock"
    );

    mac.send(&hello(3360));
    for i in 0..30 {
        mac.send(&frame(i, i == 0, client_now() + 30 * MS));
        thread::sleep(Duration::from_millis(11));
    }
    mac.wait_for("keyframe request", |m| *m == Message::KeyframeRequest);

    mac.send(&Message::Haptics(Haptics {
        hand: 1,
        amplitude: 0.5,
        duration_ns: 20 * MS,
        frequency: 0.0,
    }));
    let mut unknown = 4u32.to_le_bytes().to_vec();
    unknown.extend_from_slice(&[250, 1, 2, 3]);
    mac.stream.write_all(&unknown).unwrap();
    mac.send(&Message::AudioStart(AUDIO));
    let base = client_now() + 40 * MS;
    for i in 0..10 {
        mac.send(&audio(AUDIO, i, base + i64::from(i) * 5 * MS));
    }
    mac.send(&Message::AudioStop { stream: 1 });
    mac.send(&Message::StreamEnded);

    mac.send(&hello(2560));
    for i in 0..5 {
        mac.send(&frame(i, i == 0, client_now() + 30 * MS));
        thread::sleep(Duration::from_millis(11));
    }
    assert!(headset.wait_for(Duration::from_secs(3), |r| r.video.frames == 35));
    thread::sleep(Duration::from_millis(100));
    let report = headset.finish();

    assert!(report.passed, "{:#?}", report.errors);
    assert!(report.warnings.is_empty(), "{:#?}", report.warnings);
    assert_eq!((report.video.frames, report.video.keyframes), (35, 2));
    let streams: Vec<_> = report
        .video
        .streams
        .iter()
        .map(|s| {
            (
                s.video_width,
                s.frames,
                s.began_with_keyframe,
                s.ended_at_s.is_some(),
            )
        })
        .collect();
    assert_eq!(
        streams,
        [(3360, 30, Some(true), true), (2560, 5, Some(true), true)]
    );
    assert!((-100.0..=30.001).contains(&report.video.display_lead_ms.p50));
    assert_eq!(report.stream_ended_at_s.len(), 1);
    assert_eq!(report.haptics.len(), 1);
    assert_eq!(report.haptics[0].hand, 1);
    assert_eq!(
        (
            report.audio.starts,
            report.audio.packets,
            report.audio.frames,
            report.audio.stops
        ),
        (1, 10, 2400, 1)
    );
    assert!(report.audio.schedule_error_ms.max.abs() < 1e-6);
    assert_eq!(report.audio.spacing_error_ms.count, 9);
    assert!(report.audio.spacing_error_ms.max.abs() < 1e-6);
    assert!((-100.0..=40.001).contains(&report.audio.lead_ms.min));
    assert!(report.audio.streams[0].ended_at_s.is_some());
    assert_eq!(report.unknown_messages_skipped, 1);
    assert_eq!(report.clock_pings.answered, 1);
    let starts: Vec<_> = report
        .keyframe_requests
        .iter()
        .map(|k| (k.reason, k.answered_after_frames))
        .collect();
    assert_eq!(
        starts,
        [
            (KeyframeReason::StreamStart, Some(1)),
            (KeyframeReason::StreamStart, Some(1))
        ]
    );

    let tracking = mac.tracking();
    let (first, last) = (tracking.first().unwrap().0, tracking.last().unwrap().0);
    let rate = (tracking.len() - 1) as f64 / ((last - first) as f64 / 1e9);
    assert!((60.0..=110.0).contains(&rate), "tracking at {rate:.1} Hz");
    assert_eq!(report.tracking_sent as usize, tracking.len());
    for pair in tracking.windows(2) {
        assert!(pair[1].1.sample_time_ns > pair[0].1.sample_time_ns);
    }
    let stamped_ahead = Config::new(0).tracking_lead.as_nanos() as i64;
    for (arrival, t) in &tracking {
        let lead = t.sample_time_ns - (arrival + offset);
        assert!(
            (-100 * MS..=stamped_ahead).contains(&lead),
            "stamped ahead on the headset clock: {lead}"
        );
        let norm: f32 = t.head.orientation.iter().map(|q| q * q).sum();
        assert!((norm - 1.0).abs() < 1e-4);
        assert!(t.controllers.iter().all(|c| c.active));
    }
}

#[test]
fn protocol_violations_are_errors() {
    let (listener, port) = listen();
    let offset = -2_000 * MS;
    let headset = connect(port, steady(offset));
    let mut mac = Mac::accept(&listener);
    mac.wait_for("hello", |m| matches!(m, Message::ClientHello(_)));
    let client_now = || monotonic_ns() + offset;

    mac.send(&frame(0, true, client_now() + 30 * MS));
    mac.send(&Message::ServerHello(ServerHello {
        version: 99,
        video_width: 3360,
        video_height: 1760,
    }));
    mac.send(&hello(3360));
    mac.send(&frame(5, false, client_now() + 30 * MS));
    mac.send(&Message::VideoFrame(VideoFrame {
        keyframe: true,
        ..match frame(5, false, client_now() + 31 * MS) {
            Message::VideoFrame(f) => f,
            _ => unreachable!(),
        }
    }));
    mac.send(&frame(6, false, client_now() - 10_000 * MS));
    mac.send(&Message::Haptics(Haptics {
        hand: 7,
        amplitude: 0.5,
        duration_ns: 0,
        frequency: 0.0,
    }));
    let other = AudioFormat {
        stream: 9,
        sample_rate: 44_100,
        channels: 2,
    };
    mac.send(&audio(other, 0, client_now() + 40 * MS));
    mac.send(&audio(
        AudioFormat {
            channels: 0,
            ..AUDIO
        },
        0,
        0,
    ));
    mac.send(&Message::Recenter);
    mac.send(&Message::ServerHello(ServerHello {
        version: PROTOCOL_VERSION,
        video_width: 0,
        video_height: 0,
    }));

    assert!(headset.wait_for(Duration::from_secs(3), |r| r.error_count >= 13));
    let report = headset.finish();
    assert!(!report.passed);
    for expected in [
        "frame 0 arrived outside a stream",
        "ServerHello speaks protocol 99",
        "began with frame 5, which is not a keyframe",
        "frame 5 is flagged keyframe=true",
        "frame index went from 5 to 5",
        "frame 6 is to be displayed -100",
        "frame 6's display time is -100",
        "haptics with hand 7",
        "stream 9, which was never started",
        "is 44100 Hz × 2, the headset plays 48000 Hz × 2",
        "malformed message: Malformed",
        "the Mac sent Recenter, which only a headset sends",
        "ServerHello announces a 0x0 stream",
    ] {
        assert!(
            report.errors.iter().any(|e| e.contains(expected)),
            "missing {expected:?} in {:#?}",
            report.errors
        );
    }
    assert_eq!(report.error_count, 13, "{:#?}", report.errors);
}

#[test]
fn frames_that_waited_out_a_stall_are_late_not_wrong() {
    let (listener, port) = listen();
    let offset = 5_000 * MS;
    let headset = connect(port, steady(offset));
    let mut mac = Mac::accept(&listener);
    mac.wait_for("hello", |m| matches!(m, Message::ClientHello(_)));
    let client_now = || monotonic_ns() + offset;
    mac.send(&hello(3360));
    mac.send(&frame(0, true, client_now() + 30 * MS));
    assert!(headset.wait_for(Duration::from_secs(3), |r| r.video.frames == 1));

    headset.act(Action::PauseReading(Duration::from_millis(500)));
    for i in 1..10 {
        mac.send(&frame(i, false, client_now() + 30 * MS));
        thread::sleep(Duration::from_millis(11));
    }
    assert!(headset.wait_for(Duration::from_secs(3), |r| r.video.frames == 10));
    let report = headset.finish();
    assert!(report.passed, "{:#?}", report.errors);
    assert!(report.video.late_frames >= 5, "{:?}", report.video);
    assert!(report.video.display_lead_ms.min < -100.0);
    assert_eq!(report.video.index_gaps, 0);
}

#[test]
fn pongs_carry_the_injected_offset_and_drift() {
    let (listener, port) = listen();
    let model = ClockModel {
        offset_ns: -3_000 * MS,
        drift_ppm: 20_000.0,
    };
    let started = monotonic_ns();
    let _headset = connect(
        port,
        Config {
            clock: model,
            ..Config::new(0)
        },
    );
    let mut mac = Mac::accept(&listener);
    // A pong is stamped after its ping is sent and before it arrives back, however late the
    // threads run.
    let ping = |mac: &mut Mac| {
        let sent = monotonic_ns();
        mac.send(&Message::ClockPing { mac_ns: sent });
        match mac.wait_for("pong", |m| matches!(m, Message::ClockPong { .. })) {
            (arrived, Message::ClockPong { client_ns, .. }) => (sent, arrived, client_ns),
            _ => unreachable!(),
        }
    };
    let (sent_0, arrived_0, client_0) = ping(&mut mac);
    thread::sleep(Duration::from_secs(1));
    let (sent_1, arrived_1, client_1) = ping(&mut mac);

    let rate = model.drift_ppm * 1e-6;
    let drift_so_far = ((arrived_0 - started) as f64 * rate) as i64 + 1;
    assert!(
        (sent_0..=arrived_0 + drift_so_far).contains(&(client_0 - model.offset_ns)),
        "the first pong carries the offset"
    );
    let ran = (client_1 - client_0) as f64;
    let (least, most) = ((sent_1 - arrived_0) as f64, (arrived_1 - sent_0) as f64);
    assert!(
        (least * (1.0 + rate) - 2.0..=most * (1.0 + rate) + 2.0).contains(&ran),
        "the headset clock ran {ran} ns while {least}..{most} ns passed"
    );
    assert!(ran > most, "and it ran fast");
}

#[test]
fn cues_and_calls_reach_the_mac() {
    let (listener, port) = listen();
    let at = |ms| Duration::from_millis(ms);
    let script = Script {
        motion: Motion::Still,
        cues: vec![
            Cue {
                at: at(250),
                action: Action::Press {
                    hand: Hand::Left,
                    button: button::MENU,
                    hold: at(100),
                },
            },
            Cue {
                at: at(50),
                action: Action::Squeeze {
                    hand: Hand::Right,
                    value: 1.0,
                },
            },
            Cue {
                at: at(100),
                action: Action::Recenter,
            },
            Cue {
                at: at(150),
                action: Action::Focus(false),
            },
            Cue {
                at: at(200),
                action: Action::RequestKeyframe,
            },
        ],
    };
    let headset = connect(
        port,
        Config {
            script,
            ..Config::new(0)
        },
    );
    let mut mac = Mac::accept(&listener);

    mac.wait_for("squeeze", |m| {
        matches!(m, Message::Tracking(t) if t.controllers[1].squeeze == 1.0
            && t.controllers[1].buttons & button::SQUEEZE != 0)
    });
    mac.wait_for("recenter", |m| *m == Message::Recenter);
    mac.wait_for("focus loss", |m| {
        *m == Message::InputFocus { focused: false }
    });
    mac.wait_for("keyframe request", |m| *m == Message::KeyframeRequest);
    let (pressed, _) = mac.wait_for(
        "menu press",
        |m| matches!(m, Message::Tracking(t) if t.controllers[0].buttons & button::MENU != 0),
    );
    let (released, _) = mac.wait_for(
        "menu release",
        |m| matches!(m, Message::Tracking(t) if t.controllers[0].buttons & button::MENU == 0),
    );
    let held = (released - pressed) as f64 / 1e6;
    assert!((60.0..=300.0).contains(&held), "menu held {held:.1} ms");

    headset.act(Action::Focus(true));
    mac.wait_for("focus", |m| *m == Message::InputFocus { focused: true });
    let report = headset.finish();
    assert_eq!(report.actions.len(), 6);
    assert_eq!(report.keyframe_requests.len(), 1);
    assert_eq!(report.keyframe_requests[0].reason, KeyframeReason::Scripted);
}

#[test]
fn a_paused_headset_stops_reading_and_then_catches_up() {
    let (listener, port) = listen();
    let script = Script {
        motion: Motion::Still,
        cues: vec![Cue {
            at: Duration::from_millis(100),
            action: Action::PauseReading(Duration::from_millis(1_000)),
        }],
    };
    let before = monotonic_ns();
    let headset = connect(
        port,
        Config {
            script,
            ..Config::new(0)
        },
    );
    let paused_until = before + 1_100 * MS;
    let mut mac = Mac::accept(&listener);
    mac.wait_for("hello", |m| matches!(m, Message::ClientHello(_)));
    thread::sleep(Duration::from_millis(300));

    let sent = monotonic_ns();
    assert!(sent < paused_until, "the ping goes out during the pause");
    mac.send(&Message::ClockPing { mac_ns: sent });
    let (answered, _) = mac.wait_for("pong", |m| matches!(m, Message::ClockPong { .. }));
    assert!(
        answered >= paused_until,
        "answered only once reading resumed"
    );
    let tracked = mac.tracking();
    assert!(
        tracked.iter().any(|(at, _)| *at > sent && *at < answered),
        "tracking keeps flowing while reads are paused"
    );
    drop(headset);
}

#[test]
fn a_dropped_link_is_reconnected_as_the_quest_does_and_ends_its_streams() {
    let (listener, port) = listen();
    let headset = connect(port, steady(0));
    let mut first = Mac::accept(&listener);
    first.wait_for("hello", |m| matches!(m, Message::ClientHello(_)));
    first.send(&hello(3360));
    first.send(&frame(0, true, monotonic_ns() + 30 * MS));
    first.send(&Message::AudioStart(AUDIO));
    first.send(&audio(AUDIO, 0, monotonic_ns() + 40 * MS));
    assert!(headset.wait_for(Duration::from_secs(3), |r| r.audio.packets == 1));
    drop(first);

    let mut second = Mac::accept(&listener);
    second.wait_for("hello again", |m| matches!(m, Message::ClientHello(_)));
    second.wait_for("tracking", |m| matches!(m, Message::Tracking(_)));
    second.send(&hello(3360));
    second.send(&frame(0, true, monotonic_ns() + 30 * MS));
    second.send(&Message::AudioStart(AUDIO));
    second.send(&audio(AUDIO, 0, monotonic_ns() + 40 * MS));
    assert!(headset.wait_for(Duration::from_secs(3), |r| r.audio.packets == 2));
    let live = headset.report();
    let video: Vec<_> = live
        .video
        .streams
        .iter()
        .map(|s| (s.frames, s.ended_at_s.is_some()))
        .collect();
    assert_eq!(video, [(1, true), (1, false)]);
    let audio: Vec<_> = live
        .audio
        .streams
        .iter()
        .map(|s| (s.packets, s.ended_at_s.is_some()))
        .collect();
    assert_eq!(audio, [(1, true), (1, false)]);

    let report = headset.finish();
    assert_eq!((report.link.connects, report.link.drops), (2, 1));
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("the Mac closed the link"))
    );
    assert!(
        report.passed,
        "a dropped link is a warning: {:#?}",
        report.errors
    );
    assert!(
        report.video.streams.iter().all(|s| s.ended_at_s.is_some()),
        "the session's end ends them"
    );
}

#[test]
fn a_headset_with_no_mac_reports_why() {
    let port = listen().1;
    let started = Instant::now();
    let report = run(
        Config {
            connect_timeout: Duration::from_millis(300),
            ..Config::new(port)
        },
        Duration::from_secs(10),
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!report.passed);
    assert_eq!(report.link.connects, 0);
    assert!(
        report.errors[0].contains("could not connect"),
        "{:?}",
        report.errors
    );
}

#[test]
fn the_command_line_writes_the_report_the_e2e_run_reads() {
    let (listener, port) = listen();
    let path =
        std::env::temp_dir().join(format!("tether-fake-headset-{}.json", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether-fake-headset"))
        .args(["--port", &port.to_string(), "--seconds", "2", "--report"])
        .arg(&path)
        .env_remove("TETHER_USB_PORT")
        .spawn()
        .unwrap();
    let mut mac = Mac::accept(&listener);
    mac.wait_for("hello", |m| matches!(m, Message::ClientHello(_)));
    let mac_ns = monotonic_ns();
    mac.send(&Message::ClockPing { mac_ns });
    let (_, Message::ClockPong { client_ns, .. }) =
        mac.wait_for("pong", |m| matches!(m, Message::ClockPong { .. }))
    else {
        unreachable!()
    };
    let offset = client_ns - mac_ns;
    mac.send(&hello(3360));
    for i in 0..20 {
        mac.send(&frame(i, i == 0, monotonic_ns() + offset + 30 * MS));
        thread::sleep(Duration::from_millis(11));
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");

    let json = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["passed"], true, "{json}");
    assert_eq!(report["protocol_version"], u64::from(PROTOCOL_VERSION));
    assert_eq!(report["port"], u64::from(port));
    assert_eq!(report["video"]["frames"], 20);
    assert_eq!(report["video"]["streams"][0]["began_with_keyframe"], true);
    assert_eq!(report["profile"]["refresh_rate"], 90.0);
    assert!(report["duration_s"].as_f64().unwrap() >= 2.0);
}
