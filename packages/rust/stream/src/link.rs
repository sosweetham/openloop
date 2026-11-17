//! Links carry protocol messages to one headset. `Transport` yields a `Link` per connecting
//! headset; USB is implemented, Wi-Fi plugs in as another `Transport`. A stream link sends whole
//! messages, so audio and control overtake video only between messages (docs/ARCHITECTURE.md
//! § Transports).

use std::ffi::OsStr;
use std::io::{self, BufWriter, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use tether_protocol::{
    AudioHeader, AudioPacket, DecodeError, Message, StreamDecoder, USB_PORT, VideoFrame,
    VideoHeader, encode_audio_framed, encode_framed, encode_video_framed,
};

const SILENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// A headset that accepts nothing for this long has stalled; the link is dropped as lost.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Moves the Mac's USB listener off [`USB_PORT`], so tests and a second checkout never take the
/// port a real headset's `adb reverse` tunnel lands on.
pub const USB_PORT_VAR: &str = "TETHER_USB_PORT";

/// The loopback port the USB listener binds: [`USB_PORT_VAR`] if set and non-empty, else
/// [`USB_PORT`]. `0` lets the kernel pick a free port, which [`UsbTransport::port`] reports.
pub fn usb_port() -> io::Result<u16> {
    port_from(std::env::var_os(USB_PORT_VAR).as_deref())
}

fn port_from(value: Option<&OsStr>) -> io::Result<u16> {
    match value.filter(|v| !v.is_empty()) {
        None => Ok(USB_PORT),
        Some(v) => v.to_str().and_then(|s| s.parse().ok()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{USB_PORT_VAR}={v:?} is not a port"),
            )
        }),
    }
}

pub trait Link: Send + Sync {
    fn send(&self, message: &Message) -> io::Result<()>;
    /// Sends [`Message::Audio`] with its samples read in place.
    fn send_audio(&self, header: &AudioHeader, samples: &[i16]) -> io::Result<()> {
        self.send(&Message::Audio(AudioPacket {
            header: *header,
            samples: samples.to_vec(),
        }))
    }
    /// Sends [`Message::VideoFrame`] with its access unit read in place.
    fn send_video(&self, header: &VideoHeader, data: &[u8]) -> io::Result<()> {
        self.send(&Message::VideoFrame(VideoFrame {
            frame_index: header.frame_index,
            display_time_ns: header.display_time_ns,
            keyframe: header.keyframe,
            views: header.views,
            data: data.to_vec(),
        }))
    }
    /// Blocks for the next message; an error means the headset is gone.
    fn recv(&self) -> io::Result<Message>;
    fn shutdown(&self);
}

pub trait Transport: Send {
    /// Blocks until a headset connects.
    fn accept(&mut self) -> io::Result<Box<dyn Link>>;
}

/// USB: the headset's `adb reverse` tunnel lands on this loopback listener.
pub struct UsbTransport {
    listener: TcpListener,
}

impl UsbTransport {
    pub fn bind() -> io::Result<Self> {
        Self::bind_port(usb_port()?)
    }

    pub fn bind_port(port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
        Ok(Self { listener })
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |a| a.port())
    }
}

impl Transport for UsbTransport {
    fn accept(&mut self) -> io::Result<Box<dyn Link>> {
        let (stream, _) = self.listener.accept()?;
        Ok(Box::new(StreamLink::new(stream)?))
    }
}

/// A lossless byte stream with length-prefixed messages. A video frame waits for every other
/// message already waiting to be written.
pub struct StreamLink {
    writer: Mutex<Outbox>,
    /// Senders of anything but video that are waiting for `writer`.
    urgent: AtomicUsize,
    urgent_done: Condvar,
    reader: Mutex<(TcpStream, StreamDecoder)>,
    control: TcpStream,
}

struct Outbox {
    stream: BufWriter<TcpStream>,
    scratch: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Urgent,
    Video,
}

impl StreamLink {
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        // A live peer is never silent this long (tracking or clock pongs flow constantly); a
        // dead one behind `adb reverse` may never close the socket, so silence ends the link.
        stream.set_read_timeout(Some(SILENCE_TIMEOUT))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(Self {
            writer: Mutex::new(Outbox {
                stream: BufWriter::with_capacity(1 << 20, stream.try_clone()?),
                scratch: Vec::new(),
            }),
            urgent: AtomicUsize::new(0),
            urgent_done: Condvar::new(),
            reader: Mutex::new((stream.try_clone()?, StreamDecoder::default())),
            control: stream,
        })
    }

    fn outbox(&self, lane: Lane) -> MutexGuard<'_, Outbox> {
        match lane {
            Lane::Urgent => {
                self.urgent.fetch_add(1, Ordering::SeqCst);
                let outbox = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
                self.urgent.fetch_sub(1, Ordering::SeqCst);
                outbox
            }
            Lane::Video => {
                let mut outbox = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
                while self.urgent.load(Ordering::SeqCst) > 0 {
                    outbox = self
                        .urgent_done
                        .wait(outbox)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                outbox
            }
        }
    }

    /// Writes what `encode` frames into the outbox's scratch buffer, as one message.
    fn write(&self, lane: Lane, encode: impl FnOnce(&mut Vec<u8>)) -> io::Result<()> {
        let mut outbox = self.outbox(lane);
        let Outbox { stream, scratch } = &mut *outbox;
        scratch.clear();
        encode(scratch);
        let sent = stream.write_all(scratch).and_then(|()| stream.flush());
        drop(outbox);
        if lane == Lane::Urgent {
            self.urgent_done.notify_all();
        }
        if sent.is_err() {
            // Part of a message may have gone out, so nothing after it would frame correctly.
            self.shutdown();
        }
        sent
    }
}

