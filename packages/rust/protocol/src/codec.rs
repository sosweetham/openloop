use crate::message::*;

/// Upper bound on one framed message; anything larger is a corrupt stream.
const MAX_MESSAGE: usize = 64 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    /// A length prefix no real message has: the stream cannot be resynchronised.
    TooLarge(usize),
    InvalidString,
    /// Fields outside what the protocol allows, such as audio with no channels.
    Malformed,
}

mod kind {
    pub const CLIENT_HELLO: u8 = 1;
    pub const SERVER_HELLO: u8 = 2;
    pub const TRACKING: u8 = 3;
    pub const CLOCK_PING: u8 = 4;
    pub const CLOCK_PONG: u8 = 5;
    pub const VIDEO_FRAME: u8 = 6;
    pub const RECENTER: u8 = 7;
    pub const HAPTICS: u8 = 8;
    pub const KEYFRAME_REQUEST: u8 = 9;
    pub const INPUT_FOCUS: u8 = 10;
    pub const STREAM_ENDED: u8 = 11;
    pub const AUDIO_START: u8 = 12;
    pub const AUDIO: u8 = 13;
    pub const AUDIO_STOP: u8 = 14;
}

/// Appends `message` as `[kind][payload]`, the unit a datagram link would send.
pub fn encode(message: &Message, out: &mut Vec<u8>) {
    Writer(out).message(message);
}

/// Appends `message` with a little-endian `u32` length prefix, for stream links.
pub fn encode_framed(message: &Message, out: &mut Vec<u8>) {
    framed(out, |w| w.message(message));
}

/// [`encode_framed`] for an audio packet whose samples stay in the sender's own buffer.
pub fn encode_audio_framed(header: &AudioHeader, samples: &[i16], out: &mut Vec<u8>) {
    framed(out, |w| w.audio(header, samples));
}

/// [`encode_framed`] for a video frame whose access unit stays in the sender's own buffer.
pub fn encode_video_framed(header: &VideoHeader, data: &[u8], out: &mut Vec<u8>) {
    framed(out, |w| w.video(header, data));
}

