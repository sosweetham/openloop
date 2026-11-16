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
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClientHello {
    pub version: u16,
    pub profile: DeviceProfile,
}

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Haptics {
    pub hand: u8,
    pub amplitude: f32,
    /// 0 stops any running vibration.
    pub duration_ns: i64,
    pub frequency: f32,
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
}
