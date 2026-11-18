//! The one system OpenLoop exposes: a head-mounted display shaped by the connected headset's
//! device profile, or by Quest 3 defaults until one connects.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use objc2::rc::Retained;
use objc2_metal::MTLCreateSystemDefaultDevice;
use openloop_openxr_abi::{
    XrEnvironmentBlendMode, XrFormFactor, XrGraphicsRequirementsMetalKHR, XrInstance, XrResult,
    XrStructureType, XrSystemGetInfo, XrSystemId, XrSystemProperties,
    XrViewConfigurationProperties, XrViewConfigurationType, XrViewConfigurationView,
};
use openloop_protocol::DeviceProfile;
use openloop_stream::video;

use crate::ffi::{self, OK};
use crate::instance::lookup;
use crate::link::backend;

pub const SYSTEM_ID: XrSystemId = 1;
pub const STEREO: XrViewConfigurationType =
    XrViewConfigurationType::XR_VIEW_CONFIGURATION_TYPE_PRIMARY_STEREO;
const DEFAULT_EYE: (u32, u32) = (1680, 1760);
const DEFAULT_REFRESH: f32 = 90.0;
const MAX_EYE: u32 = 4096;

/// The size apps are asked to render each eye at: the headset's own recommendation scaled by
/// the render scale, which the compositor filters back down to what is encoded.
fn eye_size(profile: Option<&DeviceProfile>) -> (u32, u32) {
    let recommended = profile.map_or(DEFAULT_EYE, |p| (p.eye_width, p.eye_height));
    let (w, h) = video::render_eye(recommended, render_scale());
    (w.min(MAX_EYE), h.min(MAX_EYE))
}

fn render_scale() -> f32 {
    static SCALE: OnceLock<f32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        let scale = video::render_scale(std::env::var_os(video::RENDER_SCALE_VAR).as_deref());
        crate::log!("render scale {scale}");
        scale
    })
}

/// The headset's refresh rate, or the default before one connects; never below 1 Hz.
pub fn refresh_rate(rate: Option<f32>) -> f32 {
    rate.filter(|r| r.is_finite())
        .unwrap_or(DEFAULT_REFRESH)
        .max(1.0)
}

fn check_system(system: XrSystemId) -> Result<(), XrResult> {
    if system == SYSTEM_ID {
        Ok(())
    } else {
        Err(XrResult::XR_ERROR_SYSTEM_INVALID)
    }
}

fn check_view_configuration(ty: XrViewConfigurationType) -> Result<(), XrResult> {
    if ty == STEREO {
        Ok(())
    } else {
        Err(XrResult::XR_ERROR_VIEW_CONFIGURATION_TYPE_UNSUPPORTED)
    }
}

pub unsafe extern "C" fn xr_get_system(
    handle: XrInstance,
    info: *const XrSystemGetInfo,
    out: *mut XrSystemId,
) -> XrResult {
    ffi::call(|| {
        lookup(handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_SYSTEM_GET_INFO)?;
        if info.formFactor != XrFormFactor::XR_FORM_FACTOR_HEAD_MOUNTED_DISPLAY {
            return Err(XrResult::XR_ERROR_FORM_FACTOR_UNSUPPORTED);
        }
        *out = SYSTEM_ID;
        OK
    })
}

pub unsafe extern "C" fn xr_get_system_properties(
    handle: XrInstance,
    system: XrSystemId,
    properties: *mut XrSystemProperties,
) -> XrResult {
    ffi::call(|| {
        lookup(handle)?;
        check_system(system)?;
        // SAFETY: caller-provided output struct.
        let props = unsafe { ffi::write(properties) }?;
        ffi::expect_type(props.type_, XrStructureType::XR_TYPE_SYSTEM_PROPERTIES)?;
        let name = backend().profile().map(|p| format!("OpenLoop: {}", p.name));
        props.systemId = SYSTEM_ID;
        props.vendorId = 0;
        ffi::write_str(&mut props.systemName, name.as_deref().unwrap_or("OpenLoop"));
        props.graphicsProperties.maxSwapchainImageWidth = MAX_EYE;
        props.graphicsProperties.maxSwapchainImageHeight = MAX_EYE;
        props.graphicsProperties.maxLayerCount = 16;
        props.trackingProperties.orientationTracking = 1;
        props.trackingProperties.positionTracking = 1;
        OK
    })
}

