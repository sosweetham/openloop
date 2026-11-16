//! The action system. Suggested bindings are stored per interaction profile; the session picks
//! the first profile it can emulate that the app suggested bindings for, and reads its inputs
//! from the controller state the headset streams.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tether_openxr_abi::{
    XR_MIN_HAPTIC_DURATION, XrAction, XrActionCreateInfo, XrActionSet, XrActionSetCreateInfo,
    XrActionStateBoolean, XrActionStateFloat, XrActionStateGetInfo, XrActionStatePose,
    XrActionStateVector2f, XrActionSuggestedBinding, XrActionType, XrActionsSyncInfo,
    XrActiveActionSet, XrBoundSourcesForActionEnumerateInfo, XrEventDataInteractionProfileChanged,
    XrHapticActionInfo, XrHapticBaseHeader, XrHapticVibration, XrInputSourceLocalizedNameGetInfo,
    XrInstance, XrInteractionProfileState, XrInteractionProfileSuggestedBinding, XrPath, XrResult,
    XrSession, XrSessionActionSetsAttachInfo, XrStructureType, XrTime, XrVector2f,
};
use tether_protocol::{ControllerState, Haptics, Message, button};
use tether_stream::time::now_ns;

use crate::ffi::{self, OK};
use crate::handles::{handle, id};
use crate::instance::{self, Instance};
use crate::session::{self, Session};
use crate::state::{OBJECTS, headset};

/// Profiles Tether can feed from Touch controllers, most specific first.
const PROFILES: [&str; 3] = [
    "/interaction_profiles/meta/touch_plus_controller",
    "/interaction_profiles/oculus/touch_controller",
    "/interaction_profiles/khr/simple_controller",
];
const HANDS: [&str; 2] = ["/user/hand/left", "/user/hand/right"];
const TOP_LEVEL: [&str; 5] = [
    "/user/hand/left",
    "/user/hand/right",
    "/user/head",
    "/user/gamepad",
    "/user/treadmill",
];
const DEFAULT_HAPTIC_NS: i64 = 20_000_000;

pub struct ActionSet {
    pub instance: u64,
    name: String,
    localized: String,
    attached: AtomicBool,
}

pub struct Action {
    pub set: u64,
    pub ty: XrActionType,
    pub subactions: Vec<XrPath>,
    name: String,
    localized: String,
    /// Suggested binding paths by interaction profile path.
    bindings: Mutex<HashMap<XrPath, Vec<XrPath>>>,
}

