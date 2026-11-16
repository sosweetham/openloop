use std::ffi::{CStr, c_char};

use tether_openxr_abi::{
    PFN_xrCreateInstance, PFN_xrDestroyInstance, PFN_xrEnumerateInstanceExtensionProperties,
    PFN_xrGetInstanceProcAddr, PFN_xrGetInstanceProperties, PFN_xrGetSystem, PFN_xrVoidFunction,
    XrInstance, XrResult,
};

use crate::{extensions, ffi, instance, system};

// The PFN types are the spec's signatures; assigning to them makes a drifted
// signature a compile error rather than a crash in the host app.
const GET_INSTANCE_PROC_ADDR: PFN_xrGetInstanceProcAddr = Some(xr_get_instance_proc_addr);
const ENUMERATE_INSTANCE_EXTENSION_PROPERTIES: PFN_xrEnumerateInstanceExtensionProperties =
    Some(extensions::xr_enumerate_instance_extension_properties);
const CREATE_INSTANCE: PFN_xrCreateInstance = Some(instance::xr_create_instance);
const DESTROY_INSTANCE: PFN_xrDestroyInstance = Some(instance::xr_destroy_instance);
const GET_INSTANCE_PROPERTIES: PFN_xrGetInstanceProperties =
    Some(instance::xr_get_instance_properties);
const GET_SYSTEM: PFN_xrGetSystem = Some(system::xr_get_system);

macro_rules! void_fn {
    ($pfn:expr) => {
        // SAFETY: function pointers share one representation; the loader casts
        // back to the PFN type matching the name it asked for.
        $pfn.map(|f| unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn()>(f as *const ())
        })
    };
}

/// Functions callable before an instance exists (spec §3.2,
/// VUID-xrGetInstanceProcAddr-instance-parameter).
fn global(name: &[u8]) -> PFN_xrVoidFunction {
    match name {
        b"xrEnumerateInstanceExtensionProperties" => {
            void_fn!(ENUMERATE_INSTANCE_EXTENSION_PROPERTIES)
        }
        b"xrCreateInstance" => void_fn!(CREATE_INSTANCE),
        _ => None,
    }
}

fn per_instance(name: &[u8]) -> PFN_xrVoidFunction {
    match name {
        b"xrGetInstanceProcAddr" => void_fn!(GET_INSTANCE_PROC_ADDR),
        b"xrDestroyInstance" => void_fn!(DESTROY_INSTANCE),
        b"xrGetInstanceProperties" => void_fn!(GET_INSTANCE_PROPERTIES),
        b"xrGetSystem" => void_fn!(GET_SYSTEM),
        _ => global(name),
    }
}

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
        let found = if handle.is_null() {
            global(name)
        } else if instance::is_live(handle) {
            per_instance(name)
        } else {
            // SAFETY: checked non-null above.
            unsafe { *function = None };
            return XrResult::XR_ERROR_HANDLE_INVALID;
        };
        // SAFETY: checked non-null above.
        unsafe { *function = found };
        if found.is_some() {
            XrResult::XR_SUCCESS
        } else {
            XrResult::XR_ERROR_FUNCTION_UNSUPPORTED
        }
    })
}
