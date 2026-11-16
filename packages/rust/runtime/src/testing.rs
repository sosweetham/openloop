use std::ffi::{CStr, c_char};

use tether_openxr_abi::{
    XR_API_VERSION_1_1, XrApplicationInfo, XrInstance, XrInstanceCreateInfo, XrResult,
    XrStructureType,
};

use crate::{ffi, instance};

/// Creates an instance through the real entry point. Leaked: tests share the process.
pub fn create_instance(extensions: &[&CStr]) -> XrInstance {
    let mut app = XrApplicationInfo {
        applicationName: [0; 128],
        applicationVersion: 1,
        engineName: [0; 128],
        engineVersion: 0,
        apiVersion: XR_API_VERSION_1_1,
    };
    ffi::write_str(&mut app.applicationName, "tether-test");
    let names: Vec<*const c_char> = extensions.iter().map(|e| e.as_ptr()).collect();
    let info = XrInstanceCreateInfo {
        type_: XrStructureType::XR_TYPE_INSTANCE_CREATE_INFO,
        next: std::ptr::null(),
        createFlags: 0,
        applicationInfo: app,
        enabledApiLayerCount: 0,
        enabledApiLayerNames: std::ptr::null(),
        enabledExtensionCount: names.len() as u32,
        enabledExtensionNames: names.as_ptr(),
    };
    let mut handle = std::ptr::null_mut();
    let r = unsafe { instance::xr_create_instance(&info, &mut handle) };
    assert_eq!(r, XrResult::XR_SUCCESS);
    handle
}
