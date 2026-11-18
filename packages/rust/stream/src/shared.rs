//! The headset's profile and tracking as openloop-ipc's shared-memory records: `openloopd` publishes
//! them with these conversions and the runtime reads them back with their inverses
//! (docs/ARCHITECTURE.md § Runtime ↔ openloopd).

use openloop_ipc::records::{self, Controller, HeadsetInfo, TrackingSample};
use openloop_protocol::{ControllerState, DeviceProfile, Fov, Pose, Tracking, Velocity, View};

use crate::headset::{Tracked, tracking_is_sane};

pub fn headset_info(profile: &DeviceProfile, connected: bool) -> HeadsetInfo {
    let mut info = <HeadsetInfo as openloop_ipc::Pod>::zeroed();
    info.eye_width = profile.eye_width;
    info.eye_height = profile.eye_height;
    info.refresh_rate = profile.refresh_rate;
    let rates = profile.refresh_rates.len().min(info.refresh_rates.len());
    info.refresh_rates[..rates].copy_from_slice(&profile.refresh_rates[..rates]);
    info.refresh_rate_count = rates as u32;
    info.audio_sample_rate = profile.audio_sample_rate;
    info.audio_channels = u32::from(profile.audio_channels);
    info.flags = if connected { HeadsetInfo::CONNECTED } else { 0 }
        | if profile.supports_hevc {
            HeadsetInfo::HEVC
        } else {
            0
        };
    info.set_name(&profile.name);
    info.set_interaction_profile(&profile.interaction_profile);
    info
}

/// What `openloopd` wrote; a string without a terminator reads as empty.
pub fn device_profile(info: &HeadsetInfo) -> DeviceProfile {
    DeviceProfile {
        name: info.name().unwrap_or_default().to_owned(),
        eye_width: info.eye_width,
        eye_height: info.eye_height,
        refresh_rates: info.refresh_rates().to_vec(),
        refresh_rate: info.refresh_rate,
        interaction_profile: info.interaction_profile().unwrap_or_default().to_owned(),
        supports_hevc: info.flags & HeadsetInfo::HEVC != 0,
        audio_sample_rate: info.audio_sample_rate,
        audio_channels: info.audio_channels.min(u32::from(u8::MAX)) as u8,
    }
}

/// `None` before the first sample.
pub fn tracking_sample(tracked: &Tracked, received_mac_ns: i64) -> Option<TrackingSample> {
    let (t, sample_mac_ns) = tracked.sample.as_ref()?;
    Some(TrackingSample {
        sample_mac_ns: *sample_mac_ns,
        received_mac_ns,
        head: pose(&t.head),
        head_velocity: velocity(&t.head_velocity),
        views: t.views.map(|v| record_view(&v)),
        controllers: t.controllers.map(|c| Controller {
            grip: pose(&c.grip),
            aim: pose(&c.aim),
            grip_velocity: velocity(&c.grip_velocity),
            trigger: c.trigger,
            squeeze: c.squeeze,
            thumbstick: c.thumbstick,
            buttons: c.buttons,
            touches: c.touches,
            active: u32::from(c.active),
            reserved: 0,
        }),
        flags: if tracked.live {
            TrackingSample::LIVE
        } else {
            0
        },
    })
}

/// A sample whose poses are not finite unit transforms is dropped, as on the link.
pub fn tracked(sample: &TrackingSample, live: bool) -> Tracked {
    let tracking = Tracking {
        sample_time_ns: sample.sample_mac_ns,
        head: from_pose(&sample.head),
        head_velocity: from_velocity(&sample.head_velocity),
        views: sample.views.map(|v| View {
            pose: from_pose(&v.pose),
            fov: Fov {
                left: v.fov.left,
                right: v.fov.right,
                up: v.fov.up,
                down: v.fov.down,
            },
        }),
        controllers: sample.controllers.map(|c| ControllerState {
            active: c.active != 0,
            grip: from_pose(&c.grip),
            aim: from_pose(&c.aim),
            grip_velocity: from_velocity(&c.grip_velocity),
            buttons: c.buttons,
            touches: c.touches,
            trigger: c.trigger,
            squeeze: c.squeeze,
            thumbstick: c.thumbstick,
        }),
    };
    if !tracking_is_sane(&tracking) {
        return Tracked::default();
    }
    Tracked {
        sample: Some((tracking, sample.sample_mac_ns)),
        live: live && sample.live(),
    }
}

pub fn record_view(v: &View) -> records::View {
    records::View {
        pose: pose(&v.pose),
        fov: records::Fov {
            left: v.fov.left,
            right: v.fov.right,
            up: v.fov.up,
            down: v.fov.down,
        },
    }
}

fn pose(p: &Pose) -> records::Pose {
    records::Pose {
        position: p.position,
        orientation: p.orientation,
    }
}

fn from_pose(p: &records::Pose) -> Pose {
    Pose {
        position: p.position,
        orientation: p.orientation,
    }
}

fn velocity(v: &Velocity) -> records::Velocity {
    records::Velocity {
        linear: v.linear,
        angular: v.angular,
    }
}

fn from_velocity(v: &records::Velocity) -> Velocity {
    Velocity {
        linear: v.linear,
        angular: v.angular,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_survives_the_record() {
        let profile = DeviceProfile {
            name: "Quest 3".into(),
            eye_width: 1680,
            eye_height: 1760,
            refresh_rates: vec![72.0, 80.0, 90.0, 120.0],
            refresh_rate: 90.0,
            interaction_profile: "/interaction_profiles/oculus/touch_controller".into(),
            supports_hevc: true,
            audio_sample_rate: 48_000,
            audio_channels: 2,
        };
        let info = headset_info(&profile, true);
        assert!(info.connected());
        assert_eq!(device_profile(&info), profile);
        assert!(!headset_info(&profile, false).connected());
    }

    #[test]
    fn tracking_survives_the_record() {
        let mut controller = ControllerState {
            active: true,
            trigger: 0.5,
            buttons: 3,
            ..Default::default()
        };
        controller.grip = Pose {
            position: [0.1, 1.0, -0.3],
            ..Pose::IDENTITY
        };
        controller.aim = Pose::IDENTITY;
        let original = Tracked {
            sample: Some((
                Tracking {
                    sample_time_ns: 99,
                    head: Pose {
                        position: [0.0, 1.6, 0.0],
                        ..Pose::IDENTITY
                    },
                    views: crate::pose::default_views(),
                    controllers: [controller, ControllerState::default()],
                    ..Default::default()
                },
                1234,
            )),
            live: true,
        };
        let sample = tracking_sample(&original, 1300).unwrap();
        assert!(sample.live());
        let back = tracked(&sample, true);
        assert!(back.live);
        let (tracking, at) = back.sample.as_ref().unwrap();
        assert_eq!(*at, 1234);
        assert_eq!(tracking.head, original.sample.as_ref().unwrap().0.head);
        assert_eq!(
            tracking.controllers,
            original.sample.as_ref().unwrap().0.controllers
        );
        assert!(
            !tracked(&sample, false).live,
            "a stale openloopd holds the pose"
        );
    }

    #[test]
    fn a_garbage_sample_reads_as_none() {
        let mut sample = <TrackingSample as openloop_ipc::Pod>::zeroed();
        sample.flags = TrackingSample::LIVE;
        assert!(
            tracked(&sample, true).sample.is_none(),
            "a zero quaternion is no rotation"
        );
    }
}
