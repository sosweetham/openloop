use std::collections::HashSet;
use std::ffi::{CStr, c_char};
use std::sync::{LazyLock, Mutex};

use tether_openxr_abi::{
    XrInstance, XrInstanceCreateInfo, XrInstanceProperties, XrResult, XrStructureType, XrVersion,
};

use crate::generated::Requirement;
use crate::{extensions, ffi, version};

pub struct Instance {
    #[expect(dead_code, reason = "reported in runtime status (R1)")]
    pub app_name: String,
    pub api_version: XrVersion,
    /// Indices into `generated::EXTENSIONS`.
    pub extensions: Vec<usize>,
}

impl Instance {
    /// Whether a function with this requirement may be handed out for this instance.
    pub fn allows(&self, requirement: Requirement) -> bool {
        match requirement {
            Requirement::Global => true,
            Requirement::Core { minor } => version::minor(self.api_version) >= minor,
            Requirement::Extension(index) => self.extensions.contains(&index),
        }
    }
}

/// Live instance handles. A handle is the address of its boxed `Instance`.
static LIVE: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(Default::default);

pub fn is_live(handle: XrInstance) -> bool {
    LIVE.lock()
        .is_ok_and(|live| live.contains(&(handle as usize)))
}

pub fn get<'a>(handle: XrInstance) -> Option<&'a Instance> {
    // SAFETY: a live handle is a leaked `Box<Instance>` until xrDestroyInstance.
    is_live(handle).then(|| unsafe { &*(handle as *const Instance) })
}

pub unsafe extern "C" fn xr_create_instance(
    create_info: *const XrInstanceCreateInfo,
    out: *mut XrInstance,
) -> XrResult {
    ffi::guard(|| {
        // SAFETY: the caller passes a valid pointer or null; null is rejected.
        let Some(info) = (unsafe { create_info.as_ref() }) else {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        };
        if out.is_null() || info.type_ != XrStructureType::XR_TYPE_INSTANCE_CREATE_INFO {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        }
        let instance = match build(info) {
            Ok(instance) => instance,
            Err(e) => return e,
        };
        let handle = Box::into_raw(Box::new(instance)) as XrInstance;
        LIVE.lock().unwrap().insert(handle as usize);
        // SAFETY: checked non-null above.
        unsafe { *out = handle };
        XrResult::XR_SUCCESS
    })
}

fn build(info: &XrInstanceCreateInfo) -> Result<Instance, XrResult> {
    let app = &info.applicationInfo;
    if version::major(app.apiVersion) != 1 || version::minor(app.apiVersion) > 1 {
        return Err(XrResult::XR_ERROR_API_VERSION_UNSUPPORTED);
    }
    let app_name = fixed_str(&app.applicationName).ok_or(XrResult::XR_ERROR_NAME_INVALID)?;
    if app_name.is_empty() {
        return Err(XrResult::XR_ERROR_NAME_INVALID);
    }

    let names: &[*const c_char] = match info.enabledExtensionCount {
        0 => &[],
        // SAFETY: the spec requires `enabledExtensionCount` valid pointers.
        n => unsafe { std::slice::from_raw_parts(info.enabledExtensionNames, n as usize) },
    };
    let extensions = names
        .iter()
        // SAFETY: each entry is a NUL-terminated string per the spec.
        .map(|&name| extensions::find(unsafe { CStr::from_ptr(name) }))
        .collect::<Option<Vec<_>>>()
        .ok_or(XrResult::XR_ERROR_EXTENSION_NOT_PRESENT)?;

    Ok(Instance {
        app_name,
        api_version: app.apiVersion,
        extensions,
    })
}

/// A fixed-size C string field; `None` if it has no terminator.
fn fixed_str(field: &[c_char]) -> Option<String> {
    let bytes: Vec<u8> = field.iter().map(|&c| c as u8).collect();
    CStr::from_bytes_until_nul(&bytes)
        .ok()?
        .to_str()
        .ok()
        .map(str::to_owned)
}

pub unsafe extern "C" fn xr_destroy_instance(handle: XrInstance) -> XrResult {
    ffi::guard(|| {
        if !LIVE.lock().unwrap().remove(&(handle as usize)) {
            return XrResult::XR_ERROR_HANDLE_INVALID;
        }
        // SAFETY: the handle was live, so it is our leaked box, and it is now unlisted.
        drop(unsafe { Box::from_raw(handle as *mut Instance) });
        XrResult::XR_SUCCESS
    })
}

pub unsafe extern "C" fn xr_get_instance_properties(
    handle: XrInstance,
    properties: *mut XrInstanceProperties,
) -> XrResult {
    ffi::guard(|| {
        if get(handle).is_none() {
            return XrResult::XR_ERROR_HANDLE_INVALID;
        }
        // SAFETY: the caller passes a valid pointer or null; null is rejected.
        let Some(props) = (unsafe { properties.as_mut() }) else {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        };
        if props.type_ != XrStructureType::XR_TYPE_INSTANCE_PROPERTIES {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        }
        props.runtimeVersion = version::runtime();
        ffi::write_str(&mut props.runtimeName, "Tether");
        XrResult::XR_SUCCESS
    })
}
