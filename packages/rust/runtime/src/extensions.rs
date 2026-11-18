use std::ffi::{CStr, c_char};

use openloop_openxr_abi::{XrExtensionProperties, XrResult, XrStructureType};

use crate::ffi;
use crate::generated::EXTENSIONS;

/// Index into `EXTENSIONS`, which is generated from `extensions.txt`.
pub fn find(name: &CStr) -> Option<usize> {
    EXTENSIONS.iter().position(|e| e.name == name)
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
            ffi::two_call(
                EXTENSIONS,
                capacity,
                count_output,
                properties,
                |dst, ext| {
                    if dst.type_ != XrStructureType::XR_TYPE_EXTENSION_PROPERTIES {
                        return Err(XrResult::XR_ERROR_VALIDATION_FAILURE);
                    }
                    ffi::write_str(
                        &mut dst.extensionName,
                        ext.name.to_str().unwrap_or_default(),
                    );
                    dst.extensionVersion = ext.spec_version;
                    Ok(())
                },
            )
        }
    })
}