#[derive(Default)]
pub struct SessionInput {
    attached: Vec<u64>,
    active: Vec<u64>,
    profile: Option<XrPath>,
    now: [Option<ControllerState>; 2],
    before: [Option<ControllerState>; 2],
    synced_at: XrTime,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PoseKind {
    Grip,
    Aim,
}

enum Input {
    Bool(bool),
    Float(f32),
    Vector([f32; 2]),
    Pose,
    Haptic,
}

/// One controller component, e.g. `/input/trigger/value`.
fn read(c: &ControllerState, component: &str) -> Option<Input> {
    let pressed = |b| c.buttons & b != 0;
    let touched = |b| c.touches & b != 0;
    Some(match component {
        "/input/a/click" | "/input/x/click" => Input::Bool(pressed(button::PRIMARY)),
        "/input/a/touch" | "/input/x/touch" => Input::Bool(touched(button::PRIMARY)),
        "/input/b/click" | "/input/y/click" => Input::Bool(pressed(button::SECONDARY)),
        "/input/b/touch" | "/input/y/touch" => Input::Bool(touched(button::SECONDARY)),
        "/input/menu/click" => Input::Bool(pressed(button::MENU)),
        "/input/select/click" | "/input/trigger/click" => Input::Bool(c.trigger > 0.5),
        "/input/trigger/value" => Input::Float(c.trigger),
        "/input/trigger/touch" => Input::Bool(touched(button::TRIGGER)),
        "/input/squeeze/value" => Input::Float(c.squeeze),
        "/input/squeeze/click" => Input::Bool(c.squeeze > 0.5),
        "/input/thumbstick" => Input::Vector(c.thumbstick),
        "/input/thumbstick/x" => Input::Float(c.thumbstick[0]),
        "/input/thumbstick/y" => Input::Float(c.thumbstick[1]),
        "/input/thumbstick/click" => Input::Bool(pressed(button::THUMBSTICK)),
        "/input/thumbstick/touch" => Input::Bool(touched(button::THUMBSTICK)),
        "/input/thumbrest/touch" => Input::Bool(touched(button::THUMBREST)),
        "/input/grip/pose" => Input::Pose,
        "/input/aim/pose" => Input::Pose,
        "/output/haptic" => Input::Haptic,
        _ => return None,
    })
}

/// The `(hand, component)` sources an action is bound to in the session's profile.
fn sources(
    instance: &Instance,
    action: &Action,
    profile: XrPath,
    subaction: XrPath,
) -> Vec<(usize, String)> {
    let filter = (subaction != 0)
        .then(|| instance.path_name(subaction))
        .flatten();
    let bindings = action.bindings.lock().unwrap_or_else(|e| e.into_inner());
    bindings
        .get(&profile)
        .into_iter()
        .flatten()
        .filter_map(|&p| instance.path_name(p))
        .filter_map(|path| {
            let hand = HANDS
                .iter()
                .position(|h| path.starts_with(h) && path[h.len()..].starts_with('/'))?;
            if filter.as_deref().is_some_and(|f| f != HANDS[hand]) {
                return None;
            }
            Some((hand, path[HANDS[hand].len()..].to_owned()))
        })
        .collect()
}

pub fn destroy_children_of_instance(instance: u64) {
    OBJECTS.action_sets.remove_where(|s| s.instance == instance);
    OBJECTS
        .actions
        .remove_where(|a| OBJECTS.action_sets.get(a.set).is_none());
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_' | '.'))
}

pub unsafe extern "C" fn xr_create_action_set(
    instance_handle: XrInstance,
    info: *const XrActionSetCreateInfo,
    out: *mut XrActionSet,
) -> XrResult {
    ffi::call(|| {
        instance::lookup(instance_handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_ACTION_SET_CREATE_INFO)?;
        let name = ffi::fixed_str(&info.actionSetName)
            .filter(|n| valid_name(n))
            .ok_or(XrResult::XR_ERROR_PATH_FORMAT_INVALID)?;
        let localized = ffi::fixed_str(&info.localizedActionSetName)
            .filter(|n| !n.is_empty())
            .ok_or(XrResult::XR_ERROR_LOCALIZED_NAME_INVALID)?;
        let instance = id(instance_handle);
        for (_, set) in OBJECTS.action_sets.all() {
            if set.instance == instance && set.name == name {
                return Err(XrResult::XR_ERROR_NAME_DUPLICATED);
            }
            if set.instance == instance && set.localized == localized {
                return Err(XrResult::XR_ERROR_LOCALIZED_NAME_DUPLICATED);
            }
        }
        *out = handle(OBJECTS.action_sets.insert(ActionSet {
            instance,
            name,
            localized,
            attached: AtomicBool::new(false),
        }));
        OK
    })
}

pub unsafe extern "C" fn xr_destroy_action_set(set: XrActionSet) -> XrResult {
    ffi::call(|| {
        OBJECTS
            .action_sets
            .remove(id(set))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        OBJECTS.actions.remove_where(|a| a.set == id(set));
        OK
    })
}

pub unsafe extern "C" fn xr_create_action(
    set_handle: XrActionSet,
    info: *const XrActionCreateInfo,
    out: *mut XrAction,
) -> XrResult {
    ffi::call(|| {
        let set = OBJECTS
            .action_sets
            .get(id(set_handle))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        let instance = OBJECTS
            .instances
            .get(set.instance)
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_ACTION_CREATE_INFO)?;
        if set.attached.load(Ordering::Acquire) {
            return Err(XrResult::XR_ERROR_ACTIONSETS_ALREADY_ATTACHED);
        }
        let name = ffi::fixed_str(&info.actionName)
            .filter(|n| valid_name(n))
            .ok_or(XrResult::XR_ERROR_PATH_FORMAT_INVALID)?;
        let localized = ffi::fixed_str(&info.localizedActionName)
            .filter(|n| !n.is_empty())
            .ok_or(XrResult::XR_ERROR_LOCALIZED_NAME_INVALID)?;
        // SAFETY: counted array from the app.
        let subactions =
            unsafe { ffi::slice(info.subactionPaths, info.countSubactionPaths) }?.to_vec();
        for &p in &subactions {
            let path = instance
                .path_name(p)
                .ok_or(XrResult::XR_ERROR_PATH_INVALID)?;
            if !TOP_LEVEL.contains(&path.as_str()) {
                return Err(XrResult::XR_ERROR_PATH_UNSUPPORTED);
            }
        }
        for (_, a) in OBJECTS.actions.all() {
            if a.set == id(set_handle) && a.name == name {
                return Err(XrResult::XR_ERROR_NAME_DUPLICATED);
            }
            if a.set == id(set_handle) && a.localized == localized {
                return Err(XrResult::XR_ERROR_LOCALIZED_NAME_DUPLICATED);
            }
        }
        *out = handle(OBJECTS.actions.insert(Action {
            set: id(set_handle),
            ty: info.actionType,
            subactions,
            name,
            localized,
            bindings: Mutex::default(),
        }));
        OK
    })
}

