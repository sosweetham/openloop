use crate::message::*;

/// Upper bound on one framed message; anything larger is a corrupt stream.
const MAX_MESSAGE: usize = 64 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    UnknownKind(u8),
    TooLarge(usize),
    InvalidString,
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
}

/// Appends `message` as `[kind][payload]`, the unit a datagram link would send.
pub fn encode(message: &Message, out: &mut Vec<u8>) {
    let w = &mut Writer(out);
    match message {
        Message::ClientHello(h) => {
            w.u8(kind::CLIENT_HELLO);
            w.u16(h.version);
            let p = &h.profile;
            w.str(&p.name);
            w.u32(p.eye_width);
            w.u32(p.eye_height);
            w.u32(p.refresh_rates.len() as u32);
            p.refresh_rates.iter().for_each(|r| w.f32(*r));
            w.f32(p.refresh_rate);
            w.str(&p.interaction_profile);
            w.bool(p.supports_hevc);
        }
        Message::ServerHello(h) => {
            w.u8(kind::SERVER_HELLO);
            w.u16(h.version);
            w.u32(h.video_width);
            w.u32(h.video_height);
        }
        Message::Tracking(t) => {
            w.u8(kind::TRACKING);
            w.i64(t.sample_time_ns);
            w.pose(&t.head);
            w.velocity(&t.head_velocity);
            t.views.iter().for_each(|v| w.view(v));
            t.controllers.iter().for_each(|c| w.controller(c));
        }
        Message::ClockPing { mac_ns } => {
            w.u8(kind::CLOCK_PING);
            w.i64(*mac_ns);
        }
        Message::ClockPong { mac_ns, client_ns } => {
            w.u8(kind::CLOCK_PONG);
            w.i64(*mac_ns);
            w.i64(*client_ns);
        }
        Message::VideoFrame(f) => {
            w.u8(kind::VIDEO_FRAME);
            w.u64(f.frame_index);
            w.i64(f.display_time_ns);
            w.bool(f.keyframe);
            f.views.iter().for_each(|v| w.view(v));
            w.u32(f.data.len() as u32);
            w.0.extend_from_slice(&f.data);
        }
        Message::Recenter => w.u8(kind::RECENTER),
        Message::Haptics(h) => {
            w.u8(kind::HAPTICS);
            w.u8(h.hand);
            w.f32(h.amplitude);
            w.i64(h.duration_ns);
            w.f32(h.frequency);
        }
        Message::KeyframeRequest => w.u8(kind::KEYFRAME_REQUEST),
        Message::InputFocus { focused } => {
            w.u8(kind::INPUT_FOCUS);
            w.bool(*focused);
        }
    }
}

/// Appends `message` with a little-endian `u32` length prefix, for stream links.
pub fn encode_framed(message: &Message, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    encode(message, out);
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
    let r = &mut Reader(bytes);
    Ok(match r.u8()? {
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
        other => return Err(DecodeError::UnknownKind(other)),
    })
}

/// Reassembles length-prefixed messages from arbitrary stream reads.
#[derive(Default)]
pub struct StreamDecoder {
    buffer: Vec<u8>,
}

impl StreamDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete message, `Ok(None)` if more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<Message>, DecodeError> {
        let Some(header) = self.buffer.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*header) as usize;
        if len > MAX_MESSAGE {
            return Err(DecodeError::TooLarge(len));
        }
        if self.buffer.len() < 4 + len {
            return Ok(None);
        }
        let message = decode(&self.buffer[4..4 + len]);
        self.buffer.drain(..4 + len);
        message.map(Some)
    }
}

struct Writer<'a>(&'a mut Vec<u8>);

impl Writer<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }
    fn u16(&mut self, v: u16) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
                profile: DeviceProfile {
                    name: "Meta Quest 3".into(),
                    eye_width: 1680,
                    eye_height: 1760,
                    refresh_rates: vec![72.0, 90.0, 120.0],
                    refresh_rate: 90.0,
                    interaction_profile: "/interaction_profiles/oculus/touch_controller".into(),
                    supports_hevc: true,
                },
            }),
            Message::ServerHello(ServerHello {
                version: 1,
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
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for message in samples() {
            let mut bytes = Vec::new();
            encode(&message, &mut bytes);
            assert_eq!(decode(&bytes), Ok(message));
        }
    }

    #[test]
    fn stream_decoder_reassembles_byte_by_byte() {
        let messages = samples();
        let mut stream = Vec::new();
        messages.iter().for_each(|m| encode_framed(m, &mut stream));

        let mut decoder = StreamDecoder::default();
        let mut out = Vec::new();
        for byte in stream {
            decoder.push(&[byte]);
            while let Some(m) = decoder.next_message().unwrap() {
                out.push(m);
            }
        }
        assert_eq!(out, messages);
    }

    #[test]
    fn rejects_truncated_and_unknown() {
        assert_eq!(decode(&[kind::TRACKING, 1, 2]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[200]), Err(DecodeError::UnknownKind(200)));
    }
}
