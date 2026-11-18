//! Sessions: lifecycle state machine, frame pacing, and the LOCAL anchor that a headset
//! recenter moves.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLCommandQueue;
use openloop_openxr_abi::{
    XrCompositionLayerBaseHeader, XrEnvironmentBlendMode, XrEventDataReferenceSpaceChangePending,
    XrEventDataSessionStateChanged, XrFrameBeginInfo, XrFrameEndInfo, XrFrameState,
    XrFrameWaitInfo, XrGraphicsBindingMetalKHR, XrInstance, XrReferenceSpaceType, XrResult,
    XrSession, XrSessionBeginInfo, XrSessionCreateInfo, XrSessionState, XrStructureType,
};
use openloop_stream::pose::{DEFAULT_HEAD, Rigid};
use openloop_stream::time::now_ns;

use crate::actions::SessionInput;
use crate::ffi::{self, OK};
use crate::frame::Streamer;
use crate::handles::{handle, id};
use crate::instance::Instance;
use crate::link::SessionLink;
use crate::state::OBJECTS;
use crate::{convert, system};

pub struct Queue(pub Retained<ProtocolObject<dyn MTLCommandQueue>>);
// SAFETY: Metal command queues may be used from any thread.
unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

pub struct Session {
    pub instance: u64,
    pub queue: Queue,
    pub link: SessionLink,
    lifecycle: Mutex<Lifecycle>,
    frame: Mutex<FrameTiming>,
    anchors: Mutex<Anchors>,
    pub input: Mutex<SessionInput>,
    pub streamer: Mutex<Option<Streamer>>,
}

struct Lifecycle {
    state: XrSessionState,
    running: bool,
    /// XR_SESSION_STATE_LOSS_PENDING has been queued.
    lost: bool,
}

#[derive(Default)]
struct FrameTiming {
    next_deadline: i64,
    waited: bool,
    begun: bool,
}

struct Anchors {
    /// LOCAL's origin in tracking space: the head at capture time, yaw only.
    local: Rigid,
    captured: bool,
    recenters_seen: u64,
}

