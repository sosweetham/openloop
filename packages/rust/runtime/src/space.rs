//! Spaces, all resolved against the headset's tracking space (its STAGE: floor at y = 0).

use glam::Vec3;
use tether_openxr_abi::{
    XR_SPACE_LOCATION_ORIENTATION_TRACKED_BIT, XR_SPACE_LOCATION_ORIENTATION_VALID_BIT,
    XR_SPACE_LOCATION_POSITION_TRACKED_BIT, XR_SPACE_LOCATION_POSITION_VALID_BIT,
    XR_SPACE_VELOCITY_ANGULAR_VALID_BIT, XR_SPACE_VELOCITY_LINEAR_VALID_BIT,
    XR_VIEW_STATE_ORIENTATION_TRACKED_BIT, XR_VIEW_STATE_ORIENTATION_VALID_BIT,
    XR_VIEW_STATE_POSITION_TRACKED_BIT, XR_VIEW_STATE_POSITION_VALID_BIT, XrActionSpaceCreateInfo,
    XrActionType, XrExtent2Df, XrPath, XrPosef, XrReferenceSpaceCreateInfo, XrReferenceSpaceType,
    XrResult, XrSession, XrSpace, XrSpaceLocation, XrSpaceLocationData, XrSpaceLocations,
    XrSpaceVelocities, XrSpaceVelocity, XrSpaceVelocityData, XrSpacesLocateInfo, XrStructureType,
    XrTime, XrView, XrViewLocateInfo, XrViewState,
};
use tether_stream::pose::Rigid;

use crate::actions::{self, PoseKind};
use crate::ffi::{self, OK};
use crate::handles::{handle, id};
use crate::session::{self, Session};
use crate::state::{OBJECTS, headset};
use crate::{convert, system};

/// Step for finite-difference velocities.
const VELOCITY_DT_NS: i64 = 5_000_000;

pub enum Kind {
    Reference(XrReferenceSpaceType),
    Action { action: u64, subaction: XrPath },
}

pub struct Space {
    pub session: u64,
    kind: Kind,
    offset: Rigid,
}

/// The space's pose in tracking space at `time`; `None` while untracked.
pub fn pose_in_tracking(space: &Space, session: &Session, time: i64) -> Option<Rigid> {
    let origin = match space.kind {
        Kind::Reference(XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_VIEW) => {
            headset().state().head_at(time)
        }
        Kind::Reference(XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_LOCAL) => {
            session.local_anchor()
        }
        Kind::Reference(XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_LOCAL_FLOOR) => {
            let local = session.local_anchor();
            Rigid {
                translation: Vec3::new(local.translation.x, 0.0, local.translation.z),
                ..local
            }
        }
        Kind::Reference(_) => Rigid::IDENTITY,
        Kind::Action { action, subaction } => {
            let (hand, kind) = actions::pose_binding(session, action, subaction)?;
            let (grip, aim) = headset().state().hand_at(hand, time)?;
            match kind {
                PoseKind::Grip => grip,
                PoseKind::Aim => aim,
            }
        }
    };
    Some(origin * space.offset)
}

fn relative(space: &Space, base: &Space, session: &Session, time: i64) -> Option<Rigid> {
    Some(pose_in_tracking(base, session, time)?.inverse() * pose_in_tracking(space, session, time)?)
}

fn velocity(space: &Space, base: &Space, session: &Session, time: i64) -> Option<(Vec3, Vec3)> {
    let a = relative(space, base, session, time)?;
    let b = relative(space, base, session, time + VELOCITY_DT_NS)?;
    let dt = VELOCITY_DT_NS as f32 * 1e-9;
    let linear = (b.translation - a.translation) / dt;
    let (axis, angle) = (b.rotation * a.rotation.inverse()).to_axis_angle();
    Some((linear, axis * angle / dt))
}

fn reference_spaces(session: &Session) -> Vec<XrReferenceSpaceType> {
    let mut spaces = vec![
        XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_VIEW,
        XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_LOCAL,
        XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_STAGE,
    ];
    if OBJECTS
        .instances
        .get(session.instance)
        .is_some_and(|i| i.api_minor() >= 1)
    {
        spaces.push(XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_LOCAL_FLOOR);
    }
    spaces
}

fn valid_pose(p: &XrPosef) -> bool {
    let q = p.orientation;
    let norm = (q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w).sqrt();
    (norm - 1.0).abs() < 0.01
}

fn lookup(space: XrSpace) -> Result<std::sync::Arc<Space>, XrResult> {
    OBJECTS
        .spaces
        .get(id(space))
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)
}

