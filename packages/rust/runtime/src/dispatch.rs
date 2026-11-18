use std::ffi::{CStr, c_char};

use openloop_openxr_abi::{PFN_xrVoidFunction, XrInstance, XrResult};

use crate::generated::{Entries, Requirement, lookup};
use crate::{actions, extensions, ffi, instance, session, space, strings, swapchain, system};

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
    e.xrPollEvent = Some(instance::xr_poll_event);
    e.xrStringToPath = Some(instance::xr_string_to_path);
    e.xrPathToString = Some(instance::xr_path_to_string);
    e.xrResultToString = Some(strings::xr_result_to_string);
    e.xrStructureTypeToString = Some(strings::xr_structure_type_to_string);

    e.xrGetSystem = Some(system::xr_get_system);
    e.xrGetSystemProperties = Some(system::xr_get_system_properties);
    e.xrEnumerateViewConfigurations = Some(system::xr_enumerate_view_configurations);
    e.xrGetViewConfigurationProperties = Some(system::xr_get_view_configuration_properties);
    e.xrEnumerateViewConfigurationViews = Some(system::xr_enumerate_view_configuration_views);
    e.xrEnumerateEnvironmentBlendModes = Some(system::xr_enumerate_environment_blend_modes);
    e.xrGetMetalGraphicsRequirementsKHR = Some(system::xr_get_metal_graphics_requirements);

    e.xrCreateSession = Some(session::xr_create_session);
    e.xrDestroySession = Some(session::xr_destroy_session);
    e.xrBeginSession = Some(session::xr_begin_session);
    e.xrEndSession = Some(session::xr_end_session);
    e.xrRequestExitSession = Some(session::xr_request_exit_session);
    e.xrWaitFrame = Some(session::xr_wait_frame);
    e.xrBeginFrame = Some(session::xr_begin_frame);
    e.xrEndFrame = Some(session::xr_end_frame);

    e.xrEnumerateReferenceSpaces = Some(space::xr_enumerate_reference_spaces);
    e.xrCreateReferenceSpace = Some(space::xr_create_reference_space);
    e.xrGetReferenceSpaceBoundsRect = Some(space::xr_get_reference_space_bounds_rect);
    e.xrCreateActionSpace = Some(space::xr_create_action_space);
    e.xrDestroySpace = Some(space::xr_destroy_space);
    e.xrLocateSpace = Some(space::xr_locate_space);
    e.xrLocateSpaces = Some(space::xr_locate_spaces);
    e.xrLocateViews = Some(space::xr_locate_views);

    e.xrEnumerateSwapchainFormats = Some(swapchain::xr_enumerate_swapchain_formats);
    e.xrCreateSwapchain = Some(swapchain::xr_create_swapchain);
    e.xrDestroySwapchain = Some(swapchain::xr_destroy_swapchain);
    e.xrEnumerateSwapchainImages = Some(swapchain::xr_enumerate_swapchain_images);
    e.xrAcquireSwapchainImage = Some(swapchain::xr_acquire_swapchain_image);
    e.xrWaitSwapchainImage = Some(swapchain::xr_wait_swapchain_image);
    e.xrReleaseSwapchainImage = Some(swapchain::xr_release_swapchain_image);

    e.xrCreateActionSet = Some(actions::xr_create_action_set);
    e.xrDestroyActionSet = Some(actions::xr_destroy_action_set);
    e.xrCreateAction = Some(actions::xr_create_action);
    e.xrDestroyAction = Some(actions::xr_destroy_action);
    e.xrSuggestInteractionProfileBindings = Some(actions::xr_suggest_interaction_profile_bindings);
    e.xrAttachSessionActionSets = Some(actions::xr_attach_session_action_sets);
    e.xrGetCurrentInteractionProfile = Some(actions::xr_get_current_interaction_profile);
    e.xrSyncActions = Some(actions::xr_sync_actions);
    e.xrGetActionStateBoolean = Some(actions::xr_get_action_state_boolean);
    e.xrGetActionStateFloat = Some(actions::xr_get_action_state_float);
    e.xrGetActionStateVector2f = Some(actions::xr_get_action_state_vector2f);
    e.xrGetActionStatePose = Some(actions::xr_get_action_state_pose);
    e.xrApplyHapticFeedback = Some(actions::xr_apply_haptic_feedback);
    e.xrStopHapticFeedback = Some(actions::xr_stop_haptic_feedback);
    e.xrEnumerateBoundSourcesForAction = Some(actions::xr_enumerate_bound_sources_for_action);
    e.xrGetInputSourceLocalizedName = Some(actions::xr_get_input_source_localized_name);
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
    use openloop_openxr_abi::XR_KHR_METAL_ENABLE_EXTENSION_NAME;

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
