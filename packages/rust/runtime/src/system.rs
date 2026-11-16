use tether_openxr_abi::{
    XrFormFactor, XrInstance, XrResult, XrStructureType, XrSystemGetInfo, XrSystemId,
};

use crate::{ffi, instance};

// TODO(R1): the simulated headset makes the HMD form factor available.
pub unsafe extern "C" fn xr_get_system(
    handle: XrInstance,
    get_info: *const XrSystemGetInfo,
    system_id: *mut XrSystemId,
) -> XrResult {
    ffi::guard(|| {
        if !instance::is_live(handle) {
            return XrResult::XR_ERROR_HANDLE_INVALID;
        }
        // SAFETY: the caller passes a valid pointer or null; null is rejected.
        let Some(info) = (unsafe { get_info.as_ref() }) else {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        };
        if system_id.is_null() || info.type_ != XrStructureType::XR_TYPE_SYSTEM_GET_INFO {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        }
        if info.formFactor == XrFormFactor::XR_FORM_FACTOR_HEAD_MOUNTED_DISPLAY {
            XrResult::XR_ERROR_FORM_FACTOR_UNAVAILABLE
        } else {
            XrResult::XR_ERROR_FORM_FACTOR_UNSUPPORTED
        }
    })
}
