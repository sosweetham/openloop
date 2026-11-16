//! Stands in for a headset on the USB port: sends a Quest 3 profile and still tracking at 90 Hz,
//! and reports the video that comes back. Run it next to an OpenXR app on Tether:
//!   cargo run -p tether-stream --example fake_headset -- [seconds]

use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use tether_protocol::{
    ClientHello, ControllerState, DeviceProfile, Message, PROTOCOL_VERSION, Pose, Tracking,
    USB_PORT,
};
use tether_stream::link::{Link, StreamLink};
use tether_stream::pose::default_views;
use tether_stream::time::now_ns;

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let link = StreamLink::new(
        TcpStream::connect(("127.0.0.1", USB_PORT)).expect("Tether is not listening"),
    )
    .expect("link");
    let link = std::sync::Arc::new(link);
    link.send(&Message::ClientHello(ClientHello {
        version: PROTOCOL_VERSION,
        profile: DeviceProfile {
            name: "Fake Quest 3".into(),
            eye_width: 1680,
            eye_height: 1760,
            refresh_rates: vec![72.0, 90.0, 120.0],
            refresh_rate: 90.0,
            interaction_profile: "/interaction_profiles/oculus/touch_controller".into(),
            supports_hevc: true,
        },
    }))
    .expect("hello");

    let sender = link.clone();
    thread::spawn(move || {
        let head = Pose {
            position: [0.0, 1.6, 0.0],
            ..Pose::IDENTITY
        };
        loop {
            let tracking = Tracking {
                sample_time_ns: now_ns(),
                head,
                views: default_views(),
                controllers: [ControllerState::default(); 2],
                ..Default::default()
            };
            if sender.send(&Message::Tracking(Box::new(tracking))).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(11));
        }
    });

    let start = Instant::now();
    let (mut frames, mut keyframes, mut bytes, mut last_index) = (0u64, 0u64, 0usize, None::<u64>);
    let mut gaps = 0;
    while start.elapsed() < Duration::from_secs(seconds) {
        match link.recv() {
            Ok(Message::ServerHello(h)) => println!("video {}x{}", h.video_width, h.video_height),
            Ok(Message::ClockPing { mac_ns }) => {
                let _ = link.send(&Message::ClockPong {
                    mac_ns,
                    client_ns: now_ns(),
                });
            }
            Ok(Message::VideoFrame(f)) => {
                if last_index.is_some_and(|l| f.frame_index != l + 1) {
                    gaps += 1;
                }
                last_index = Some(f.frame_index);
                frames += 1;
                keyframes += f.keyframe as u64;
                bytes += f.data.len();
            }
            Ok(_) => {}
            Err(e) => {
                println!("link closed: {e}");
                break;
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "{frames} frames in {secs:.1} s ({:.1} fps), {keyframes} keyframes, {gaps} index gaps, {:.1} Mbps",
        frames as f64 / secs,
        bytes as f64 * 8.0 / secs / 1e6
    );
}