impl Session {
    pub fn is_running(&self) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
    }

    pub fn is_visible(&self) -> bool {
        let state = self
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state;
        state == XrSessionState::XR_SESSION_STATE_VISIBLE
            || state == XrSessionState::XR_SESSION_STATE_FOCUSED
    }

    pub fn is_focused(&self) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            == XrSessionState::XR_SESSION_STATE_FOCUSED
    }

    /// Moves a running session between FOCUSED and VISIBLE as OpenLoop's menu on the headset takes
    /// and returns input focus. VISIBLE is how OpenXR tells an app that a system overlay has the
    /// input: apps keep rendering but pause and hide their controllers.
    fn track_focus(&self, session: u64) {
        let focused = self.link.status().focused;
        let Some(instance) = self.instance() else {
            return;
        };
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let next = match (lifecycle.state, focused) {
            (XrSessionState::XR_SESSION_STATE_FOCUSED, false) => {
                XrSessionState::XR_SESSION_STATE_VISIBLE
            }
            (XrSessionState::XR_SESSION_STATE_VISIBLE, true) if lifecycle.running => {
                XrSessionState::XR_SESSION_STATE_FOCUSED
            }
            _ => return,
        };
        lifecycle.state = next;
        instance.push_event(XrEventDataSessionStateChanged {
            type_: XrStructureType::XR_TYPE_EVENT_DATA_SESSION_STATE_CHANGED,
            next: std::ptr::null(),
            session: handle(session),
            state: next,
            time: now_ns(),
        });
    }

    /// Queues XR_SESSION_STATE_LOSS_PENDING once the `openloopd` holding the session has gone
    /// away; true from then on. "The application should destroy the current session"
    /// (XrSessionState, XR_SESSION_STATE_LOSS_PENDING); until it does, the session paces
    /// frames and they return the XR_SESSION_LOSS_PENDING success code.
    fn track_loss(&self, session: u64) -> bool {
        if !self.link.lost() {
            return false;
        }
        let Some(instance) = self.instance() else {
            return true;
        };
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !std::mem::replace(&mut lifecycle.lost, true) {
            lifecycle.state = XrSessionState::XR_SESSION_STATE_LOSS_PENDING;
            instance.push_event(XrEventDataSessionStateChanged {
                type_: XrStructureType::XR_TYPE_EVENT_DATA_SESSION_STATE_CHANGED,
                next: std::ptr::null(),
                session: handle(session),
                state: XrSessionState::XR_SESSION_STATE_LOSS_PENDING,
                time: now_ns(),
            });
            crate::log!("openloopd went away and took session {session} with it: loss pending");
        }
        true
    }

    /// What a frame call returns: a success code either way.
    fn outcome(&self, lost: bool) -> ffi::XrOutcome {
        Ok(if lost {
            XrResult::XR_SESSION_LOSS_PENDING
        } else {
            XrResult::XR_SUCCESS
        })
    }

    pub fn local_anchor(&self) -> Rigid {
        self.anchors.lock().unwrap_or_else(|e| e.into_inner()).local
    }

    fn instance(&self) -> Option<Arc<Instance>> {
        OBJECTS.instances.get(self.instance)
    }

    fn transition(&self, session: u64, states: &[XrSessionState]) {
        let Some(instance) = self.instance() else {
            return;
        };
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        for &state in states {
            lifecycle.state = state;
            instance.push_event(XrEventDataSessionStateChanged {
                type_: XrStructureType::XR_TYPE_EVENT_DATA_SESSION_STATE_CHANGED,
                next: std::ptr::null(),
                session: handle(session),
                state,
                time: now_ns(),
            });
        }
    }

    /// Captures LOCAL when tracking first arrives and moves it on every headset recenter.
    fn track_anchors(&self, session: u64) {
        let tracked = self.link.tracked();
        if !tracked.live {
            return;
        }
        let head = tracked.head_at(now_ns());
        let recenters = self.link.status().recenters;
        let mut anchors = self.anchors.lock().unwrap_or_else(|e| e.into_inner());
        if anchors.captured && anchors.recenters_seen == recenters {
            return;
        }
        let previous = anchors.local;
        anchors.local = head.yaw_only();
        anchors.recenters_seen = recenters;
        let announce = std::mem::replace(&mut anchors.captured, true);
        let moved = previous.inverse() * anchors.local;
        drop(anchors);
        if !announce {
            return;
        }
        let Some(instance) = self.instance() else {
            return;
        };
        crate::log!("recentred");
        let mut spaces = vec![XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_LOCAL];
        if instance.api_minor() >= 1 {
            spaces.push(XrReferenceSpaceType::XR_REFERENCE_SPACE_TYPE_LOCAL_FLOOR);
        }
        for space in spaces {
            instance.push_event(XrEventDataReferenceSpaceChangePending {
                type_: XrStructureType::XR_TYPE_EVENT_DATA_REFERENCE_SPACE_CHANGE_PENDING,
                next: std::ptr::null(),
                session: handle(session),
                referenceSpaceType: space,
                changeTime: now_ns(),
                poseValid: 1,
                poseInPreviousSpace: convert::posef(moved),
            });
        }
    }
}

pub fn lookup(session: XrSession) -> Result<Arc<Session>, XrResult> {
    OBJECTS
        .sessions
        .get(id(session))
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)
}

pub fn poll_sessions(instance: u64) {
    for (session_id, session) in OBJECTS.sessions.all() {
        if session.instance == instance && !session.track_loss(session_id) {
            session.track_anchors(session_id);
            session.track_focus(session_id);
        }
    }
}

pub fn destroy_children_of_instance(instance: u64) {
    for (session, s) in OBJECTS.sessions.all() {
        if s.instance == instance {
            destroy(session);
        }
    }
    crate::actions::destroy_children_of_instance(instance);
}

fn destroy(session: u64) {
    if let Some(s) = OBJECTS.sessions.remove(session) {
        *s.streamer.lock().unwrap_or_else(|e| e.into_inner()) = None;
        s.link.destroy();
    }
    OBJECTS.spaces.remove_where(|s| s.session == session);
    OBJECTS.swapchains.remove_where(|s| s.session == session);
}

