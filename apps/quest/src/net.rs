//! The link to `tetherd`. Over USB the Mac is reachable on loopback through `adb reverse`;
//! the client keeps retrying, so launch order does not matter.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tether_protocol::{
    ClientHello, DeviceProfile, Message, PROTOCOL_VERSION, StreamDecoder, USB_PORT, encode_framed,
};

pub enum Event {
    Connected,
    Message(Message),
    Disconnected,
}

pub struct Net {
    writer: Arc<Mutex<Option<TcpStream>>>,
    pub events: Receiver<Event>,
}

/// The client clock on the wire: CLOCK_MONOTONIC, nanoseconds.
pub fn now_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

impl Net {
    pub fn start(profile: DeviceProfile) -> Self {
        let writer = Arc::new(Mutex::new(None));
        let (tx, events) = mpsc::channel();
        let shared = writer.clone();
        thread::Builder::new()
            .name("tether-net".into())
            .spawn(move || {
                loop {
                    if let Ok(stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, USB_PORT)) {
                        if serve(stream, &profile, &shared, &tx).is_err()
                            && tx.send(Event::Disconnected).is_err()
                        {
                            return;
                        }
                        *shared.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            })
            .expect("spawning the network thread");
        Self { writer, events }
    }

    pub fn send(&self, message: &Message) {
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(stream) = guard.as_mut() {
            let mut bytes = Vec::new();
            encode_framed(message, &mut bytes);
            if stream.write_all(&bytes).is_err() {
                *guard = None;
            }
        }
    }
}

fn serve(
    stream: TcpStream,
    profile: &DeviceProfile,
    writer: &Mutex<Option<TcpStream>>,
    events: &Sender<Event>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = stream.try_clone()?;
    let mut hello = Vec::new();
    encode_framed(
        &Message::ClientHello(ClientHello {
            version: PROTOCOL_VERSION,
            profile: profile.clone(),
        }),
        &mut hello,
    );
    (&stream).write_all(&hello)?;
    *writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(stream);
    let _ = events.send(Event::Connected);
    log::info!("connected to the Mac");

    let mut decoder = StreamDecoder::default();
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        decoder.push(&buf[..n]);
        while let Some(message) = decoder
            .next_message()
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?
        {
            if let Message::ClockPing { mac_ns } = message {
                // Answered here, not on the render thread, so the round trip stays tight.
                let mut pong = Vec::new();
                encode_framed(
                    &Message::ClockPong {
                        mac_ns,
                        client_ns: now_ns(),
                    },
                    &mut pong,
                );
                if let Some(s) = writer.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    s.write_all(&pong)?;
                }
            } else if events.send(Event::Message(message)).is_err() {
                return Ok(());
            }
        }
    }
}
