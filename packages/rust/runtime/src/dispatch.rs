use std::ffi::{CStr, c_char};

use tether_openxr_abi::{PFN_xrVoidFunction, XrInstance, XrResult};

use crate::generated::{Entries, Requirement, lookup};
use crate::{extensions, ffi, instance, strings, system};

/// Every entry point the runtime hands out. Fields are typed by the spec's PFN aliases, so an
/// implementation with a drifted signature fails to compile; anything not set here is a
/// generated stub returning `XR_ERROR_FUNCTION_UNSUPPORTED`.
static ENTRIES: Entries = {
    let mut e = Entries::STUBS;
    e.xrGetInstanceProcAddr = Some(xr_get_instance_proc_addr);
    e.xrEnumerateInstanceExtensionProperties =
        Some(extensions::xr_enumerate_instance_extension_properties);
    e.xrCreateInstance = Some(instance::xr_create_instance);
    e.xrDestroyInstance = Some(instance::xr_destroy_instance);
    e.xrGetInstanceProperties = Some(instance::xr_get_instance_properties);
    e.xrResultToString = Some(strings::xr_result_to_string);
    e.xrStructureTypeToString = Some(strings::xr_structure_type_to_string);
    e.xrGetSystem = Some(system::xr_get_system);
    e
};

pub unsafe extern "C" fn xr_get_instance_proc_addr(
    handle: XrInstance,
    name: *const c_char,
    function: *mut PFN_xrVoidFunction,
) -> XrResult {
    ffi::guard(|| {
        if name.is_null() || function.is_null() {
            return XrResult::XR_ERROR_VALIDATION_FAILURE;
        }
        // SAFETY: `name` is a NUL-terminated string per the spec.
        let name = unsafe { CStr::from_ptr(name) }.to_bytes();
        let (pfn, result) = match resolve(handle, name) {
            Ok(pfn) => (pfn, XrResult::XR_SUCCESS),
            Err(e) => (None, e),
        };
        // SAFETY: checked non-null above.
        unsafe { *function = pfn };
        result
    })
}

fn resolve(handle: XrInstance, name: &[u8]) -> Result<PFN_xrVoidFunction, XrResult> {
    let found = lookup(&ENTRIES, name);
    if handle.is_null() {
        // VUID-xrGetInstanceProcAddr-instance-parameter: only global functions without an instance.
        return match found {
            Some((pfn, Requirement::Global)) => Ok(pfn),
            Some(_) => Err(XrResult::XR_ERROR_HANDLE_INVALID),
            None => Err(XrResult::XR_ERROR_FUNCTION_UNSUPPORTED),
        };
    }
    let instance = instance::get(handle).ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
    match found {
        Some((pfn, requirement)) if instance.allows(requirement) => Ok(pfn),
        _ => Err(XrResult::XR_ERROR_FUNCTION_UNSUPPORTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::create_instance;
    use tether_openxr_abi::XR_KHR_METAL_ENABLE_EXTENSION_NAME;

    #[test]
    fn global_functions_resolve_without_an_instance() {
        assert!(
            resolve(std::ptr::null_mut(), b"xrCreateInstance")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            resolve(std::ptr::null_mut(), b"xrGetSystem"),
            Err(XrResult::XR_ERROR_HANDLE_INVALID)
        );
        assert_eq!(
            resolve(std::ptr::null_mut(), b"xrNotAFunction"),
            Err(XrResult::XR_ERROR_FUNCTION_UNSUPPORTED)
        );
    }

    #[test]
    fn every_core_function_resolves_for_an_instance() {
        let instance = create_instance(&[]);
        for name in [
            b"xrCreateSession".as_slice(),
            b"xrWaitFrame",
            b"xrLocateSpaces",
        ] {
            assert!(
                resolve(instance, name).unwrap().is_some(),
                "{}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn extension_functions_need_the_extension_enabled() {
        let name = b"xrGetMetalGraphicsRequirementsKHR";
        assert_eq!(
            resolve(create_instance(&[]), name),
            Err(XrResult::XR_ERROR_FUNCTION_UNSUPPORTED)
        );
        let metal = CStr::from_bytes_with_nul(XR_KHR_METAL_ENABLE_EXTENSION_NAME).unwrap();
        assert!(resolve(create_instance(&[metal]), name).unwrap().is_some());
    }
}
