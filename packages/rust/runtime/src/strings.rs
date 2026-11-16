use std::ffi::c_char;

use tether_openxr_abi::{
    XR_MAX_RESULT_STRING_SIZE, XR_MAX_STRUCTURE_NAME_SIZE, XrInstance, XrResult, XrStructureType,
};

use crate::generated::{result_name, structure_type_name};
use crate::{ffi, instance};

pub unsafe extern "C" fn xr_result_to_string(
    handle: XrInstance,
    value: XrResult,
    buffer: *mut c_char,
) -> XrResult {
    let name = || match result_name(value) {
        Some(name) => name.to_string_lossy().into_owned(),
        None if value.0 >= 0 => format!("XR_UNKNOWN_SUCCESS_{}", value.0),
        None => format!("XR_UNKNOWN_FAILURE_{}", value.0),
    };
    // SAFETY: forwarded from the caller, who owns a buffer of the spec's size.
    unsafe { write_name(handle, buffer, XR_MAX_RESULT_STRING_SIZE, name) }
}

pub unsafe extern "C" fn xr_structure_type_to_string(
    handle: XrInstance,
    value: XrStructureType,
    buffer: *mut c_char,
) -> XrResult {
    let name = || match structure_type_name(value) {
        Some(name) => name.to_string_lossy().into_owned(),
        None => format!("XR_UNKNOWN_STRUCTURE_TYPE_{}", value.0),
    };
    // SAFETY: forwarded from the caller, who owns a buffer of the spec's size.
    unsafe { write_name(handle, buffer, XR_MAX_STRUCTURE_NAME_SIZE, name) }
}

/// # Safety
/// `buffer` must be null or valid for `size` writes.
unsafe fn write_name(
    handle: XrInstance,
    buffer: *mut c_char,
    size: u32,
    name: impl FnOnce() -> String,
) -> XrResult {
    ffi::guard(|| {
        if instance::get(handle).is_none() {
            return XrResult::XR_ERROR_HANDLE_INVALID;
        }
        if buffer.is_null() {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        }
        // SAFETY: the caller guarantees `size` writable bytes.
        let out = unsafe { std::slice::from_raw_parts_mut(buffer, size as usize) };
        ffi::write_str(out, &name());
        XrResult::XR_SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::create_instance;
    use std::ffi::CStr;

    fn to_string(value: XrResult) -> String {
        let mut buf = [0 as c_char; XR_MAX_RESULT_STRING_SIZE as usize];
        let r = unsafe { xr_result_to_string(create_instance(&[]), value, buf.as_mut_ptr()) };
        assert_eq!(r, XrResult::XR_SUCCESS);
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn names_known_and_unknown_results() {
        assert_eq!(to_string(XrResult::XR_SUCCESS), "XR_SUCCESS");
        assert_eq!(
            to_string(XrResult::XR_ERROR_HANDLE_INVALID),
            "XR_ERROR_HANDLE_INVALID"
        );
        assert_eq!(to_string(XrResult(-424242)), "XR_UNKNOWN_FAILURE_-424242");
        assert_eq!(to_string(XrResult(424242)), "XR_UNKNOWN_SUCCESS_424242");
    }
}