pub unsafe extern "C" fn xr_destroy_action(action: XrAction) -> XrResult {
    ffi::call(|| {
        OBJECTS
            .actions
            .remove(id(action))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        OK
    })
}

pub unsafe extern "C" fn xr_suggest_interaction_profile_bindings(
    instance_handle: XrInstance,
    info: *const XrInteractionProfileSuggestedBinding,
) -> XrResult {
    ffi::call(|| {
        let instance = instance::lookup(instance_handle)?;
        // SAFETY: caller-provided pointer and counted array.
        let info = unsafe { ffi::read(info) }?;
        ffi::expect_type(
            info.type_,
            XrStructureType::XR_TYPE_INTERACTION_PROFILE_SUGGESTED_BINDING,
        )?;
        let profile = instance
            .path_name(info.interactionProfile)
            .ok_or(XrResult::XR_ERROR_PATH_INVALID)?;
        if !profile.starts_with("/interaction_profiles/") {
            return Err(XrResult::XR_ERROR_PATH_UNSUPPORTED);
        }
        let suggested: &[XrActionSuggestedBinding] =
            unsafe { ffi::slice(info.suggestedBindings, info.countSuggestedBindings) }?;
        let mut per_action: HashMap<u64, Vec<XrPath>> = HashMap::new();
        for s in suggested {
            let action = OBJECTS
                .actions
                .get(id(s.action))
                .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
            if OBJECTS
                .action_sets
                .get(action.set)
                .is_some_and(|set| set.attached.load(Ordering::Acquire))
            {
                return Err(XrResult::XR_ERROR_ACTIONSETS_ALREADY_ATTACHED);
            }
            instance
                .path_name(s.binding)
                .ok_or(XrResult::XR_ERROR_PATH_INVALID)?;
            per_action.entry(id(s.action)).or_default().push(s.binding);
        }
        // A later suggestion for the same profile replaces the earlier one.
        for (action_id, action) in OBJECTS.actions.all() {
            let in_instance = OBJECTS
                .action_sets
                .get(action.set)
                .is_some_and(|s| s.instance == id(instance_handle));
            if in_instance {
                let mut bindings = action.bindings.lock().unwrap_or_else(|e| e.into_inner());
                match per_action.remove(&action_id) {
                    Some(paths) => bindings.insert(info.interactionProfile, paths),
                    None => bindings.remove(&info.interactionProfile),
                };
            }
        }
        OK
    })
}

