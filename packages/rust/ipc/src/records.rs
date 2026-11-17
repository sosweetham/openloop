//! What the session region carries (docs/ARCHITECTURE.md § Runtime ↔ tetherd). Every time is
//! `XrTime` nanoseconds on the Mac's clock unless its type says otherwise. Each side reads what
//! the other wrote as untrusted bytes, so the accessors here validate instead of assuming.

use crate::pod::pod;

pod! {
    /// Metres, and a unit quaternion `[x, y, z, w]`.
    pub struct Pose {
        pub position: [f32; 3],
        pub orientation: [f32; 4],
    }
}

impl Pose {
    pub const IDENTITY: Pose = Pose {
        position: [0.0; 3],
        orientation: [0.0, 0.0, 0.0, 1.0],
    };
}

pod! {
    /// Half-angles in radians, OpenXR's convention (left and down negative).
    pub struct Fov {
        pub left: f32,
        pub right: f32,
        pub up: f32,
        pub down: f32,
    }
}

pod! {
    pub struct Velocity {
        pub linear: [f32; 3],
        pub angular: [f32; 3],
    }
}

pod! {
    /// One eye relative to the head.
    pub struct View {
        pub pose: Pose,
        pub fov: Fov,
    }
}

pod! {
    pub struct Controller {
        pub grip: Pose,
        pub aim: Pose,
        pub grip_velocity: Velocity,
        pub trigger: f32,
        pub squeeze: f32,
        pub thumbstick: [f32; 2],
        /// `tether_protocol::button` bits.
        pub buttons: u32,
        pub touches: u32,
        /// Nonzero while the headset tracks this controller.
        pub active: u32,
        pub reserved: u32,
    }
}

pod! {
    /// A nanosecond on the headset's clock, which only `tetherd`'s clock mapping relates to
    /// `XrTime`.
    pub struct HeadsetTime {
        pub ns: i64,
    }
}

pod! {
    /// The headset's profile; `tetherd` publishes a new one whenever the headset (re)connects.
    pub struct HeadsetInfo {
        /// Recommended per-eye render size.
        pub eye_width: u32,
        pub eye_height: u32,
        pub refresh_rate: f32,
        pub refresh_rate_count: u32,
        pub refresh_rates: [f32; 8],
        /// 0: the headset plays no audio.
        pub audio_sample_rate: u32,
        pub audio_channels: u32,
        pub flags: u32,
        pub reserved: u32,
        /// UTF-8, NUL-terminated.
        pub name: [u8; 64],
        /// UTF-8, NUL-terminated.
        pub interaction_profile: [u8; 96],
    }
}

impl HeadsetInfo {
    pub const CONNECTED: u32 = 1 << 0;
    pub const HEVC: u32 = 1 << 1;

    pub fn connected(&self) -> bool {
        self.flags & Self::CONNECTED != 0
    }

    pub fn name(&self) -> Option<&str> {
        text(&self.name)
    }

    pub fn interaction_profile(&self) -> Option<&str> {
        text(&self.interaction_profile)
    }

    /// Truncates at a character boundary to fit.
    pub fn set_name(&mut self, name: &str) {
        set_text(&mut self.name, name);
    }

    pub fn set_interaction_profile(&mut self, profile: &str) {
        set_text(&mut self.interaction_profile, profile);
    }

    /// The advertised rates the peer wrote, bounded by the array.
    pub fn refresh_rates(&self) -> &[f32] {
        &self.refresh_rates[..(self.refresh_rate_count as usize).min(self.refresh_rates.len())]
    }
}

pod! {
    /// The latest tracking, already on the Mac's clock: `tetherd` maps the headset's sample
    /// time before publishing.
    pub struct TrackingSample {
        pub sample_mac_ns: i64,
        pub received_mac_ns: i64,
        /// In the headset's tracking space (STAGE, floor at y = 0).
        pub head: Pose,
        pub head_velocity: Velocity,
        pub views: [View; 2],
        pub controllers: [Controller; 2],
        pub flags: u32,
    }
}

impl TrackingSample {
    /// Tracking is arriving on the current link; otherwise the last pose is held.
    pub const LIVE: u32 = 1 << 0;

    pub fn live(&self) -> bool {
        self.flags & Self::LIVE != 0
    }
}

pod! {
    /// `headset = mac + offset_ns`, from the fastest recent ping round trip.
    pub struct ClockMapping {
        pub offset_ns: i64,
        /// The round trip the offset came from; the offset is good to half of it.
        pub round_trip_ns: i64,
        pub measured_mac_ns: i64,
        pub flags: u64,
    }
}

impl ClockMapping {
    pub const SYNCED: u64 = 1 << 0;

    pub fn synced(&self) -> bool {
        self.flags & Self::SYNCED != 0
    }

    pub fn to_headset(&self, mac_ns: i64) -> HeadsetTime {
        HeadsetTime {
            ns: mac_ns.wrapping_add(self.offset_ns),
        }
    }