fn framed(out: &mut Vec<u8>, body: impl FnOnce(&mut Writer<'_>)) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    body(&mut Writer(out));
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Decodes one `[kind][payload]` unit. `Ok(None)` is a kind this build does not know, which the
/// receiver skips.
pub fn decode(bytes: &[u8]) -> Result<Option<Message>, DecodeError> {
    let r = &mut Reader(bytes);
    Ok(Some(match r.u8()? {
        kind::CLIENT_HELLO => {
            let version = r.u16()?;
            let name = r.str()?;
            let eye_width = r.u32()?;
            let eye_height = r.u32()?;
            let rates = (0..r.u32()?).map(|_| r.f32()).collect::<Result<_, _>>()?;
            Message::ClientHello(ClientHello {
                version,
                profile: DeviceProfile {
                    name,
                    eye_width,
                    eye_height,
                    refresh_rates: rates,
                    refresh_rate: r.f32()?,
                    interaction_profile: r.str()?,
                    supports_hevc: r.bool()?,
                    audio_sample_rate: r.appended(Reader::u32)?,
                    audio_channels: r.appended(Reader::u8)?,
                },
            })
        }
        kind::SERVER_HELLO => Message::ServerHello(ServerHello {
            version: r.u16()?,
            video_width: r.u32()?,
            video_height: r.u32()?,
        }),
        kind::TRACKING => Message::Tracking(Box::new(Tracking {
            sample_time_ns: r.i64()?,
            head: r.pose()?,
            head_velocity: r.velocity()?,
            views: [r.view()?, r.view()?],
            controllers: [r.controller()?, r.controller()?],
        })),
        kind::CLOCK_PING => Message::ClockPing { mac_ns: r.i64()? },
        kind::CLOCK_PONG => Message::ClockPong {
            mac_ns: r.i64()?,
            client_ns: r.i64()?,
        },
        kind::VIDEO_FRAME => {
            let frame_index = r.u64()?;
            let display_time_ns = r.i64()?;
            let keyframe = r.bool()?;
            let views = [r.view()?, r.view()?];
            let len = r.u32()? as usize;
            Message::VideoFrame(VideoFrame {
                frame_index,
                display_time_ns,
                keyframe,
                views,
                data: r.take(len)?.to_vec(),
            })
        }
        kind::RECENTER => Message::Recenter,
        kind::HAPTICS => Message::Haptics(Haptics {
            hand: r.u8()?,
            amplitude: r.f32()?,
            duration_ns: r.i64()?,
            frequency: r.f32()?,
        }),
        kind::KEYFRAME_REQUEST => Message::KeyframeRequest,
        kind::INPUT_FOCUS => Message::InputFocus { focused: r.bool()? },
        kind::STREAM_ENDED => Message::StreamEnded,
        kind::AUDIO_START => Message::AudioStart(r.audio_format()?),
        kind::AUDIO => Message::Audio(r.audio()?.to_packet()),
        kind::AUDIO_STOP => Message::AudioStop { stream: r.u16()? },
        _ => return Ok(None),
    }))
}

/// An [`AudioPacket`] read in place from the bytes it arrived in, so a receiver can copy its
/// samples into storage of its own without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioPacketRef<'a> {
    pub header: AudioHeader,
    /// Little-endian `i16`s, whole frames.
    pcm: &'a [u8],
}

impl<'a> AudioPacketRef<'a> {
    /// The audio packet in `unit`, a `[kind][payload]` as [`StreamDecoder::next_unit`] yields
    /// it or a datagram carries it; `Ok(None)` for any other kind.
    pub fn decode(unit: &'a [u8]) -> Result<Option<Self>, DecodeError> {
        let r = &mut Reader(unit);
        if r.u8()? != kind::AUDIO {
            return Ok(None);
        }
        r.audio().map(Some)
    }

    pub fn frames(&self) -> usize {
        self.pcm.len() / 2 / usize::from(self.header.format.channels)
    }

    /// Interleaved, `header.format.channels` per frame.
    pub fn samples(&self) -> impl ExactSizeIterator<Item = i16> + use<'a> {
        let pcm: &'a [u8] = self.pcm;
        pcm.chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
    }

    pub fn to_packet(&self) -> AudioPacket {
        AudioPacket {
            header: self.header,
            samples: self.samples().collect(),
        }
    }
}

/// Reassembles length-prefixed units from arbitrary stream reads. Its buffer grows to fit the
/// largest message and is then reused, so a steady stream allocates nothing here.
#[derive(Default)]
pub struct StreamDecoder {
    buffer: Vec<u8>,
    /// Where the first unread unit starts in `buffer`.
    start: usize,
    skipped: u64,
}

impl StreamDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        // Consumed units are dropped here, once per read, rather than once per unit.
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete unit, still encoded as `[kind][payload]`; `Ok(None)` if more bytes are
    /// needed. A receiver that must not allocate reads audio out of it with
    /// [`AudioPacketRef::decode`] and everything else with [`decode`]. The only error is
    /// [`DecodeError::TooLarge`], after which the stream cannot be resynchronised.
    pub fn next_unit(&mut self) -> Result<Option<&[u8]>, DecodeError> {
        let unread = &self.buffer[self.start..];
        let Some(prefix) = unread.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*prefix) as usize;
        if len > MAX_MESSAGE {
            return Err(DecodeError::TooLarge(len));
        }
        if unread.len() < 4 + len {
            return Ok(None);
        }
        let unit = self.start + 4..self.start + 4 + len;
        self.start = unit.end;
        Ok(Some(&self.buffer[unit]))
    }

    /// The next complete message, `Ok(None)` if more bytes are needed. Messages of unknown kinds
    /// are skipped. After any error but [`DecodeError::TooLarge`] the bad message is consumed,
    /// so decoding can go on with the next one.
    pub fn next_message(&mut self) -> Result<Option<Message>, DecodeError> {
        while let Some(unit) = self.next_unit()? {
            match decode(unit)? {
                Some(message) => return Ok(Some(message)),
                None => self.skipped += 1,
            }
        }
        Ok(None)
    }

    /// Messages [`StreamDecoder::next_message`] skipped because this build does not know their
    /// kind.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
}

struct Writer<'a>(&'a mut Vec<u8>);

