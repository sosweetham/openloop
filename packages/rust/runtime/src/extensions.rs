use std::ffi::{CStr, c_char};

use tether_openxr_abi::{
    XR_KHR_METAL_ENABLE_EXTENSION_NAME, XR_KHR_metal_enable_SPEC_VERSION, XrExtensionProperties,
    XrResult, XrStructureType,
};

use crate::ffi;

pub struct Extension {
    pub name: &'static CStr,
    pub spec_version: u32,
}

// TODO(R1): XR_KHR_metal_enable is advertised so apps can create an instance;
// its functions (graphics requirements, Metal swapchains) land with the
// simulated headset.
pub static SUPPORTED: &[Extension] = &[Extension {
    name: c_str(XR_KHR_METAL_ENABLE_EXTENSION_NAME),
    spec_version: XR_KHR_metal_enable_SPEC_VERSION,
}];

const fn c_str(bytes: &'static [u8]) -> &'static CStr {
    match CStr::from_bytes_with_nul(bytes) {
        Ok(s) => s,
        Err(_) => panic!("extension name constants are NUL-terminated"),
    }
}

pub fn find(name: &CStr) -> Option<&'static Extension> {
    SUPPORTED.iter().find(|e| e.name == name)
}

pub unsafe extern "C" fn xr_enumerate_instance_extension_properties(
    layer_name: *const c_char,
    capacity: u32,
    count_output: *mut u32,
    properties: *mut XrExtensionProperties,
) -> XrResult {
    ffi::guard(|| {
        if !layer_name.is_null() {
            return XrResult::XR_ERROR_API_LAYER_NOT_PRESENT;
        }
        // SAFETY: forwarded straight from the caller under the same contract.
        unsafe {
            ffi::two_call(SUPPORTED, capacity, count_output, properties, |dst, ext| {
                if dst.type_ != XrStructureType::XR_TYPE_EXTENSION_PROPERTIES {
                    return Err(XrResult::XR_ERROR_VALIDATION_FAILURE);
                }
                ffi::write_str(
                    &mut dst.extensionName,
                    ext.name.to_str().unwrap_or_default(),
                );
                dst.extensionVersion = ext.spec_version;
                Ok(())
            })
        }
    })
}
