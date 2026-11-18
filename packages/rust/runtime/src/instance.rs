use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use openloop_openxr_abi::{
    XrEventDataBuffer, XrInstance, XrInstanceCreateInfo, XrInstanceProperties, XrPath, XrResult,
    XrStructureType, XrVersion,
};

use crate::ffi::{self, OK};
use crate::generated::Requirement;
use crate::handles::{handle, id};
use crate::state::OBJECTS;
use crate::{extensions, version};

pub struct Instance {
    pub app_name: String,
    pub api_version: XrVersion,
    /// Indices into `generated::EXTENSIONS`.
    pub extensions: Vec<usize>,
    pub graphics_requirements_queried: AtomicBool,
    events: Mutex<VecDeque<QueuedEvent>>,
    paths: Mutex<Paths>,
}

struct QueuedEvent(XrEventDataBuffer);
// SAFETY: queued events are plain data; `push_event` only stores structs whose `next` is null.
unsafe impl Send for QueuedEvent {}

#[derive(Default)]
struct Paths {
    by_name: HashMap<String, XrPath>,
    names: Vec<String>,
}

impl Instance {
    /// Whether a function with this requirement may be handed out for this instance.
    pub fn allows(&self, requirement: Requirement) -> bool {
        match requirement {
            Requirement::Global => true,
            Requirement::Core { minor } => self.api_minor() >= minor,
            Requirement::Extension(index) => self.extensions.contains(&index),
        }
    }

    pub fn api_minor(&self) -> u64 {
        version::minor(self.api_version)
    }

    /// Queues an event struct for `xrPollEvent`.
    pub fn push_event<T>(&self, event: T) {
        const { assert!(size_of::<T>() <= size_of::<XrEventDataBuffer>()) };
        // SAFETY: XrEventDataBuffer is plain data; any event struct fits and starts with type/next.
        let mut buffer: XrEventDataBuffer = unsafe { std::mem::zeroed() };
        unsafe { std::ptr::write(&mut buffer as *mut _ as *mut T, event) };
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(QueuedEvent(buffer));
    }

    pub fn path(&self, name: &str) -> Result<XrPath, XrResult> {
        if !valid_path(name) {
            return Err(XrResult::XR_ERROR_PATH_FORMAT_INVALID);
        }
        let mut paths = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&p) = paths.by_name.get(name) {
            return Ok(p);
        }
        paths.names.push(name.to_owned());
        let p = paths.names.len() as XrPath;
        paths.by_name.insert(name.to_owned(), p);
        Ok(p)
    }

    pub fn path_name(&self, path: XrPath) -> Option<String> {
        let paths = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        path.checked_sub(1)
            .and_then(|i| paths.names.get(i as usize))
            .cloned()
    }
}

/// OpenXR path syntax (spec §6.1): `/`-separated non-empty segments of `[a-z0-9-_.]`.
fn valid_path(name: &str) -> bool {
    name.len() > 1
        && name.starts_with('/')
        && !name.ends_with('/')
        && name[1..].split('/').all(|s| {
            !s.is_empty()
                && !s.chars().all(|c| c == '.')
                && s.chars()
                    .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_' | '.'))
        })
}

pub fn get(handle: XrInstance) -> Option<std::sync::Arc<Instance>> {
    OBJECTS.instances.get(id(handle))
}

pub fn lookup(handle: XrInstance) -> Result<std::sync::Arc<Instance>, XrResult> {
    get(handle).ok_or(XrResult::XR_ERROR_HANDLE_INVALID)
}