pub unsafe extern "C" fn xr_attach_session_action_sets(
    session_handle: XrSession,
    info: *const XrSessionActionSetsAttachInfo,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        let instance = OBJECTS
            .instances
            .get(s.instance)
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        // SAFETY: caller-provided pointer and counted array.
        let info = unsafe { ffi::read(info) }?;
        ffi::expect_type(
            info.type_,
            XrStructureType::XR_TYPE_SESSION_ACTION_SETS_ATTACH_INFO,
        )?;
        let sets: Vec<u64> = unsafe { ffi::slice(info.actionSets, info.countActionSets) }?
            .iter()
            .map(|&h| id(h))
            .collect();
        let mut input = s.input.lock().unwrap_or_else(|e| e.into_inner());
        if !input.attached.is_empty() {
            return Err(XrResult::XR_ERROR_ACTIONSETS_ALREADY_ATTACHED);
        }
        for &set in &sets {
            OBJECTS
                .action_sets
                .get(set)
                .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?
                .attached
                .store(true, Ordering::Release);
        }
        let suggested = |profile: XrPath| {
            OBJECTS.actions.all().iter().any(|(_, a)| {
                sets.contains(&a.set)
                    && a.bindings
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains_key(&profile)
            })
        };
        input.profile = PROFILES
            .iter()
            .filter_map(|p| instance.path(p).ok())
            .find(|&p| suggested(p));
        input.attached = sets;
        crate::log!(
            "interaction profile {}",
            input
                .profile
                .and_then(|p| instance.path_name(p))
                .unwrap_or_else(|| "none".into())
        );
        drop(input);
        instance.push_event(XrEventDataInteractionProfileChanged {
            type_: XrStructureType::XR_TYPE_EVENT_DATA_INTERACTION_PROFILE_CHANGED,
            next: std::ptr::null(),
            session: session_handle,
        });
        OK
    })
}

pub unsafe extern "C" fn xr_get_current_interaction_profile(
    session_handle: XrSession,
    top_level: XrPath,
    state: *mut XrInteractionProfileState,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        let instance = OBJECTS
            .instances
            .get(s.instance)
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        // SAFETY: caller-provided output struct.
        let state = unsafe { ffi::write(state) }?;
        ffi::expect_type(
            state.type_,
            XrStructureType::XR_TYPE_INTERACTION_PROFILE_STATE,
        )?;
        let input = s.input.lock().unwrap_or_else(|e| e.into_inner());
        if input.attached.is_empty() {
            return Err(XrResult::XR_ERROR_ACTIONSET_NOT_ATTACHED);
        }
        let path = instance
            .path_name(top_level)
            .ok_or(XrResult::XR_ERROR_PATH_INVALID)?;
        if !TOP_LEVEL.contains(&path.as_str()) {
            return Err(XrResult::XR_ERROR_PATH_UNSUPPORTED);
        }
        state.interactionProfile = if HANDS.contains(&path.as_str()) {
            input.profile.unwrap_or(0)
        } else {
            0
        };
        OK
    })
}

pub unsafe extern "C" fn xr_sync_actions(
    session_handle: XrSession,
    info: *const XrActionsSyncInfo,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointer and counted array.
        let info = unsafe { ffi::read(info) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_ACTIONS_SYNC_INFO)?;
        let active: &[XrActiveActionSet] =
            unsafe { ffi::slice(info.activeActionSets, info.countActiveActionSets) }?;
        // Without focus every action is inactive (OpenXR 1.1, 11.4 "Action Sets"; xrSyncActions
        // returns XR_SESSION_NOT_FOCUSED), so apps stop drawing controllers they cannot locate.
        let focused = s.is_focused();
        let controllers = if focused {
            let state = headset().state();
            [state.controller(0), state.controller(1)]
        } else {
            [None, None]
        };
        let mut input = s.input.lock().unwrap_or_else(|e| e.into_inner());
        for set in active {
            if !input.attached.contains(&id(set.actionSet)) {
                return Err(XrResult::XR_ERROR_ACTIONSET_NOT_ATTACHED);
            }
        }
        input.active = active.iter().map(|a| id(a.actionSet)).collect();
        input.before = std::mem::replace(&mut input.now, controllers);
        input.synced_at = now_ns();
        if !focused {
            return Ok(XrResult::XR_SESSION_NOT_FOCUSED);
        }
        OK
    })
}

