use glam::{Quat, Vec3};
use openloop_protocol::{Fov, Pose, Velocity, View};

/// A rigid transform. `a * b` applies `b` first, then `a`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rigid {
    pub rotation: Quat,
    pub translation: Vec3,
}

impl Rigid {
    pub const IDENTITY: Rigid = Rigid {
        rotation: Quat::IDENTITY,
        translation: Vec3::ZERO,
    };

    pub fn new(rotation: Quat, translation: Vec3) -> Self {
        Self {
            rotation: rotation.normalize(),
            translation,
        }
    }

    pub fn from_pose(p: &Pose) -> Self {
        Self::new(
            Quat::from_array(p.orientation),
            Vec3::from_array(p.position),
        )
    }

    pub fn to_pose(self) -> Pose {
        Pose {
            position: self.translation.to_array(),
            orientation: self.rotation.to_array(),
        }
    }

    pub fn inverse(self) -> Self {
        let rotation = self.rotation.inverse();
        Self {
            rotation,
            translation: rotation * -self.translation,
        }
    }

    /// Yaw-only, gravity-aligned copy, as OpenXR's LOCAL space requires.
    pub fn yaw_only(self) -> Self {
        let forward = self.rotation * Vec3::NEG_Z;
        let yaw = (-forward.x).atan2(-forward.z);
        let yaw = if yaw.is_finite() { yaw } else { 0.0 };
        Self {
            rotation: Quat::from_rotation_y(yaw),
            translation: self.translation,
        }
    }
}

impl std::ops::Mul for Rigid {
    type Output = Rigid;
    fn mul(self, rhs: Rigid) -> Rigid {
        Rigid {
            rotation: (self.rotation * rhs.rotation).normalize(),
            translation: self.rotation * rhs.translation + self.translation,
        }
    }
}

/// Constant-velocity extrapolation; velocities are in the same space as the pose.
pub fn predict(pose: Rigid, velocity: &Velocity, seconds: f32) -> Rigid {
    let linear = Vec3::from_array(velocity.linear);
    let angular = Vec3::from_array(velocity.angular);
    let spin = if angular.length_squared() > 0.0 {
        Quat::from_scaled_axis(angular * seconds)
    } else {
        Quat::IDENTITY
    };
    Rigid::new(spin * pose.rotation, pose.translation + linear * seconds)
}

/// What an app sees before any headset has connected: a standing viewer at the origin.
pub fn default_views() -> [View; 2] {
    let eye = |x: f32, fov: Fov| View {
        pose: Pose {
            position: [x, 0.0, 0.0],
            ..Pose::IDENTITY
        },
        fov,
    };
    [
        eye(
            -0.032,
            Fov {
                left: -0.94,
                right: 0.79,
                up: 0.86,
                down: -0.94,
            },
        ),
        eye(
            0.032,
            Fov {
                left: -0.79,
                right: 0.94,
                up: 0.86,
                down: -0.94,
            },
        ),
    ]
}

pub const DEFAULT_HEAD: Rigid = Rigid {
    rotation: Quat::IDENTITY,
    translation: Vec3::new(0.0, 1.6, 0.0),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Rigid, b: Rigid) -> bool {
        a.translation.abs_diff_eq(b.translation, 1e-5) && a.rotation.abs_diff_eq(b.rotation, 1e-5)
    }

    #[test]
    fn inverse_composes_to_identity() {
        let t = Rigid::new(
            Quat::from_rotation_y(0.7) * Quat::from_rotation_x(0.2),
            Vec3::new(1.0, 2.0, 3.0),
        );
        assert!(close(t * t.inverse(), Rigid::IDENTITY));
        assert!(close(t.inverse() * t, Rigid::IDENTITY));
    }

    #[test]
    fn prediction_follows_velocity() {
        let v = Velocity {
            linear: [1.0, 0.0, 0.0],
            angular: [0.0, std::f32::consts::PI, 0.0],
        };
        let p = predict(Rigid::IDENTITY, &v, 0.5);
        assert!(p.translation.abs_diff_eq(Vec3::new(0.5, 0.0, 0.0), 1e-5));
        assert!(
            p.rotation
                .abs_diff_eq(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2), 1e-5)
        );
    }

    #[test]
    fn yaw_only_keeps_heading_and_drops_tilt() {
        let tilted = Rigid::new(
            Quat::from_rotation_y(0.5) * Quat::from_rotation_x(0.4),
            Vec3::ONE,
        );
        let y = tilted.yaw_only();
        assert!(y.rotation.abs_diff_eq(Quat::from_rotation_y(0.5), 1e-5));
        assert_eq!(y.translation, Vec3::ONE);
    }
}
