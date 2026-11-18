//! The link to `openloopd`. Over USB the Mac is reachable on loopback through `adb reverse`;
//! the client keeps retrying, so launch order does not matter. Audio goes from here straight
//! into the playout's ring, never through the main thread's once-per-frame drain.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use openloop_playout::PacketWriter;
use openloop_protocol::{
    AudioPacketRef, ClientHello, ClientTime, DeviceProfile, Message, PROTOCOL_VERSION,
    StreamDecoder, USB_PORT, decode, encode_framed,
};

use crate::audio::Granted;

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
    pub fn start(profile: DeviceProfile, packets: PacketWriter, granted: Arc<Granted>) -> Self {
        let writer = Arc::new(Mutex::new(None));
        let (tx, events) = mpsc::channel();
        let shared = writer.clone();
        thread::Builder::new()
            .name("openloop-net".into())
            .spawn(move || {
                let mut audio = AudioFeed {
                    packets,
                    playing: None,
                };
                loop {
                    if let Ok(stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, USB_PORT)) {
                        // A reopened output can run at another rate; each link reports the
                        // current one.
                        let (audio_sample_rate, audio_channels) = granted.get();
                        let profile = DeviceProfile {
                            audio_sample_rate,
                            audio_channels,
                            ..profile.clone()
                        };
                        let served = serve(stream, &profile, &shared, &tx, &mut audio);
                        audio.end(ClientTime(now_ns()));
                        if served.is_err() && tx.send(Event::Disconnected).is_err() {
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

/// The network thread's end of the audio playout.
struct AudioFeed {
    packets: PacketWriter,
    /// One stream plays at a time.
    playing: Option<u16>,
}

impl AudioFeed {
    fn packet(&mut self, packet: &AudioPacketRef<'_>, arrived: ClientTime) {
        self.playing = Some(packet.header.format.stream);
        self.packets.push_ref(packet, arrived);
    }

    /// `AudioStart`: the new stream replaces whatever was playing, even one of the same id.
    fn start(&mut self, stream: u16, arrived: ClientTime) {
        self.end(arrived);
        self.playing = Some(stream);
    }

    fn stop(&mut self, stream: u16, arrived: ClientTime) {
        self.packets.push_stop(stream, arrived);
        if self.playing == Some(stream) {
            self.playing = None;
        }
    }

    /// Playout ends the stream after its last received frame instead of counting the silence
    /// that follows as dropouts.
    fn end(&mut self, arrived: ClientTime) {
        if let Some(stream) = self.playing.take() {
            self.packets.push_stop(stream, arrived);
        }
    }
}

fn serve(
    stream: TcpStream,
    profile: &DeviceProfile,
    writer: &Mutex<Option<TcpStream>>,
    events: &Sender<Event>,
    audio: &mut AudioFeed,
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
    log::info!(
        "connected to the Mac; audio {} Hz, {} channels",
        profile.audio_sample_rate,
        profile.audio_channels
    );

    let mut decoder = StreamDecoder::default();
    let mut buf = vec![0u8; 256 << 10];
    let mut pong = Vec::new();
    let mut told_of_skips = false;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let arrived = ClientTime(now_ns());
        decoder.push(&buf[..n]);
        while let Some(unit) = decoder
            .next_unit()
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?
        {
            // Audio is read in place and copied into a ring slot, allocating nothing.
            let decoded = match AudioPacketRef::decode(unit) {
                Ok(Some(packet)) => {
                    audio.packet(&packet, arrived);
                    continue;
                }
                Ok(None) => decode(unit),
                Err(e) => Err(e),
            };
            let message = match decoded {
                Ok(Some(message)) => message,
                Ok(None) => {
                    if !told_of_skips {
                        log::info!(
                            "skipping messages of kinds this client does not know; the Mac is newer"
                        );
                        told_of_skips = true;
                    }
                    continue;
                }
                // The framing held, so the next message decodes.
                Err(e) => {
                    log::warn!("skipping a malformed message: {e:?}");
                    continue;
                }
            };
            match message {
                Message::ClockPing { mac_ns } => {
                    // Answered here, not on the render thread, so the round trip stays tight.
                    pong.clear();
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
                }
                Message::AudioStart(format) => audio.start(format.stream, arrived),
                Message::AudioStop { stream } => audio.stop(stream, arrived),
                message => {
                    if matches!(message, Message::StreamEnded) {
                        audio.end(arrived);
                    }
                    if events.send(Event::Message(message)).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}