/// Current and previous combined values of a non-pose action, and whether any source is active.
struct Evaluated<T> {
    now: T,
    before: T,
    active: bool,
}

fn evaluate<T: Copy + Default>(
    s: &Session,
    info: &XrActionStateGetInfo,
    expected: XrActionType,
    pick: impl Fn(&Input) -> Option<T>,
    combine: impl Fn(T, T) -> T,
) -> Result<(Evaluated<T>, XrTime), XrResult> {
    ffi::expect_type(info.type_, XrStructureType::XR_TYPE_ACTION_STATE_GET_INFO)?;
    let action = OBJECTS
        .actions
        .get(id(info.action))
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
    if action.ty != expected {
        return Err(XrResult::XR_ERROR_ACTION_TYPE_MISMATCH);
    }
    if info.subactionPath != 0 && !action.subactions.contains(&info.subactionPath) {
        return Err(XrResult::XR_ERROR_PATH_UNSUPPORTED);
    }
    let instance = OBJECTS
        .instances
        .get(s.instance)
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
    let input = s.input.lock().unwrap_or_else(|e| e.into_inner());
    if !input.attached.contains(&action.set) {
        return Err(XrResult::XR_ERROR_ACTIONSET_NOT_ATTACHED);
    }
    let mut out = Evaluated {
        now: T::default(),
        before: T::default(),
        active: false,
    };
    let Some(profile) = input.profile.filter(|_| input.active.contains(&action.set)) else {
        return Ok((out, input.synced_at));
    };
    for (hand, component) in sources(&instance, &action, profile, info.subactionPath) {
        if let Some(value) = input.now[hand]
            .as_ref()
            .and_then(|c| read(c, &component))
            .and_then(|i| pick(&i))
        {
            out.now = combine(out.now, value);
            out.active = true;
        }
        if let Some(value) = input.before[hand]
            .as_ref()
            .and_then(|c| read(c, &component))
            .and_then(|i| pick(&i))
        {
            out.before = combine(out.before, value);
        }
    }
    Ok((out, input.synced_at))
}

fn as_bool(i: &Input) -> Option<bool> {
    match *i {
        Input::Bool(b) => Some(b),
        Input::Float(f) => Some(f > 0.5),
        _ => None,
    }
}

fn as_float(i: &Input) -> Option<f32> {
    match *i {
        Input::Float(f) => Some(f),
        Input::Bool(b) => Some(b as u8 as f32),
        _ => None,
    }
}

fn as_vector(i: &Input) -> Option<[f32; 2]> {
    match *i {
        Input::Vector(v) => Some(v),
        _ => None,
    }
}

fn larger(a: f32, b: f32) -> f32 {
    if b.abs() > a.abs() { b } else { a }
}

fn longer(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    if b[0].hypot(b[1]) > a[0].hypot(a[1]) {
        b
    } else {
        a
    }
}

pub unsafe extern "C" fn xr_get_action_state_boolean(
    session_handle: XrSession,
    info: *const XrActionStateGetInfo,
    state: *mut XrActionStateBoolean,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let (info, state) = (unsafe { ffi::read(info) }?, unsafe { ffi::write(state) }?);
        ffi::expect_type(state.type_, XrStructureType::XR_TYPE_ACTION_STATE_BOOLEAN)?;
        let (e, at) = evaluate(
            &s,
            info,
            XrActionType::XR_ACTION_TYPE_BOOLEAN_INPUT,
            as_bool,
            |a, b| a || b,
        )?;
        state.currentState = e.now as u32;
        state.changedSinceLastSync = (e.active && e.now != e.before) as u32;
        state.lastChangeTime = if state.changedSinceLastSync != 0 {
            at
        } else {
            0
        };
        state.isActive = e.active as u32;
        OK
    })
}