pub unsafe extern "C" fn xr_enumerate_view_configurations(
    handle: XrInstance,
    system: XrSystemId,
    capacity: u32,
    count: *mut u32,
    types: *mut XrViewConfigurationType,
) -> XrResult {
    ffi::call(|| {
        lookup(handle)?;
        check_system(system)?;
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&[STEREO], capacity, count, types, |d, s| {
                let _: () = *d = *s;
                Ok(())
            })
        })
    })
}

pub unsafe extern "C" fn xr_get_view_configuration_properties(
    handle: XrInstance,
    system: XrSystemId,
    ty: XrViewConfigurationType,
    properties: *mut XrViewConfigurationProperties,
) -> XrResult {
    ffi::call(|| {
        lookup(handle)?;
        check_system(system)?;
        check_view_configuration(ty)?;
        // SAFETY: caller-provided output struct.
        let props = unsafe { ffi::write(properties) }?;
        ffi::expect_type(
            props.type_,
            XrStructureType::XR_TYPE_VIEW_CONFIGURATION_PROPERTIES,
        )?;
        props.viewConfigurationType = ty;
        props.fovMutable = 1;
        OK
    })
}

pub unsafe extern "C" fn xr_enumerate_view_configuration_views(
    handle: XrInstance,
    system: XrSystemId,
    ty: XrViewConfigurationType,
    capacity: u32,
    count: *mut u32,
    views: *mut XrViewConfigurationView,
) -> XrResult {
    ffi::call(|| {
        lookup(handle)?;
        check_system(system)?;
        check_view_configuration(ty)?;
        let (w, h) = eye_size(backend().profile().as_ref());
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&[(); 2], capacity, count, views, |v, _| {
                ffi::expect_type(v.type_, XrStructureType::XR_TYPE_VIEW_CONFIGURATION_VIEW)?;
                v.recommendedImageRectWidth = w;
                v.recommendedImageRectHeight = h;
                v.maxImageRectWidth = MAX_EYE;
                v.maxImageRectHeight = MAX_EYE;
                v.recommendedSwapchainSampleCount = 1;
                v.maxSwapchainSampleCount = 1;
                Ok(())
            })
        })
    })
}

pub unsafe extern "C" fn xr_enumerate_environment_blend_modes(
    handle: XrInstance,
    system: XrSystemId,
    ty: XrViewConfigurationType,
    capacity: u32,
    count: *mut u32,
    modes: *mut XrEnvironmentBlendMode,
) -> XrResult {
    ffi::call(|| {
        lookup(handle)?;
        check_system(system)?;
        check_view_configuration(ty)?;
        let opaque = [XrEnvironmentBlendMode::XR_ENVIRONMENT_BLEND_MODE_OPAQUE];
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&opaque, capacity, count, modes, |d, s| {
                let _: () = *d = *s;
                Ok(())
            })
        })
    })
}

/// The device sessions must use: the system default GPU, retained for the process lifetime.
pub fn metal_device() -> Option<*mut c_void> {
    static DEVICE: OnceLock<Option<usize>> = OnceLock::new();
    DEVICE
        .get_or_init(|| {
            MTLCreateSystemDefaultDevice().map(|d| Retained::into_raw(d) as *mut c_void as usize)
        })
        .map(|p| p as *mut c_void)
}

pub unsafe extern "C" fn xr_get_metal_graphics_requirements(
    handle: XrInstance,
    system: XrSystemId,
    requirements: *mut XrGraphicsRequirementsMetalKHR,
) -> XrResult {
    ffi::call(|| {
        let instance = lookup(handle)?;
        check_system(system)?;
        // SAFETY: caller-provided output struct.
        let req = unsafe { ffi::write(requirements) }?;
        ffi::expect_type(
            req.type_,
            XrStructureType::XR_TYPE_GRAPHICS_REQUIREMENTS_METAL_KHR,
        )?;
        req.metalDevice = metal_device().ok_or(XrResult::XR_ERROR_RUNTIME_FAILURE)?;
        instance
            .graphics_requirements_queried
            .store(true, Ordering::Release);
        OK
    })
}
