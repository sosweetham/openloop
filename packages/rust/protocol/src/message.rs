/// Position in metres, orientation as a unit quaternion `[x, y, z, w]`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pose {
    pub position: [f32; 3],
    pub orientation: [f32; 4],
}

impl Pose {
    pub const IDENTITY: Pose = Pose {
        position: [0.0; 3],
        orientation: [0.0, 0.0, 0.0, 1.0],
    };
}

/// Half-angles in radians, OpenXR convention (left and down negative).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fov {
    pub left: f32,
    pub right: f32,
    pub up: f32,
    pub down: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Velocity {
    pub linear: [f32; 3],
    pub angular: [f32; 3],
}

/// One eye: pose and field of view.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct View {
    pub pose: Pose,
    pub fov: Fov,
}

/// Bitflags for [`ControllerState::buttons`] and [`ControllerState::touches`].
pub mod button {
    pub const PRIMARY: u32 = 1 << 0; // A / X
    pub const SECONDARY: u32 = 1 << 1; // B / Y
    pub const THUMBSTICK: u32 = 1 << 2;
    pub const MENU: u32 = 1 << 3;
    pub const TRIGGER: u32 = 1 << 4;
    pub const SQUEEZE: u32 = 1 << 5;
    pub const THUMBREST: u32 = 1 << 6;
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ControllerState {
    pub active: bool,
    /// Poses in the client's tracking space.
    pub grip: Pose,
    pub aim: Pose,
    pub grip_velocity: Velocity,
    pub buttons: u32,
    pub touches: u32,
    pub trigger: f32,
    pub squeeze: f32,
    pub thumbstick: [f32; 2],
}

/// Nanoseconds on the headset client's clock (CLOCK_MONOTONIC on Android). The Mac converts
/// into it with its clock mapping; the client compares it with its own clock directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientTime(pub i64);

/// Everything the Mac needs to configure itself for a headset; the Mac assumes nothing else.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceProfile {
    pub name: String,
    /// Recommended per-eye render size.
    pub eye_width: u32,
    pub eye_height: u32,
    pub refresh_rates: Vec<f32>,
    pub refresh_rate: f32,
    pub interaction_profile: String,
    pub supports_hevc: bool,
    /// The rate the client's audio output runs at; the Mac sends audio at this rate and no
    /// other. 0: the client plays no audio, so the Mac sends none.
    pub audio_sample_rate: u32,
    /// At most [`MAX_AUDIO_CHANNELS`].
    pub audio_channels: u8,
}

/// The client's first message on every link. `version` leads the payload in every protocol
/// version, so the Mac can always read it and refuse a client it cannot serve.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientHello {
    pub version: u16,
    pub profile: DeviceProfile,
}

/// Starts a video stream: frames of this size follow, the first a keyframe.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerHello {
    pub version: u16,
    /// Encoded video size, both eyes side by side.
    pub video_width: u32,
    pub video_height: u32,
}

/// A tracking sample. Times are the client's clock, nanoseconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tracking {
    pub sample_time_ns: i64,
    /// Head pose in the client's tracking space (STAGE, floor at y = 0).
    pub head: Pose,
    pub head_velocity: Velocity,
    /// Eyes relative to the head.
    pub views: [View; 2],
    pub controllers: [ControllerState; 2],
}

/// An encoded video frame and the views it was rendered for, in the client's tracking space.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoFrame {
    pub frame_index: u64,
    /// When the frame was predicted to be displayed, client clock.
    pub display_time_ns: i64,
    pub keyframe: bool,
    pub views: [View; 2],
    /// Annex-B HEVC access unit.
    pub data: Vec<u8>,
}

/// Everything in a [`VideoFrame`] but its data.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VideoHeader {
    pub frame_index: u64,
    /// When the frame was predicted to be displayed, client clock.
    pub display_time_ns: i64,
    pub keyframe: bool,
    pub views: [View; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Haptics {
    pub hand: u8,
    pub amplitude: f32,
    /// 0 stops any running vibration.
    pub duration_ns: i64,
    pub frequency: f32,
}

/// The most frames one [`AudioPacket`] carries (10 ms at 48 kHz), so a receiver can hold any
/// packet in storage sized once.
pub const MAX_AUDIO_FRAMES: usize = 480;
pub const MAX_AUDIO_CHANNELS: u8 = 2;

/// What an audio stream carries. Every packet repeats it, so a receiver that missed
/// [`Message::AudioStart`] still knows what it is playing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    /// One continuous capture. A new value restarts playout.
    pub stream: u16,
    /// The client's [`DeviceProfile::audio_sample_rate`].
    pub sample_rate: u32,
    pub channels: u8,
}

/// Everything in an [`AudioPacket`] but its samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioHeader {
    pub format: AudioFormat,
    /// +1 per packet within a stream; a gap is a lost packet.
    pub sequence: u32,
    /// Index of the packet's first frame, counted from the start of the stream on the Mac's
    /// audio clock.
    pub first_frame: u64,
    /// When that frame is to be heard.
    pub present_at: ClientTime,
}

/// One period of the app's audio, scheduled on the client's clock.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioPacket {
    pub header: AudioHeader,
    /// Signed 16-bit PCM, interleaved, `header.format.channels` samples per frame. A decoded
    /// packet holds 1 to [`MAX_AUDIO_CHANNELS`] channels and at most [`MAX_AUDIO_FRAMES`] whole
    /// frames.
    pub samples: Vec<i16>,
}

impl AudioPacket {
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.header.format.channels.max(1))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    Tracking(Box<Tracking>),
    /// Mac → client; the client answers immediately with a pong echoing `mac_ns`.
    ClockPing {
        mac_ns: i64,
    },
    ClockPong {
        mac_ns: i64,
        client_ns: i64,
    },
    VideoFrame(VideoFrame),
    /// The headset's system recenter fired.
    Recenter,
    Haptics(Haptics),
    /// The client lost decoder state and needs a keyframe to recover.
    KeyframeRequest,
    /// Client → Mac: whether the app has input focus. Tether's menu takes it while open; the
    /// runtime then moves the app's session to VISIBLE, which apps answer by pausing.
    InputFocus {
        focused: bool,
    },
    /// Mac → client: the app's video stream is over. The client drops what it holds of it and
    /// shows its idle state until the next [`ServerHello`].
    StreamEnded,
    /// Mac → client: an audio stream starts and its packets follow, so the client can open its
    /// output ahead of them. One stream plays at a time; this one replaces any other.
    AudioStart(AudioFormat),
    /// Mac → client.
    Audio(AudioPacket),
    /// Mac → client: no more packets of `stream` will come.
    AudioStop {
        stream: u16,
    },
}
