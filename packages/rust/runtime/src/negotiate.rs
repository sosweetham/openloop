use std::mem::size_of;

use tether_openxr_abi::{
    XR_CURRENT_API_VERSION, XR_CURRENT_LOADER_RUNTIME_VERSION, XR_LOADER_INFO_STRUCT_VERSION,
    XR_RUNTIME_INFO_STRUCT_VERSION, XrLoaderInterfaceStructs, XrNegotiateLoaderInfo,
    XrNegotiateRuntimeRequest, XrResult,
};

use crate::{dispatch, ffi, version};

/// The one symbol the loader resolves by name; everything else goes through
/// `xrGetInstanceProcAddr`.
///
/// # Safety
/// Called by the OpenXR loader with pointers it owns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xrNegotiateLoaderRuntimeInterface(
    loader_info: *const XrNegotiateLoaderInfo,
    runtime_request: *mut XrNegotiateRuntimeRequest,
) -> XrResult {
    ffi::guard(|| {
        // SAFETY: the loader passes valid pointers or null; null is rejected.
        let (Some(info), Some(request)) = (unsafe { loader_info.as_ref() }, unsafe {
            runtime_request.as_mut()
        }) else {
            return XrResult::XR_ERROR_INITIALIZATION_FAILED;
        };
        negotiate(info, request)
    })
}

fn negotiate(info: &XrNegotiateLoaderInfo, request: &mut XrNegotiateRuntimeRequest) -> XrResult {
    let loader_info_ok = info.structType
        == XrLoaderInterfaceStructs::XR_LOADER_INTERFACE_STRUCT_LOADER_INFO
        && info.structVersion == XR_LOADER_INFO_STRUCT_VERSION
        && info.structSize == size_of::<XrNegotiateLoaderInfo>();
    let request_ok = request.structType
        == XrLoaderInterfaceStructs::XR_LOADER_INTERFACE_STRUCT_RUNTIME_REQUEST
        && request.structVersion == XR_RUNTIME_INFO_STRUCT_VERSION
        && request.structSize == size_of::<XrNegotiateRuntimeRequest>();
    let interface_ok = (info.minInterfaceVersion..=info.maxInterfaceVersion)
        .contains(&XR_CURRENT_LOADER_RUNTIME_VERSION);
    let ours = version::major_minor(XR_CURRENT_API_VERSION);
    let api_ok = version::major_minor(info.minApiVersion) <= ours && ours <= info.maxApiVersion;

    if !(loader_info_ok && request_ok && interface_ok && api_ok) {
        return XrResult::XR_ERROR_INITIALIZATION_FAILED;
    }

    request.runtimeInterfaceVersion = XR_CURRENT_LOADER_RUNTIME_VERSION;
    request.runtimeApiVersion = XR_CURRENT_API_VERSION;
    request.getInstanceProcAddr = Some(dispatch::xr_get_instance_proc_addr);
    XrResult::XR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use tether_openxr_abi::XR_API_VERSION_1_0;

    fn loader_info() -> XrNegotiateLoaderInfo {
        XrNegotiateLoaderInfo {
            structType: XrLoaderInterfaceStructs::XR_LOADER_INTERFACE_STRUCT_LOADER_INFO,
            structVersion: XR_LOADER_INFO_STRUCT_VERSION,
            structSize: size_of::<XrNegotiateLoaderInfo>(),
            minInterfaceVersion: 1,
            maxInterfaceVersion: XR_CURRENT_LOADER_RUNTIME_VERSION,
            minApiVersion: XR_API_VERSION_1_0,
            maxApiVersion: version::make(1, 0x3ff, 0xfff),
        }
    }

    fn request() -> XrNegotiateRuntimeRequest {
        XrNegotiateRuntimeRequest {
            structType: XrLoaderInterfaceStructs::XR_LOADER_INTERFACE_STRUCT_RUNTIME_REQUEST,
            structVersion: XR_RUNTIME_INFO_STRUCT_VERSION,
            structSize: size_of::<XrNegotiateRuntimeRequest>(),
            runtimeInterfaceVersion: 0,
            runtimeApiVersion: 0,
            getInstanceProcAddr: None,
        }
    }

    #[test]
    fn accepts_a_current_loader() {
        let mut req = request();
        assert_eq!(negotiate(&loader_info(), &mut req), XrResult::XR_SUCCESS);
        assert_eq!(req.runtimeApiVersion, XR_CURRENT_API_VERSION);
        assert!(req.getInstanceProcAddr.is_some());
    }

    #[test]
    fn rejects_a_loader_that_needs_a_newer_api() {
        let mut info = loader_info();
        info.minApiVersion = version::make(2, 0, 0);
        assert_eq!(
            negotiate(&info, &mut request()),
            XrResult::XR_ERROR_INITIALIZATION_FAILED
        );
    }

    #[test]
    fn rejects_a_mismatched_struct() {
        let mut info = loader_info();
        info.structSize -= 1;
        assert_eq!(
            negotiate(&info, &mut request()),
            XrResult::XR_ERROR_INITIALIZATION_FAILED
        );
    }
}