    pub fn to_mac(&self, time: HeadsetTime) -> i64 {
        time.ns.wrapping_sub(self.offset_ns)
    }
}

pod! {
    /// `tetherd`'s schedule for the app's next frame (R2 S4).
    pub struct FrameTiming {
        /// The frame this schedule is for; `xrWaitFrame` hands out each one once.
        pub frame_index: u64,
        /// When `xrWaitFrame` returns.
        pub wake_mac_ns: i64,
        pub display_mac_ns: i64,
        pub period_ns: i64,
        /// Render, compose, encode, transmit and decode, as measured.
        pub latency_ns: i64,
        pub flags: u64,
    }
}

impl FrameTiming {
    pub const SHOULD_RENDER: u64 = 1 << 0;

    pub fn should_render(&self) -> bool {
        self.flags & Self::SHOULD_RENDER != 0
    }
}

pod! {
    /// Levels behind the edges in the event rings: a reader that missed an event (its ring was
    /// full) still finds the state here. Counters only grow.
    pub struct SessionState {
        pub flags: u64,
        pub recenters: u64,
        pub last_recenter_mac_ns: i64,
        pub keyframe_requests: u64,
        /// Bumps with every new [`HeadsetInfo`].
        pub headset_generation: u64,
    }
}

impl SessionState {
    pub const LINK_UP: u64 = 1 << 0;
    /// The app has input focus; Tether's menu takes it while open.
    pub const FOCUSED: u64 = 1 << 1;
    /// `tetherd` is going away; the runtime detaches instead of waiting for it.
    pub const STOPPING: u64 = 1 << 2;

    pub fn link_up(&self) -> bool {
        self.flags & Self::LINK_UP != 0
    }

    pub fn focused(&self) -> bool {
        self.flags & Self::FOCUSED != 0
    }

    pub fn stopping(&self) -> bool {
        self.flags & Self::STOPPING != 0
    }
}

pod! {
    /// The header of one encoded video access unit, app → `tetherd` (R2 S1 and S2).
    pub struct AccessUnit {
        pub frame_index: u64,
        /// `tetherd` maps it to the headset's clock when it sends the frame.
        pub display_mac_ns: i64,
        pub encoded_mac_ns: i64,
        /// The views the frame was rendered for, in the headset's tracking space.
        pub views: [View; 2],
        /// Both eyes side by side.
        pub width: u32,
        pub height: u32,
        pub flags: u32,
        /// Set by the ring on commit.
        pub len: u32,
    }
}

impl AccessUnit {
    pub const KEYFRAME: u32 = 1 << 0;

    pub fn keyframe(&self) -> bool {
        self.flags & Self::KEYFRAME != 0
    }
}

pod! {
    /// One entry of an event ring. Build and read it through [`Message`]; `kind` decides which
    /// fields mean anything.
    pub struct Event {
        pub kind: u32,
        pub hand: u32,
        pub time_mac_ns: i64,
        pub duration_ns: i64,
        pub value: u64,
        pub amplitude: f32,
        pub frequency_hz: f32,
    }
}

