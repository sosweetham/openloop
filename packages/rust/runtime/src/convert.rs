use glam::{Quat, Vec3};
use openloop_openxr_abi::{XrFovf, XrPosef, XrQuaternionf, XrVector3f};
use openloop_protocol::Fov;
use openloop_stream::pose::Rigid;

pub fn rigid(p: &XrPosef) -> Rigid {
    let q = p.orientation;
    let v = p.position;
    Rigid::new(
        Quat::from_xyzw(q.x, q.y, q.z, q.w),
        Vec3::new(v.x, v.y, v.z),
    )
}

pub fn posef(r: Rigid) -> XrPosef {
    let (q, t) = (r.rotation, r.translation);
    XrPosef {
        orientation: XrQuaternionf {
            x: q.x,
            y: q.y,
            z: q.z,
            w: q.w,
        },
        position: XrVector3f {
            x: t.x,
            y: t.y,
            z: t.z,
        },
    }
}

pub fn vector(v: Vec3) -> XrVector3f {
    XrVector3f {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

pub fn fovf(f: &Fov) -> XrFovf {
    XrFovf {
        angleLeft: f.left,
        angleRight: f.right,
        angleUp: f.up,
        angleDown: f.down,
    }
}

pub fn fov(f: &XrFovf) -> Fov {
    Fov {
        left: f.angleLeft,
        right: f.angleRight,
        up: f.angleUp,
        down: f.angleDown,
    }
}
