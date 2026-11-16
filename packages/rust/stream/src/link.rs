//! Links carry protocol messages to one headset. `Transport` yields a `Link` per connecting
//! headset; USB is implemented, Wi-Fi plugs in as another `Transport`.

use std::io::{self, BufWriter, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;

use tether_protocol::{Message, StreamDecoder, USB_PORT, encode_framed};

pub trait Link: Send + Sync {
    fn send(&self, message: &Message) -> io::Result<()>;
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
        Self::bind_port(USB_PORT)
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

/// A lossless byte stream with length-prefixed messages.
pub struct StreamLink {
    writer: Mutex<(BufWriter<TcpStream>, Vec<u8>)>,
    reader: Mutex<(TcpStream, StreamDecoder)>,
    control: TcpStream,
}

impl StreamLink {
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            writer: Mutex::new((
                BufWriter::with_capacity(1 << 20, stream.try_clone()?),
                Vec::new(),
            )),
            reader: Mutex::new((stream.try_clone()?, StreamDecoder::default())),
            control: stream,
        })
    }
}

impl Link for StreamLink {
    fn send(&self, message: &Message) -> io::Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("link poisoned"))?;
        let (writer, scratch) = &mut *guard;
        scratch.clear();
        encode_framed(message, scratch);
        writer.write_all(scratch)?;
        writer.flush()
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
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}"))),
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
}