pub unsafe extern "C" fn xr_create_session(
    instance_handle: XrInstance,
    info: *const XrSessionCreateInfo,
    out: *mut XrSession,
) -> XrResult {
    ffi::call(|| {
        let instance = crate::instance::lookup(instance_handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_SESSION_CREATE_INFO)?;
        if info.systemId != system::SYSTEM_ID {
            return Err(XrResult::XR_ERROR_SYSTEM_INVALID);
        }
        if !instance
            .graphics_requirements_queried
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(XrResult::XR_ERROR_GRAPHICS_REQUIREMENTS_CALL_MISSING);
        }
        // SAFETY: `next` is the app's structure chain.
        let binding = unsafe {
            ffi::find_in_chain::<XrGraphicsBindingMetalKHR>(
                info.next,
                XrStructureType::XR_TYPE_GRAPHICS_BINDING_METAL_KHR,
            )
        }
        .ok_or(XrResult::XR_ERROR_GRAPHICS_DEVICE_INVALID)?;
        // SAFETY: the binding's queue is an id<MTLCommandQueue> the app keeps alive; we retain it.
        let queue = unsafe {
            Retained::retain((*binding).commandQueue as *mut ProtocolObject<dyn MTLCommandQueue>)
        }
        .ok_or(XrResult::XR_ERROR_GRAPHICS_DEVICE_INVALID)?;
        let link = crate::link::backend().create_session()?;

        let session = Session {
            instance: id(instance_handle),
            queue: Queue(queue),
            link,
            lifecycle: Mutex::new(Lifecycle {
                state: XrSessionState::XR_SESSION_STATE_UNKNOWN,
                running: false,
                lost: false,
            }),
            frame: Mutex::default(),
            anchors: Mutex::new(Anchors {
                local: DEFAULT_HEAD.yaw_only(),
                captured: false,
                recenters_seen: 0,
            }),
            input: Mutex::default(),
            streamer: Mutex::new(None),
        };
        let session_id = OBJECTS.sessions.insert(session);
        if let Some(s) = OBJECTS.sessions.get(session_id) {
            s.transition(
                session_id,
                &[
                    XrSessionState::XR_SESSION_STATE_IDLE,
                    XrSessionState::XR_SESSION_STATE_READY,
                ],
            );
        }
        crate::log!("Metal session created");
        *out = handle(session_id);
        OK
    })
}

pub unsafe extern "C" fn xr_destroy_session(session: XrSession) -> XrResult {
    ffi::call(|| {
        lookup(session)?;
        destroy(id(session));
        OK
    })
}

pub unsafe extern "C" fn xr_begin_session(
    session: XrSession,
    info: *const XrSessionBeginInfo,
) -> XrResult {
    ffi::call(|| {
        let s = lookup(session)?;
        // SAFETY: caller-provided pointer.
        let info = unsafe { ffi::read(info) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_SESSION_BEGIN_INFO)?;
        if info.primaryViewConfigurationType != system::STEREO {
            return Err(XrResult::XR_ERROR_VIEW_CONFIGURATION_TYPE_UNSUPPORTED);
        }
        {
            let mut lifecycle = s.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            if lifecycle.running {
                return Err(XrResult::XR_ERROR_SESSION_RUNNING);
            }
            if lifecycle.state != XrSessionState::XR_SESSION_STATE_READY {
                return Err(XrResult::XR_ERROR_SESSION_NOT_READY);
            }
            lifecycle.running = true;
        }
        s.link.begin();
        s.transition(
            id(session),
            &[
                XrSessionState::XR_SESSION_STATE_SYNCHRONIZED,
                XrSessionState::XR_SESSION_STATE_VISIBLE,
                XrSessionState::XR_SESSION_STATE_FOCUSED,
            ],
        );
        OK
    })
}

pub unsafe extern "C" fn xr_request_exit_session(session: XrSession) -> XrResult {
    ffi::call(|| {
        let s = lookup(session)?;
        if !s.is_running() {
            return Err(XrResult::XR_ERROR_SESSION_NOT_RUNNING);
        }
        s.transition(
            id(session),
            &[
                XrSessionState::XR_SESSION_STATE_VISIBLE,
                XrSessionState::XR_SESSION_STATE_SYNCHRONIZED,
                XrSessionState::XR_SESSION_STATE_STOPPING,
            ],
        );
        OK
    })
}

pub unsafe extern "C" fn xr_end_session(session: XrSession) -> XrResult {
    ffi::call(|| {
        let s = lookup(session)?;
        {
            let mut lifecycle = s.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            if !lifecycle.running {
                return Err(XrResult::XR_ERROR_SESSION_NOT_RUNNING);
            }
            if lifecycle.state != XrSessionState::XR_SESSION_STATE_STOPPING {
                return Err(XrResult::XR_ERROR_SESSION_NOT_STOPPING);
            }
            lifecycle.running = false;
        }
        *s.streamer.lock().unwrap_or_else(|e| e.into_inner()) = None;
        s.link.end();
        s.transition(
            id(session),
            &[
                XrSessionState::XR_SESSION_STATE_IDLE,
                XrSessionState::XR_SESSION_STATE_EXITING,
            ],
        );
        OK
    })
}