pub unsafe extern "C" fn xr_enumerate_reference_spaces(
    session_handle: XrSession,
    capacity: u32,
    count: *mut u32,
    spaces: *mut XrReferenceSpaceType,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&reference_spaces(&s), capacity, count, spaces, |d, v| {
                let _: () = *d = *v;
                Ok(())
            })
        })
    })
}

pub unsafe extern "C" fn xr_create_reference_space(
    session_handle: XrSession,
    info: *const XrReferenceSpaceCreateInfo,
    out: *mut XrSpace,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(
            info.type_,
            XrStructureType::XR_TYPE_REFERENCE_SPACE_CREATE_INFO,
        )?;
        if !reference_spaces(&s).contains(&info.referenceSpaceType) {
            return Err(XrResult::XR_ERROR_REFERENCE_SPACE_UNSUPPORTED);
        }
        if !valid_pose(&info.poseInReferenceSpace) {
            return Err(XrResult::XR_ERROR_POSE_INVALID);
        }
        *out = handle(OBJECTS.spaces.insert(Space {
            session: id(session_handle),
            kind: Kind::Reference(info.referenceSpaceType),
            offset: convert::rigid(&info.poseInReferenceSpace),
        }));
        OK
    })
}

pub unsafe extern "C" fn xr_get_reference_space_bounds_rect(
    session_handle: XrSession,
    ty: XrReferenceSpaceType,
    bounds: *mut XrExtent2Df,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        if !reference_spaces(&s).contains(&ty) {
            return Err(XrResult::XR_ERROR_REFERENCE_SPACE_UNSUPPORTED);
        }
        // SAFETY: caller-provided output struct.
        *unsafe { ffi::write(bounds) }? = XrExtent2Df {
            width: 0.0,
            height: 0.0,
        };
        Ok(XrResult::XR_SPACE_BOUNDS_UNAVAILABLE)
    })
}

pub unsafe extern "C" fn xr_create_action_space(
    session_handle: XrSession,
    info: *const XrActionSpaceCreateInfo,
    out: *mut XrSpace,
) -> XrResult {
    ffi::call(|| {
        session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(
            info.type_,
            XrStructureType::XR_TYPE_ACTION_SPACE_CREATE_INFO,
        )?;
        let action = OBJECTS
            .actions
            .get(id(info.action))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        if action.ty != XrActionType::XR_ACTION_TYPE_POSE_INPUT {
            return Err(XrResult::XR_ERROR_ACTION_TYPE_MISMATCH);
        }
        if info.subactionPath != 0 && !action.subactions.contains(&info.subactionPath) {
            return Err(XrResult::XR_ERROR_PATH_UNSUPPORTED);
        }
        if !valid_pose(&info.poseInActionSpace) {
            return Err(XrResult::XR_ERROR_POSE_INVALID);
        }
        *out = handle(OBJECTS.spaces.insert(Space {
            session: id(session_handle),
            kind: Kind::Action {
                action: id(info.action),
                subaction: info.subactionPath,
            },
            offset: convert::rigid(&info.poseInActionSpace),
        }));
        OK
    })
}

pub unsafe extern "C" fn xr_destroy_space(space: XrSpace) -> XrResult {
    ffi::call(|| {
        OBJECTS
            .spaces
            .remove(id(space))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        OK
    })
}

const LOCATED: u64 = XR_SPACE_LOCATION_ORIENTATION_VALID_BIT
    | XR_SPACE_LOCATION_POSITION_VALID_BIT
    | XR_SPACE_LOCATION_ORIENTATION_TRACKED_BIT
    | XR_SPACE_LOCATION_POSITION_TRACKED_BIT;

pub unsafe extern "C" fn xr_locate_space(
    space: XrSpace,
    base: XrSpace,
    time: XrTime,
    location: *mut XrSpaceLocation,
) -> XrResult {
    ffi::call(|| {
        let (space, base) = (lookup(space)?, lookup(base)?);
        let s = OBJECTS
            .sessions
            .get(space.session)
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        if time <= 0 {
            return Err(XrResult::XR_ERROR_TIME_INVALID);
        }
        // SAFETY: caller-provided output struct and its chain.
        let location = unsafe { ffi::write(location) }?;
        ffi::expect_type(location.type_, XrStructureType::XR_TYPE_SPACE_LOCATION)?;
        let pose = relative(&space, &base, &s, time);
        location.locationFlags = if pose.is_some() { LOCATED } else { 0 };
        location.pose = convert::posef(pose.unwrap_or(Rigid::IDENTITY));
        if let Some(v) = unsafe {
            ffi::find_in_chain::<XrSpaceVelocity>(
                location.next,
                XrStructureType::XR_TYPE_SPACE_VELOCITY,
            )
        } {
            // SAFETY: found in the app's output chain.
            let v = unsafe { &mut *v };
            let velocity = velocity(&space, &base, &s, time);
            let (linear, angular) = velocity.unwrap_or((Vec3::ZERO, Vec3::ZERO));
            v.velocityFlags = if velocity.is_some() {
                XR_SPACE_VELOCITY_LINEAR_VALID_BIT | XR_SPACE_VELOCITY_ANGULAR_VALID_BIT
            } else {
                0
            };
            v.linearVelocity = convert::vector(linear);
            v.angularVelocity = convert::vector(angular);
        }
        OK
    })
}