pub unsafe extern "C" fn xr_get_action_state_float(
    session_handle: XrSession,
    info: *const XrActionStateGetInfo,
    state: *mut XrActionStateFloat,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let (info, state) = (unsafe { ffi::read(info) }?, unsafe { ffi::write(state) }?);
        ffi::expect_type(state.type_, XrStructureType::XR_TYPE_ACTION_STATE_FLOAT)?;
        let (e, at) = evaluate(
            &s,
            info,
            XrActionType::XR_ACTION_TYPE_FLOAT_INPUT,
            as_float,
            larger,
        )?;
        state.currentState = e.now;
        state.changedSinceLastSync = (e.active && e.now != e.before) as u32;
        state.lastChangeTime = if state.changedSinceLastSync != 0 {
            at
        } else {
            0
        };
        state.isActive = e.active as u32;
        OK
    })
}

pub unsafe extern "C" fn xr_get_action_state_vector2f(
    session_handle: XrSession,
    info: *const XrActionStateGetInfo,
    state: *mut XrActionStateVector2f,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let (info, state) = (unsafe { ffi::read(info) }?, unsafe { ffi::write(state) }?);
        ffi::expect_type(state.type_, XrStructureType::XR_TYPE_ACTION_STATE_VECTOR2F)?;
        let (e, at) = evaluate(
            &s,
            info,
            XrActionType::XR_ACTION_TYPE_VECTOR2F_INPUT,
            as_vector,
            longer,
        )?;
        state.currentState = XrVector2f {
            x: e.now[0],
            y: e.now[1],
        };
        state.changedSinceLastSync = (e.active && e.now != e.before) as u32;
        state.lastChangeTime = if state.changedSinceLastSync != 0 {
            at
        } else {
            0
        };
        state.isActive = e.active as u32;
        OK
    })
}

pub unsafe extern "C" fn xr_get_action_state_pose(
    session_handle: XrSession,
    info: *const XrActionStateGetInfo,
    state: *mut XrActionStatePose,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let (info, state) = (unsafe { ffi::read(info) }?, unsafe { ffi::write(state) }?);
        ffi::expect_type(state.type_, XrStructureType::XR_TYPE_ACTION_STATE_POSE)?;
        let pose = |i: &Input| matches!(i, Input::Pose).then_some(true);
        let (e, _) = evaluate(
            &s,
            info,
            XrActionType::XR_ACTION_TYPE_POSE_INPUT,
            pose,
            |a, b| a || b,
        )?;
        state.isActive = e.active as u32;
        OK
    })
}

/// Which hand and which pose an action space follows in the session's profile.
pub fn pose_binding(s: &Session, action: u64, subaction: XrPath) -> Option<(usize, PoseKind)> {
    let action = OBJECTS.actions.get(action)?;
    let instance = OBJECTS.instances.get(s.instance)?;
    let profile = s.input.lock().unwrap_or_else(|e| e.into_inner()).profile?;
    sources(&instance, &action, profile, subaction)
        .into_iter()
        .find_map(|(hand, component)| match component.as_str() {
            "/input/grip/pose" => Some((hand, PoseKind::Grip)),
            "/input/aim/pose" => Some((hand, PoseKind::Aim)),
            _ => None,
        })
}

fn haptic_hands(s: &Session, info: &XrHapticActionInfo) -> Result<Vec<usize>, XrResult> {
    ffi::expect_type(info.type_, XrStructureType::XR_TYPE_HAPTIC_ACTION_INFO)?;
    let action = OBJECTS
        .actions
        .get(id(info.action))
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
    if action.ty != XrActionType::XR_ACTION_TYPE_VIBRATION_OUTPUT {
        return Err(XrResult::XR_ERROR_ACTION_TYPE_MISMATCH);
    }
    let instance = OBJECTS
        .instances
        .get(s.instance)
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
    let input = s.input.lock().unwrap_or_else(|e| e.into_inner());
    if !input.attached.contains(&action.set) {
        return Err(XrResult::XR_ERROR_ACTIONSET_NOT_ATTACHED);
    }
    let Some(profile) = input.profile else {
        return Ok(Vec::new());
    };
    Ok(sources(&instance, &action, profile, info.subactionPath)
        .into_iter()
        .filter(|(_, c)| c == "/output/haptic")
        .map(|(hand, _)| hand)
        .collect())
}