pub unsafe extern "C" fn xr_wait_frame(
    session: XrSession,
    info: *const XrFrameWaitInfo,
    state: *mut XrFrameState,
) -> XrResult {
    ffi::call(|| {
        let s = lookup(session)?;
        if !info.is_null() {
            // SAFETY: non-null caller pointer.
            ffi::expect_type(
                unsafe { &*info }.type_,
                XrStructureType::XR_TYPE_FRAME_WAIT_INFO,
            )?;
        }
        // SAFETY: caller-provided output struct.
        let state = unsafe { ffi::write(state) }?;
        ffi::expect_type(state.type_, XrStructureType::XR_TYPE_FRAME_STATE)?;
        if !s.is_running() {
            return Err(XrResult::XR_ERROR_SESSION_NOT_RUNNING);
        }
        let lost = s.track_loss(id(session));
        if !lost {
            s.track_anchors(id(session));
            s.track_focus(id(session));
        }

        let period = s
            .link
            .period_ns()
            .unwrap_or((1e9 / f64::from(system::refresh_rate(None))) as i64);
        let deadline = {
            let mut f = s.frame.lock().unwrap_or_else(|e| e.into_inner());
            let now = now_ns();
            // An absolute grid absorbs oversleep; far behind (a stall), restart it instead of bursting.
            if f.next_deadline == 0 || now - f.next_deadline > 2 * period {
                f.next_deadline = now + period;
            }
            let deadline = f.next_deadline;
            f.next_deadline += period;
            deadline
        };
        let wait = deadline - now_ns();
        if wait > 0 {
            std::thread::sleep(Duration::from_nanos(wait as u64));
        }
        s.frame.lock().unwrap_or_else(|e| e.into_inner()).waited = true;

        // openloopd times the app's sound by the same latency, so picture and sound stay together.
        state.predictedDisplayTime = deadline + s.link.latency_ns();
        state.predictedDisplayPeriod = period;
        state.shouldRender = s.is_visible() as u32;
        s.outcome(lost)
    })
}

pub unsafe extern "C" fn xr_begin_frame(
    session: XrSession,
    info: *const XrFrameBeginInfo,
) -> XrResult {
    ffi::call(|| {
        let s = lookup(session)?;
        if !info.is_null() {
            // SAFETY: non-null caller pointer.
            ffi::expect_type(
                unsafe { &*info }.type_,
                XrStructureType::XR_TYPE_FRAME_BEGIN_INFO,
            )?;
        }
        if !s.is_running() {
            return Err(XrResult::XR_ERROR_SESSION_NOT_RUNNING);
        }
        let mut f = s.frame.lock().unwrap_or_else(|e| e.into_inner());
        if !f.waited {
            return Err(XrResult::XR_ERROR_CALL_ORDER_INVALID);
        }
        f.waited = false;
        if f.begun {
            return Ok(XrResult::XR_FRAME_DISCARDED);
        }
        f.begun = true;
        drop(f);
        s.outcome(s.link.lost())
    })
}

pub unsafe extern "C" fn xr_end_frame(session: XrSession, info: *const XrFrameEndInfo) -> XrResult {
    ffi::call(|| {
        let s = lookup(session)?;
        // SAFETY: caller-provided pointer.
        let info = unsafe { ffi::read(info) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_FRAME_END_INFO)?;
        if !s.is_running() {
            return Err(XrResult::XR_ERROR_SESSION_NOT_RUNNING);
        }
        if !std::mem::take(&mut s.frame.lock().unwrap_or_else(|e| e.into_inner()).begun) {
            return Err(XrResult::XR_ERROR_CALL_ORDER_INVALID);
        }
        if info.displayTime <= 0 {
            return Err(XrResult::XR_ERROR_TIME_INVALID);
        }
        if info.environmentBlendMode != XrEnvironmentBlendMode::XR_ENVIRONMENT_BLEND_MODE_OPAQUE {
            return Err(XrResult::XR_ERROR_ENVIRONMENT_BLEND_MODE_UNSUPPORTED);
        }
        // SAFETY: the spec requires `layerCount` valid layer pointers.
        let layers: &[*const XrCompositionLayerBaseHeader] =
            unsafe { ffi::slice(info.layers, info.layerCount) }?;
        if s.link.lost() {
            return s.outcome(true);
        }
        crate::frame::submit(&s, layers, info.displayTime)?;
        OK
    })
}