/// Left and right, OpenXR's `/user/hand/left` and `/user/hand/right`.
pub const HANDS: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Message {
    /// App → `tetherd`. `frequency_hz` 0 means the headset's default; `duration_ns` is never
    /// negative, so the runtime resolves `XR_MIN_HAPTIC_DURATION` before sending.
    Haptic {
        hand: u32,
        amplitude: f32,
        duration_ns: i64,
        frequency_hz: f32,
    },
    /// App → `tetherd`.
    StopHaptic { hand: u32 },
    /// `tetherd` → app.
    Focus { focused: bool, at_mac_ns: i64 },
    /// `tetherd` → app: `count` is [`SessionState::recenters`] after this one.
    Recenter { count: u64, at_mac_ns: i64 },
    /// `tetherd` → app, while the app encodes (R2 S1 and S2).
    KeyframeRequest,
    /// Only for tests and probes: the receiver answers with the same value.
    Echo { value: u64, sent_mac_ns: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Malformed(pub Event);

impl Event {
    const HAPTIC: u32 = 1;
    const STOP_HAPTIC: u32 = 2;
    const FOCUS: u32 = 3;
    const RECENTER: u32 = 4;
    const KEYFRAME_REQUEST: u32 = 5;
    const ECHO: u32 = 6;

    pub fn decode(&self) -> Result<Message, Malformed> {
        let hand = (self.hand < HANDS).then_some(self.hand);
        let message = match self.kind {
            Self::HAPTIC => Message::Haptic {
                hand: hand.ok_or(Malformed(*self))?,
                amplitude: self.amplitude,
                duration_ns: self.duration_ns,
                frequency_hz: self.frequency_hz,
            },
            Self::STOP_HAPTIC => Message::StopHaptic {
                hand: hand.ok_or(Malformed(*self))?,
            },
            Self::FOCUS => Message::Focus {
                focused: self.value != 0,
                at_mac_ns: self.time_mac_ns,
            },
            Self::RECENTER => Message::Recenter {
                count: self.value,
                at_mac_ns: self.time_mac_ns,
            },
            Self::KEYFRAME_REQUEST => Message::KeyframeRequest,
            Self::ECHO => Message::Echo {
                value: self.value,
                sent_mac_ns: self.time_mac_ns,
            },
            _ => return Err(Malformed(*self)),
        };
        let valid = match message {
            Message::Haptic {
                amplitude,
                duration_ns,
                frequency_hz,
                ..
            } => {
                (0.0..=1.0).contains(&amplitude)
                    && duration_ns >= 0
                    && frequency_hz.is_finite()
                    && frequency_hz >= 0.0
            }
            _ => true,
        };
        if valid {
            Ok(message)
        } else {
            Err(Malformed(*self))
        }
    }
}

impl From<Message> for Event {
    fn from(message: Message) -> Event {
        let event = Event::default();
        match message {
            Message::Haptic {
                hand,
                amplitude,
                duration_ns,
                frequency_hz,
            } => Event {
                kind: Event::HAPTIC,
                hand,
                amplitude,
                duration_ns,
                frequency_hz,
                ..event
            },
            Message::StopHaptic { hand } => Event {
                kind: Event::STOP_HAPTIC,
                hand,
                ..event
            },
            Message::Focus { focused, at_mac_ns } => Event {
                kind: Event::FOCUS,
                value: u64::from(focused),
                time_mac_ns: at_mac_ns,
                ..event
            },
            Message::Recenter { count, at_mac_ns } => Event {
                kind: Event::RECENTER,
                value: count,
                time_mac_ns: at_mac_ns,
                ..event
            },
            Message::KeyframeRequest => Event {
                kind: Event::KEYFRAME_REQUEST,
                ..event
            },
            Message::Echo { value, sent_mac_ns } => Event {
                kind: Event::ECHO,
                value,
                time_mac_ns: sent_mac_ns,
                ..event
            },
        }
    }
}

fn text(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&bytes[..end]).ok()
}

fn set_text(bytes: &mut [u8], value: &str) {
    let mut end = value.len().min(bytes.len() - 1);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    bytes.fill(0);
    bytes[..end].copy_from_slice(&value.as_bytes()[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pod;

    #[test]
    fn messages_round_trip() {
        let messages = [
            Message::Haptic {
                hand: 1,
                amplitude: 0.5,
                duration_ns: 20_000_000,
                frequency_hz: 0.0,
            },
            Message::StopHaptic { hand: 0 },
            Message::Focus {
                focused: true,
                at_mac_ns: 42,
            },
            Message::Recenter {
                count: 3,
                at_mac_ns: -1,
            },
            Message::KeyframeRequest,
            Message::Echo {
                value: u64::MAX,
                sent_mac_ns: 7,
            },
        ];
        for message in messages {
            let event = Event::from(message);
            let bytes = event.as_bytes().to_vec();
            assert_eq!(Event::from_bytes(&bytes).unwrap().decode(), Ok(message));
        }
    }

    #[test]
    fn rejects_what_a_peer_cannot_mean() {
        let haptic = Event::from(Message::Haptic {
            hand: 0,
            amplitude: 1.0,
            duration_ns: 1,
            frequency_hz: 1.0,
        });
        for bad in [
            Event { kind: 0, ..haptic },
            Event { kind: 99, ..haptic },
            Event { hand: 2, ..haptic },
            Event {
                amplitude: f32::NAN,
                ..haptic
            },
            Event {
                amplitude: 1.5,
                ..haptic
            },
            Event {
                duration_ns: -1,
                ..haptic
            },
            Event {
                frequency_hz: f32::INFINITY,
                ..haptic
            },
        ] {
            assert!(bad.decode().is_err(), "{bad:?} decoded");
        }
    }

    #[test]
    fn strings_need_a_terminator_and_valid_utf8() {
        let mut info = HeadsetInfo::default();
        info.set_name("Quest 3");
        assert_eq!(info.name(), Some("Quest 3"));
        info.name = [b'x'; 64];
        assert_eq!(info.name(), None);
        info.name[..3].copy_from_slice(&[0xff, 0xfe, 0]);
        assert_eq!(info.name(), None);
        info.set_name(&"é".repeat(40));
        assert_eq!(info.name().map(str::len), Some(62));
        info.refresh_rate_count = u32::MAX;
        assert_eq!(info.refresh_rates().len(), 8);
    }

    #[test]
    fn clock_mapping_round_trips() {
        let clock = ClockMapping {
            offset_ns: 10_000_000_000,
            ..ClockMapping::default()
        };
        assert_eq!(clock.to_headset(5), HeadsetTime { ns: 10_000_000_005 });
        assert_eq!(clock.to_mac(clock.to_headset(-9)), -9);
    }
}