pub unsafe extern "C" fn xr_locate_spaces(
    session_handle: XrSession,
    info: *const XrSpacesLocateInfo,
    locations: *mut XrSpaceLocations,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(locations) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_SPACES_LOCATE_INFO)?;
        ffi::expect_type(out.type_, XrStructureType::XR_TYPE_SPACE_LOCATIONS)?;
        if info.time <= 0 {
            return Err(XrResult::XR_ERROR_TIME_INVALID);
        }
        if out.locationCount != info.spaceCount {
            return Err(XrResult::XR_ERROR_VALIDATION_FAILURE);
        }
        let base = lookup(info.baseSpace)?;
        // SAFETY: counted arrays from the app.
        let spaces = unsafe { ffi::slice(info.spaces, info.spaceCount) }?;
        let data: &mut [XrSpaceLocationData] =
            unsafe { std::slice::from_raw_parts_mut(out.locations, out.locationCount as usize) };
        let velocities = unsafe {
            ffi::find_in_chain::<XrSpaceVelocities>(
                out.next,
                XrStructureType::XR_TYPE_SPACE_VELOCITIES,
            )
        }
        .map(|v| unsafe {
            std::slice::from_raw_parts_mut((*v).velocities, (*v).velocityCount as usize)
                as &mut [XrSpaceVelocityData]
        });
        let mut velocities = velocities;
        for (i, (&space, slot)) in spaces.iter().zip(data.iter_mut()).enumerate() {
            let space = lookup(space)?;
            let pose = relative(&space, &base, &s, info.time);
            slot.locationFlags = if pose.is_some() { LOCATED } else { 0 };
            slot.pose = convert::posef(pose.unwrap_or(Rigid::IDENTITY));
            if let Some(v) = velocities.as_deref_mut().and_then(|v| v.get_mut(i)) {
                let velocity = velocity(&space, &base, &s, info.time);
                let (linear, angular) = velocity.unwrap_or((Vec3::ZERO, Vec3::ZERO));
                v.velocityFlags = if velocity.is_some() {
                    XR_SPACE_VELOCITY_LINEAR_VALID_BIT | XR_SPACE_VELOCITY_ANGULAR_VALID_BIT
                } else {
                    0
                };
                v.linearVelocity = convert::vector(linear);
                v.angularVelocity = convert::vector(angular);
            }
        }
        OK
    })
}

pub unsafe extern "C" fn xr_locate_views(
    session_handle: XrSession,
    info: *const XrViewLocateInfo,
    state: *mut XrViewState,
    capacity: u32,
    count: *mut u32,
    views: *mut XrView,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let state = unsafe { ffi::write(state) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_VIEW_LOCATE_INFO)?;
        ffi::expect_type(state.type_, XrStructureType::XR_TYPE_VIEW_STATE)?;
        if info.viewConfigurationType != system::STEREO {
            return Err(XrResult::XR_ERROR_VIEW_CONFIGURATION_TYPE_UNSUPPORTED);
        }
        if info.displayTime <= 0 {
            return Err(XrResult::XR_ERROR_TIME_INVALID);
        }
        let base = lookup(info.space)?;
        let base_pose = pose_in_tracking(&base, &s, info.displayTime).unwrap_or(Rigid::IDENTITY);
        let (head, eyes) = {
            let headset = headset().state();
            (headset.head_at(info.displayTime), headset.views())
        };
        state.viewStateFlags = XR_VIEW_STATE_ORIENTATION_VALID_BIT
            | XR_VIEW_STATE_POSITION_VALID_BIT
            | XR_VIEW_STATE_ORIENTATION_TRACKED_BIT
            | XR_VIEW_STATE_POSITION_TRACKED_BIT;
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&eyes, capacity, count, views, |view, eye| {
                ffi::expect_type(view.type_, XrStructureType::XR_TYPE_VIEW)?;
                view.pose =
                    convert::posef(base_pose.inverse() * head * Rigid::from_pose(&eye.pose));
                view.fov = convert::fovf(&eye.fov);
                Ok(())
            })
        })
    })
}