pub unsafe extern "C" fn xr_create_instance(
    create_info: *const XrInstanceCreateInfo,
    out: *mut XrInstance,
) -> XrResult {
    ffi::call(|| {
        // SAFETY: caller-provided pointers per the spec.
        let info = unsafe { ffi::read(create_info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_INSTANCE_CREATE_INFO)?;
        let instance = build(info)?;
        crate::log!(
            "instance for '{}' (OpenXR 1.{})",
            instance.app_name,
            instance.api_minor()
        );
        let engine = ffi::fixed_str(&info.applicationInfo.engineName).unwrap_or_default();
        crate::link::connect(&instance.app_name, &engine, instance.api_version)?;
        *out = handle(OBJECTS.instances.insert(instance));
        OK
    })
}

fn build(info: &XrInstanceCreateInfo) -> Result<Instance, XrResult> {
    let app = &info.applicationInfo;
    if version::major(app.apiVersion) != 1 || version::minor(app.apiVersion) > 1 {
        return Err(XrResult::XR_ERROR_API_VERSION_UNSUPPORTED);
    }
    let app_name = ffi::fixed_str(&app.applicationName)
        .filter(|n| !n.is_empty())
        .ok_or(XrResult::XR_ERROR_NAME_INVALID)?;
    // SAFETY: the spec requires `enabledExtensionCount` valid NUL-terminated strings.
    let names: &[*const c_char] =
        unsafe { ffi::slice(info.enabledExtensionNames, info.enabledExtensionCount) }?;
    let extensions = names
        .iter()
        .map(|&name| extensions::find(unsafe { CStr::from_ptr(name) }))
        .collect::<Option<Vec<_>>>()
        .ok_or(XrResult::XR_ERROR_EXTENSION_NOT_PRESENT)?;
    Ok(Instance {
        app_name,
        api_version: app.apiVersion,
        extensions,
        graphics_requirements_queried: AtomicBool::new(false),
        events: Mutex::default(),
        paths: Mutex::default(),
    })
}

pub unsafe extern "C" fn xr_destroy_instance(handle: XrInstance) -> XrResult {
    ffi::call(|| {
        let instance = id(handle);
        OBJECTS
            .instances
            .remove(instance)
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        crate::session::destroy_children_of_instance(instance);
        OK
    })
}

pub unsafe extern "C" fn xr_get_instance_properties(
    handle: XrInstance,
    properties: *mut XrInstanceProperties,
) -> XrResult {
    ffi::call(|| {
        lookup(handle)?;
        // SAFETY: caller-provided output struct.
        let props = unsafe { ffi::write(properties) }?;
        ffi::expect_type(props.type_, XrStructureType::XR_TYPE_INSTANCE_PROPERTIES)?;
        props.runtimeVersion = version::runtime();
        ffi::write_str(&mut props.runtimeName, "OpenLoop");
        OK
    })
}

pub unsafe extern "C" fn xr_poll_event(
    handle: XrInstance,
    buffer: *mut XrEventDataBuffer,
) -> XrResult {
    ffi::call(|| {
        let instance = lookup(handle)?;
        // SAFETY: caller-provided output struct.
        let out = unsafe { ffi::write(buffer) }?;
        ffi::expect_type(out.type_, XrStructureType::XR_TYPE_EVENT_DATA_BUFFER)?;
        crate::session::poll_sessions(id(handle));
        match instance
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            Some(QueuedEvent(event)) => {
                *out = event;
                OK
            }
            None => Ok(XrResult::XR_EVENT_UNAVAILABLE),
        }
    })
}

pub unsafe extern "C" fn xr_string_to_path(
    handle: XrInstance,
    name: *const c_char,
    path: *mut XrPath,
) -> XrResult {
    ffi::call(|| {
        let instance = lookup(handle)?;
        if name.is_null() {
            return Err(XrResult::XR_ERROR_VALIDATION_FAILURE);
        }
        // SAFETY: a NUL-terminated string per the spec.
        let name = unsafe { CStr::from_ptr(name) }
            .to_str()
            .map_err(|_| XrResult::XR_ERROR_PATH_FORMAT_INVALID)?;
        *unsafe { ffi::write(path) }? = instance.path(name)?;
        OK
    })
}

pub unsafe extern "C" fn xr_path_to_string(
    handle: XrInstance,
    path: XrPath,
    capacity: u32,
    count: *mut u32,
    buffer: *mut c_char,
) -> XrResult {
    ffi::call(|| {
        let instance = lookup(handle)?;
        let name = instance
            .path_name(path)
            .ok_or(XrResult::XR_ERROR_PATH_INVALID)?;
        let mut bytes = name.into_bytes();
        bytes.push(0);
        let chars: Vec<c_char> = bytes.iter().map(|&b| b as c_char).collect();
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&chars, capacity, count, buffer, |d, s| {
                let _: () = *d = *s;
                Ok(())
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::valid_path;

    #[test]
    fn path_syntax() {
        assert!(valid_path("/user/hand/left"));
        assert!(valid_path("/interaction_profiles/oculus/touch_controller"));
        assert!(!valid_path("/user/hand/left/"));
        assert!(!valid_path("user/hand"));
        assert!(!valid_path("/User/hand"));
        assert!(!valid_path("/user//hand"));
        assert!(!valid_path("/"));
    }
}