impl Writer<'_> {
    fn message(&mut self, message: &Message) {
        match message {
            Message::ClientHello(h) => {
                self.u8(kind::CLIENT_HELLO);
                self.u16(h.version);
                let p = &h.profile;
                self.str(&p.name);
                self.u32(p.eye_width);
                self.u32(p.eye_height);
                self.u32(p.refresh_rates.len() as u32);
                p.refresh_rates.iter().for_each(|r| self.f32(*r));
                self.f32(p.refresh_rate);
                self.str(&p.interaction_profile);
                self.bool(p.supports_hevc);
                self.u32(p.audio_sample_rate);
                self.u8(p.audio_channels);
            }
            Message::ServerHello(h) => {
                self.u8(kind::SERVER_HELLO);
                self.u16(h.version);
                self.u32(h.video_width);
                self.u32(h.video_height);
            }
            Message::Tracking(t) => {
                self.u8(kind::TRACKING);
                self.i64(t.sample_time_ns);
                self.pose(&t.head);
                self.velocity(&t.head_velocity);
                t.views.iter().for_each(|v| self.view(v));
                t.controllers.iter().for_each(|c| self.controller(c));
            }
            Message::ClockPing { mac_ns } => {
                self.u8(kind::CLOCK_PING);
                self.i64(*mac_ns);
            }
            Message::ClockPong { mac_ns, client_ns } => {
                self.u8(kind::CLOCK_PONG);
                self.i64(*mac_ns);
                self.i64(*client_ns);
            }
            Message::VideoFrame(f) => self.video(
                &VideoHeader {
                    frame_index: f.frame_index,
                    display_time_ns: f.display_time_ns,
                    keyframe: f.keyframe,
                    views: f.views,
                },
                &f.data,
            ),
            Message::Recenter => self.u8(kind::RECENTER),
            Message::Haptics(h) => {
                self.u8(kind::HAPTICS);
                self.u8(h.hand);
                self.f32(h.amplitude);
                self.i64(h.duration_ns);
                self.f32(h.frequency);
            }
            Message::KeyframeRequest => self.u8(kind::KEYFRAME_REQUEST),
            Message::InputFocus { focused } => {
                self.u8(kind::INPUT_FOCUS);
                self.bool(*focused);
            }
            Message::StreamEnded => self.u8(kind::STREAM_ENDED),
            Message::AudioStart(format) => {
                self.u8(kind::AUDIO_START);
                self.audio_format(format);
            }
            Message::Audio(a) => self.audio(&a.header, &a.samples),
            Message::AudioStop { stream } => {
                self.u8(kind::AUDIO_STOP);
                self.u16(*stream);
            }
        }
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, v: &str) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v.as_bytes());
    }
    fn pose(&mut self, p: &Pose) {
        p.position
            .iter()
            .chain(&p.orientation)
            .for_each(|v| self.f32(*v));
    }
    fn velocity(&mut self, v: &Velocity) {
        v.linear.iter().chain(&v.angular).for_each(|x| self.f32(*x));
    }
    fn view(&mut self, v: &View) {
        self.pose(&v.pose);
        [v.fov.left, v.fov.right, v.fov.up, v.fov.down]
            .iter()
            .for_each(|x| self.f32(*x));
    }
    fn controller(&mut self, c: &ControllerState) {
        self.bool(c.active);
        self.pose(&c.grip);
        self.pose(&c.aim);
        self.velocity(&c.grip_velocity);
        self.u32(c.buttons);
        self.u32(c.touches);
        self.f32(c.trigger);
        self.f32(c.squeeze);
        self.f32(c.thumbstick[0]);
        self.f32(c.thumbstick[1]);
    }
    fn audio_format(&mut self, f: &AudioFormat) {
        self.u16(f.stream);
        self.u32(f.sample_rate);
        self.u8(f.channels);
    }
    fn audio(&mut self, header: &AudioHeader, samples: &[i16]) {
        self.u8(kind::AUDIO);
        self.audio_format(&header.format);
        self.u32(header.sequence);
        self.u64(header.first_frame);
        self.i64(header.present_at.0);
        self.u32(samples.len() as u32);
        self.0.reserve(samples.len() * 2);
        samples.iter().for_each(|s| self.i16(*s));
    }

    fn video(&mut self, header: &VideoHeader, data: &[u8]) {
        self.u8(kind::VIDEO_FRAME);
        self.u64(header.frame_index);
        self.i64(header.display_time_ns);
        self.bool(header.keyframe);
        header.views.iter().for_each(|v| self.view(v));
        self.u32(data.len() as u32);
        self.0.extend_from_slice(data);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    /// A field appended to its message after the message first shipped: a sender that predates
    /// it ends the payload before it.
    fn appended<T: Default>(
        &mut self,
        read: fn(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<T, DecodeError> {
        if self.0.is_empty() {
            Ok(T::default())
        } else {
            read(self)
        }
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("take returned N bytes"))
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }
    fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    fn f32(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_le_bytes(self.array()?))
    }
    fn str(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| DecodeError::InvalidString)
    }
    fn f32s<const N: usize>(&mut self) -> Result<[f32; N], DecodeError> {
        let mut out = [0.0; N];
        for v in &mut out {
            *v = self.f32()?;
        }
        Ok(out)
    }
    fn pose(&mut self) -> Result<Pose, DecodeError> {
        Ok(Pose {
            position: self.f32s()?,
            orientation: self.f32s()?,
        })
    }
    fn velocity(&mut self) -> Result<Velocity, DecodeError> {
        Ok(Velocity {
            linear: self.f32s()?,
            angular: self.f32s()?,
        })
    }
    fn view(&mut self) -> Result<View, DecodeError> {
        let pose = self.pose()?;
        let [left, right, up, down] = self.f32s()?;
        Ok(View {
            pose,
            fov: Fov {
                left,
                right,
                up,
                down,
            },
        })
    }
    fn controller(&mut self) -> Result<ControllerState, DecodeError> {
        Ok(ControllerState {
            active: self.bool()?,
            grip: self.pose()?,
            aim: self.pose()?,
            grip_velocity: self.velocity()?,
            buttons: self.u32()?,
            touches: self.u32()?,
            trigger: self.f32()?,
            squeeze: self.f32()?,
            thumbstick: self.f32s()?,
        })
    }
    fn audio_format(&mut self) -> Result<AudioFormat, DecodeError> {
        Ok(AudioFormat {
            stream: self.u16()?,
            sample_rate: self.u32()?,
            channels: self.u8()?,
        })
    }
    /// The payload of an audio packet, after its kind.
    fn audio(&mut self) -> Result<AudioPacketRef<'a>, DecodeError> {
        let header = AudioHeader {
            format: self.audio_format()?,
            sequence: self.u32()?,
            first_frame: self.u64()?,
            present_at: ClientTime(self.i64()?),
        };
        let count = self.u32()? as usize;
        let channels = usize::from(header.format.channels);
        if !(1..=usize::from(MAX_AUDIO_CHANNELS)).contains(&channels)
            || !count.is_multiple_of(channels)
            || count / channels > MAX_AUDIO_FRAMES
        {
            return Err(DecodeError::Malformed);
        }
        Ok(AudioPacketRef {
            header,
            pcm: self.take(count * 2)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quest_3() -> DeviceProfile {
        DeviceProfile {
            name: "Meta Quest 3".into(),
            eye_width: 1680,
            eye_height: 1760,
            refresh_rates: vec![72.0, 90.0, 120.0],
            refresh_rate: 90.0,
            interaction_profile: "/interaction_profiles/oculus/touch_controller".into(),
            supports_hevc: true,
            audio_sample_rate: 48_000,
            audio_channels: 2,
        }
    }

    const FORMAT: AudioFormat = AudioFormat {
        stream: 3,
        sample_rate: 48_000,
        channels: 2,
    };

    fn audio_packet() -> AudioPacket {
        AudioPacket {
            header: AudioHeader {
                format: FORMAT,
                sequence: 41,
                first_frame: 9_876_543_210,
                present_at: ClientTime(-123_456_789),
            },
            samples: (0..480).map(|i| (i * 131 - 30_000) as i16).collect(),
        }
    }

    /// Every kind of message, in kind order.
    fn samples() -> Vec<Message> {
        let view = View {
            pose: Pose {
                position: [0.03, 1.6, -0.1],
                orientation: [0.0, 0.1, 0.0, 0.995],
            },
            fov: Fov {
                left: -0.9,
                right: 0.8,
                up: 0.85,
                down: -0.95,
            },
        };
        let controller = ControllerState {
            active: true,
            grip: Pose {
                position: [0.2, 1.1, -0.3],
                orientation: [0.1, 0.2, 0.3, 0.9],
            },
            buttons: button::PRIMARY | button::TRIGGER,
            trigger: 0.75,
            thumbstick: [-0.5, 0.25],
            ..Default::default()
        };
        vec![
            Message::ClientHello(ClientHello {
                version: crate::PROTOCOL_VERSION,
                profile: quest_3(),
            }),
            Message::ServerHello(ServerHello {
                version: crate::PROTOCOL_VERSION,
                video_width: 3360,
                video_height: 1760,
            }),
            Message::Tracking(Box::new(Tracking {
                sample_time_ns: 123_456_789,
                head: view.pose,
                views: [view, view],
                controllers: [controller, ControllerState::default()],
                ..Default::default()
            })),
            Message::ClockPing { mac_ns: -5 },
            Message::ClockPong {
                mac_ns: 7,
                client_ns: 9,
            },
            Message::VideoFrame(VideoFrame {
                frame_index: 42,
                display_time_ns: 1_000,
                keyframe: true,
                views: [view, view],
                data: vec![0, 0, 0, 1, 0x40, 0x01],
            }),
            Message::Recenter,
            Message::Haptics(Haptics {
                hand: 1,
                amplitude: 0.5,
                duration_ns: 20_000_000,
                frequency: 0.0,
            }),
            Message::KeyframeRequest,
            Message::InputFocus { focused: false },
            Message::StreamEnded,
            Message::AudioStart(FORMAT),
            Message::Audio(audio_packet()),
            Message::AudioStop { stream: 3 },
        ]
    }

    fn framed(messages: &[Message]) -> Vec<u8> {
        let mut stream = Vec::new();
        messages.iter().for_each(|m| encode_framed(m, &mut stream));
        stream
    }

    /// A frame of a kind no build knows, as a newer peer would send it.
    fn unknown(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
        frame.push(kind);
        frame.extend_from_slice(payload);
        frame
    }

    fn drain(decoder: &mut StreamDecoder) -> Vec<Message> {
        let mut out = Vec::new();
        while let Some(m) = decoder.next_message().unwrap() {
            out.push(m);
        }
        out
    }

    fn encoded(message: &Message) -> Vec<u8> {
        let mut bytes = Vec::new();
        encode(message, &mut bytes);
        bytes
    }

    #[test]
    fn every_message_round_trips() {
        for message in samples() {
            assert_eq!(decode(&encoded(&message)), Ok(Some(message)));
        }
    }

    /// Kinds are the wire contract with clients already installed on headsets: never renumber.
    #[test]
    fn every_kind_keeps_its_number() {
        let kinds: Vec<u8> = samples().iter().map(|m| encoded(m)[0]).collect();
        assert_eq!(kinds, (1..=14).collect::<Vec<u8>>());
    }

    #[test]
    fn the_new_messages_keep_their_wire_layout() {
        let audio = Message::Audio(AudioPacket {
            header: AudioHeader {
                format: FORMAT,
                sequence: 41,
                first_frame: 0x0102_0304_0506_0708,
                present_at: ClientTime(-2),
            },
            samples: vec![1, -1],
        });
        #[rustfmt::skip]
        let expected: [&[u8]; 4] = [
            &[11],
            &[12, 3, 0, 0x80, 0xbb, 0, 0, 2],
            &[
                13, 3, 0, 0x80, 0xbb, 0, 0, 2,
                41, 0, 0, 0,
                8, 7, 6, 5, 4, 3, 2, 1,
                0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                2, 0, 0, 0,
                1, 0, 0xff, 0xff,
            ],
            &[14, 3, 0],
        ];
        let messages = [
            Message::StreamEnded,
            Message::AudioStart(FORMAT),
            audio,
            Message::AudioStop { stream: 3 },
        ];
        for (message, bytes) in messages.iter().zip(expected) {
            assert_eq!(encoded(message), bytes, "{message:?}");
        }
        let hello = encoded(&samples()[0]);
        assert_eq!(
            hello[hello.len() - 5..],
            [0x80, 0xbb, 0, 0, 2],
            "rate and channels last"
        );
    }

    #[test]
    fn stream_decoder_reassembles_byte_by_byte() {
        let messages = samples();
        let mut decoder = StreamDecoder::default();
        let mut out = Vec::new();
        for byte in framed(&messages) {
            decoder.push(&[byte]);
            out.extend(drain(&mut decoder));
        }
        assert_eq!(out, messages);
    }

    #[test]
    fn unknown_kinds_are_skipped_and_the_stream_stays_in_sync() {
        let known = samples();
        let mut stream = Vec::new();
        for (i, message) in known.iter().enumerate() {
            // Payloads that look like length prefixes and kinds, to catch a decoder that reads
            // into a skipped frame.
            stream.extend(unknown(200 + i as u8, &[4, 0, 0, 0, 7, 0xff][..i % 7]));
            encode_framed(message, &mut stream);
        }
        stream.extend(unknown(255, &vec![0xab; 70_000]));
        stream.extend(unknown(0, &[]));

        let mut whole = StreamDecoder::default();
        whole.push(&stream);
        assert_eq!(drain(&mut whole), known);
        assert_eq!(whole.skipped(), known.len() as u64 + 2);

        for chunk in [1, 3, 4096] {
            let mut pieces = StreamDecoder::default();
            let mut out = Vec::new();
            for piece in stream.chunks(chunk) {
                pieces.push(piece);
                out.extend(drain(&mut pieces));
            }
            assert_eq!(out, known, "pushed {chunk} bytes at a time");
            assert_eq!(pieces.skipped(), known.len() as u64 + 2);
        }
    }

    #[test]
    fn a_datagram_of_an_unknown_kind_decodes_to_nothing() {
        assert_eq!(decode(&[200, 1, 2, 3]), Ok(None));
        assert_eq!(decode(&[0]), Ok(None));
    }

    #[test]
    fn bytes_after_the_known_fields_are_ignored() {
        for message in samples() {
            let mut bytes = encoded(&message);
            bytes.extend_from_slice(&[0xab; 9]);
            assert_eq!(decode(&bytes), Ok(Some(message)));
        }
    }

    #[test]
    fn a_hello_from_before_the_audio_fields_still_shows_its_version() {
        let mut bytes = encoded(&Message::ClientHello(ClientHello {
            version: 1,
            profile: quest_3(),
        }));
        bytes.truncate(bytes.len() - 5);
        let Ok(Some(Message::ClientHello(hello))) = decode(&bytes) else {
            panic!("an older hello must decode");
        };
        assert_eq!(hello.version, 1);
        assert_eq!(hello.profile.name, "Meta Quest 3");
        assert_eq!(
            (
                hello.profile.audio_sample_rate,
                hello.profile.audio_channels
            ),
            (0, 0),
            "no audio output"
        );
    }

    #[test]
    fn audio_reads_in_place_as_it_decodes() {
        let packet = audio_packet();
        let bytes = encoded(&Message::Audio(packet.clone()));
        let view = AudioPacketRef::decode(&bytes).unwrap().unwrap();
        assert_eq!(view.header, packet.header);
        assert_eq!(view.frames(), 240);
        assert_eq!(view.samples().len(), 480);
        assert!(view.samples().eq(packet.samples.iter().copied()));
        assert_eq!(view.to_packet(), packet);
        for other in samples().iter().filter(|m| !matches!(m, Message::Audio(_))) {
            assert_eq!(
                AudioPacketRef::decode(&encoded(other)),
                Ok(None),
                "{other:?}"
            );
        }
    }

    #[test]
    fn audio_framed_from_a_borrowed_buffer_is_the_message() {
        let packet = audio_packet();
        let mut borrowed = Vec::new();
        encode_audio_framed(&packet.header, &packet.samples, &mut borrowed);
        assert_eq!(borrowed, framed(&[Message::Audio(packet)]));
    }

    #[test]
    fn video_framed_from_a_borrowed_buffer_is_the_message() {
        let header = VideoHeader {
            frame_index: 7,
            display_time_ns: -3,
            keyframe: true,
            views: [View::default(); 2],
        };
        let data = [0, 0, 0, 1, 0x40, 0x01, 9];
        let mut borrowed = Vec::new();
        encode_video_framed(&header, &data, &mut borrowed);
        let frame = VideoFrame {
            frame_index: header.frame_index,
            display_time_ns: header.display_time_ns,
            keyframe: header.keyframe,
            views: header.views,
            data: data.to_vec(),
        };
        assert_eq!(borrowed, framed(&[Message::VideoFrame(frame)]));
    }

    #[test]
    fn units_come_out_whole_and_undecoded() {
        let mut stream = framed(&[Message::Recenter]);
        stream.extend(unknown(250, &[1, 2]));
        stream.extend(framed(&[Message::Audio(audio_packet())]));
        let mut decoder = StreamDecoder::default();
        decoder.push(&stream[..stream.len() - 1]);
        assert_eq!(decoder.next_unit(), Ok(Some(&[kind::RECENTER][..])));
        assert_eq!(decoder.next_unit(), Ok(Some(&[250, 1, 2][..])));
        assert_eq!(decoder.next_unit(), Ok(None), "the audio is one byte short");
        decoder.push(&stream[stream.len() - 1..]);
        let unit = decoder.next_unit().unwrap().unwrap();
        let view = AudioPacketRef::decode(unit).unwrap().unwrap();
        assert_eq!(view.header, audio_packet().header);
        assert_eq!(decoder.next_unit(), Ok(None));
        assert_eq!(decoder.skipped(), 0, "only next_message skips");
    }

    #[test]
    fn audio_outside_the_limits_is_malformed_and_skipped() {
        let packet = |channels: u8, count: usize| {
            Message::Audio(AudioPacket {
                header: AudioHeader {
                    format: AudioFormat { channels, ..FORMAT },
                    ..AudioHeader::default()
                },
                samples: vec![1; count],
            })
        };
        let largest = 2 * MAX_AUDIO_FRAMES;
        let bad = [
            packet(0, 0),
            packet(2, 3),
            packet(MAX_AUDIO_CHANNELS + 1, 3 * 10),
            packet(2, largest + 2),
        ];
        for message in &bad {
            let bytes = encoded(message);
            assert_eq!(decode(&bytes), Err(DecodeError::Malformed), "{message:?}");
            assert_eq!(AudioPacketRef::decode(&bytes), Err(DecodeError::Malformed));
        }
        let mut stream = framed(&bad);
        stream.extend(framed(&[packet(2, largest), Message::StreamEnded]));

        let mut decoder = StreamDecoder::default();
        decoder.push(&stream);
        for _ in &bad {
            assert_eq!(decoder.next_message(), Err(DecodeError::Malformed));
        }
        assert!(matches!(
            decoder.next_message(),
            Ok(Some(Message::Audio(p))) if p.frames() == MAX_AUDIO_FRAMES
        ));
        assert_eq!(decoder.next_message(), Ok(Some(Message::StreamEnded)));
        assert_eq!(decoder.next_message(), Ok(None));
    }

    #[test]
    fn audio_frames_count_every_channel_once() {
        assert_eq!(audio_packet().frames(), 240);
    }

    #[test]
    fn rejects_truncated_messages_and_impossible_lengths() {
        assert_eq!(decode(&[kind::TRACKING, 1, 2]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[]), Err(DecodeError::Truncated));
        let audio = encoded(&Message::Audio(audio_packet()));
        assert_eq!(
            AudioPacketRef::decode(&audio[..audio.len() - 1]),
            Err(DecodeError::Truncated)
        );

        let mut decoder = StreamDecoder::default();
        decoder.push(&((MAX_MESSAGE + 1) as u32).to_le_bytes());
        assert_eq!(
            decoder.next_message(),
            Err(DecodeError::TooLarge(MAX_MESSAGE + 1))
        );
        assert_eq!(
            decoder.next_unit(),
            Err(DecodeError::TooLarge(MAX_MESSAGE + 1))
        );
    }
}