impl Link for StreamLink {
    fn send(&self, message: &Message) -> io::Result<()> {
        let lane = match message {
            Message::VideoFrame(_) => Lane::Video,
            _ => Lane::Urgent,
        };
        self.write(lane, |out| encode_framed(message, out))
    }

    fn send_audio(&self, header: &AudioHeader, samples: &[i16]) -> io::Result<()> {
        self.write(Lane::Urgent, |out| {
            encode_audio_framed(header, samples, out)
        })
    }

    fn send_video(&self, header: &VideoHeader, data: &[u8]) -> io::Result<()> {
        self.write(Lane::Video, |out| encode_video_framed(header, data, out))
    }

    fn recv(&self) -> io::Result<Message> {
        let mut guard = self
            .reader
            .lock()
            .map_err(|_| io::Error::other("link poisoned"))?;
        let (stream, decoder) = &mut *guard;
        let mut buf = [0u8; 16 << 10];
        loop {
            match decoder.next_message() {
                Ok(Some(message)) => return Ok(message),
                Ok(None) => {}
                // Only a bad length loses the framing; any other bad message was consumed.
                Err(e @ DecodeError::TooLarge(_)) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")));
                }
                Err(_) => continue,
            }
            let n = stream.read(&mut buf)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            decoder.push(&buf[..n]);
        }
    }

    fn shutdown(&self) {
        let _ = self.control.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn usb_link_carries_messages_both_ways() {
        let mut transport = UsbTransport::bind_port(0).unwrap();
        let port = transport.port();
        let headset = thread::spawn(move || {
            let link = StreamLink::new(TcpStream::connect(("127.0.0.1", port)).unwrap()).unwrap();
            link.send(&Message::ClockPing { mac_ns: 5 }).unwrap();
            link.recv().unwrap()
        });
        let link = transport.accept().unwrap();
        assert_eq!(link.recv().unwrap(), Message::ClockPing { mac_ns: 5 });
        link.send(&Message::Recenter).unwrap();
        assert_eq!(headset.join().unwrap(), Message::Recenter);
    }

    #[test]
    fn audio_waiting_for_the_link_goes_before_video_waiting_for_it() {
        use std::time::Duration;
        use tether_protocol::{AudioFormat, AudioHeader, ClientTime, VideoFrame};

        let transport = UsbTransport::bind_port(0).unwrap();
        let port = transport.port();
        let headset = thread::spawn(move || {
            let link = StreamLink::new(TcpStream::connect(("127.0.0.1", port)).unwrap()).unwrap();
            [link.recv().unwrap(), link.recv().unwrap()]
        });
        let link: Arc<StreamLink> = {
            let accepted = transport.listener.accept().unwrap().0;
            Arc::new(StreamLink::new(accepted).unwrap())
        };
        // A write in progress holds the link while a video frame and then a packet of sound
        // queue for it.
        let busy = link.writer.lock().unwrap();
        let video = {
            let link = link.clone();
            thread::spawn(move || {
                link.send(&Message::VideoFrame(VideoFrame {
                    frame_index: 1,
                    display_time_ns: 0,
                    keyframe: true,
                    views: Default::default(),
                    data: vec![0; 64 << 10],
                }))
            })
        };
        thread::sleep(Duration::from_millis(50));
        let header = AudioHeader {
            format: AudioFormat {
                stream: 1,
                sample_rate: 48_000,
                channels: 2,
            },
            sequence: 0,
            first_frame: 0,
            present_at: ClientTime(5),
        };
        let audio = {
            let link = link.clone();
            thread::spawn(move || link.send_audio(&header, &[1, 2, 3, 4]))
        };
        thread::sleep(Duration::from_millis(50));
        drop(busy);
        audio.join().unwrap().unwrap();
        video.join().unwrap().unwrap();
        let [first, second] = headset.join().unwrap();
        assert!(
            matches!(first, Message::Audio(ref a) if a.header == header && a.samples == [1, 2, 3, 4])
        );
        assert!(matches!(second, Message::VideoFrame(_)));
    }

    #[test]
    fn the_port_override_wins_only_when_it_parses() {
        assert_eq!(port_from(None).unwrap(), USB_PORT);
        assert_eq!(port_from(Some(OsStr::new(""))).unwrap(), USB_PORT);
        assert_eq!(port_from(Some(OsStr::new("51234"))).unwrap(), 51234);
        for bad in ["port", "-1", "65536", " 51234"] {
            let err = port_from(Some(OsStr::new(bad))).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }
}