pub unsafe extern "C" fn xr_apply_haptic_feedback(
    session_handle: XrSession,
    info: *const XrHapticActionInfo,
    feedback: *const XrHapticBaseHeader,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let (info, feedback) = (unsafe { ffi::read(info) }?, unsafe { ffi::read(feedback) }?);
        ffi::expect_type(feedback.type_, XrStructureType::XR_TYPE_HAPTIC_VIBRATION)?;
        // SAFETY: the header's type says this is an XrHapticVibration.
        let vibration =
            unsafe { &*(feedback as *const XrHapticBaseHeader as *const XrHapticVibration) };
        let duration_ns = if vibration.duration == XR_MIN_HAPTIC_DURATION as i64 {
            DEFAULT_HAPTIC_NS
        } else {
            vibration.duration.max(0)
        };
        for hand in haptic_hands(&s, info)? {
            headset().send(&Message::Haptics(Haptics {
                hand: hand as u8,
                amplitude: vibration.amplitude.clamp(0.0, 1.0),
                duration_ns,
                frequency: vibration.frequency,
            }));
        }
        OK
    })
}

pub unsafe extern "C" fn xr_stop_haptic_feedback(
    session_handle: XrSession,
    info: *const XrHapticActionInfo,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointer.
        let info = unsafe { ffi::read(info) }?;
        for hand in haptic_hands(&s, info)? {
            headset().send(&Message::Haptics(Haptics {
                hand: hand as u8,
                amplitude: 0.0,
                duration_ns: 0,
                frequency: 0.0,
            }));
        }
        OK
    })
}

pub unsafe extern "C" fn xr_enumerate_bound_sources_for_action(
    session_handle: XrSession,
    info: *const XrBoundSourcesForActionEnumerateInfo,
    capacity: u32,
    count: *mut u32,
    sources_out: *mut XrPath,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointer.
        let info = unsafe { ffi::read(info) }?;
        ffi::expect_type(
            info.type_,
            XrStructureType::XR_TYPE_BOUND_SOURCES_FOR_ACTION_ENUMERATE_INFO,
        )?;
        let action = OBJECTS
            .actions
            .get(id(info.action))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        let instance = OBJECTS
            .instances
            .get(s.instance)
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        let profile = {
            let input = s.input.lock().unwrap_or_else(|e| e.into_inner());
            if !input.attached.contains(&action.set) {
                return Err(XrResult::XR_ERROR_ACTIONSET_NOT_ATTACHED);
            }
            input.profile
        };
        let bound: Vec<XrPath> = profile
            .map(|p| sources(&instance, &action, p, 0))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(hand, c)| instance.path(&format!("{}{c}", HANDS[hand])).ok())
            .collect();
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&bound, capacity, count, sources_out, |d, p| {
                let _: () = *d = *p;
                Ok(())
            })
        })
    })
}

pub unsafe extern "C" fn xr_get_input_source_localized_name(
    session_handle: XrSession,
    info: *const XrInputSourceLocalizedNameGetInfo,
    capacity: u32,
    count: *mut u32,
    buffer: *mut c_char,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        let instance = OBJECTS
            .instances
            .get(s.instance)
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        // SAFETY: caller-provided pointer.
        let info = unsafe { ffi::read(info) }?;
        ffi::expect_type(
            info.type_,
            XrStructureType::XR_TYPE_INPUT_SOURCE_LOCALIZED_NAME_GET_INFO,
        )?;
        let name = instance
            .path_name(info.sourcePath)
            .ok_or(XrResult::XR_ERROR_PATH_INVALID)?;
        let chars: Vec<c_char> = name.bytes().chain([0]).map(|b| b as c_char).collect();
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&chars, capacity, count, buffer, |d, c| {
                let _: () = *d = *c;
                Ok(())
            })
        })
    })
}
