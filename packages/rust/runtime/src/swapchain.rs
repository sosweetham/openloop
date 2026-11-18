//! Metal swapchains (`XR_KHR_metal_enable`). Images are private GPU textures on the session's
//! device; every one is created with `PixelFormatView` so the compositor can read raw bytes.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLCommandQueue, MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor,
    MTLTextureType, MTLTextureUsage,
};
use openloop_openxr_abi::{
    XR_SWAPCHAIN_USAGE_UNORDERED_ACCESS_BIT, XrResult, XrSession, XrStructureType, XrSwapchain,
    XrSwapchainCreateInfo, XrSwapchainImageAcquireInfo, XrSwapchainImageBaseHeader,
    XrSwapchainImageMetalKHR, XrSwapchainImageReleaseInfo, XrSwapchainImageWaitInfo,
};

use crate::ffi::{self, OK};
use crate::handles::{handle, id};
use crate::session;
use crate::state::OBJECTS;

/// Offered in preference order; Godot and hello_xr pick from the front.
const FORMATS: [MTLPixelFormat; 7] = [
    MTLPixelFormat::RGBA8Unorm_sRGB,
    MTLPixelFormat::BGRA8Unorm_sRGB,
    MTLPixelFormat::RGBA8Unorm,
    MTLPixelFormat::BGRA8Unorm,
    MTLPixelFormat::RGBA16Float,
    MTLPixelFormat::Depth32Float,
    MTLPixelFormat::Depth32Float_Stencil8,
];
const IMAGES: usize = 3;
const MAX_SIZE: u32 = 8192;

type Texture = ProtocolObject<dyn MTLTexture>;

pub struct Swapchain {
    pub session: u64,
    images: Vec<Retained<Texture>>,
    state: Mutex<ImageState>,
}

// SAFETY: Metal textures are thread-safe reference-counted objects.
unsafe impl Send for Swapchain {}
unsafe impl Sync for Swapchain {}

#[derive(Default)]
struct ImageState {
    acquired: VecDeque<u32>,
    waited: bool,
    next: u32,
    released: Option<u32>,
}

impl Swapchain {
    /// The most recently released image: what a layer submitted this frame refers to.
    pub fn released(&self) -> Option<Retained<Texture>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.released.map(|i| self.images[i as usize].clone())
    }
}

fn lookup(swapchain: XrSwapchain) -> Result<std::sync::Arc<Swapchain>, XrResult> {
    OBJECTS
        .swapchains
        .get(id(swapchain))
        .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)
}

pub unsafe extern "C" fn xr_enumerate_swapchain_formats(
    session_handle: XrSession,
    capacity: u32,
    count: *mut u32,
    formats: *mut i64,
) -> XrResult {
    ffi::call(|| {
        session::lookup(session_handle)?;
        // SAFETY: forwarded caller buffers under the two-call contract.
        Ok(unsafe {
            ffi::two_call(&FORMATS, capacity, count, formats, |d, f| {
                let _: () = *d = f.0 as i64;
                Ok(())
            })
        })
    })
}

pub unsafe extern "C" fn xr_create_swapchain(
    session_handle: XrSession,
    info: *const XrSwapchainCreateInfo,
    out: *mut XrSwapchain,
) -> XrResult {
    ffi::call(|| {
        let s = session::lookup(session_handle)?;
        // SAFETY: caller-provided pointers.
        let info = unsafe { ffi::read(info) }?;
        let out = unsafe { ffi::write(out) }?;
        ffi::expect_type(info.type_, XrStructureType::XR_TYPE_SWAPCHAIN_CREATE_INFO)?;
        let format = FORMATS
            .iter()
            .copied()
            .find(|f| f.0 as i64 == info.format)
            .ok_or(XrResult::XR_ERROR_SWAPCHAIN_FORMAT_UNSUPPORTED)?;
        if info.faceCount != 1 || info.sampleCount != 1 {
            return Err(XrResult::XR_ERROR_FEATURE_UNSUPPORTED);
        }
        if !(1..=MAX_SIZE).contains(&info.width)
            || !(1..=MAX_SIZE).contains(&info.height)
            || info.arraySize == 0
            || info.mipCount == 0
        {
            return Err(XrResult::XR_ERROR_VALIDATION_FAILURE);
        }

        let descriptor = MTLTextureDescriptor::new();
        descriptor.setTextureType(if info.arraySize > 1 {
            MTLTextureType::Type2DArray
        } else {
            MTLTextureType::Type2D
        });
        descriptor.setPixelFormat(format);
        // SAFETY: plain descriptor configuration with validated sizes.
        unsafe {
            descriptor.setWidth(info.width as usize);
            descriptor.setHeight(info.height as usize);
            descriptor.setArrayLength(info.arraySize as usize);
            descriptor.setMipmapLevelCount(info.mipCount as usize);
        }
        let mut usage = MTLTextureUsage::ShaderRead
            | MTLTextureUsage::RenderTarget
            | MTLTextureUsage::PixelFormatView;
        if info.usageFlags & XR_SWAPCHAIN_USAGE_UNORDERED_ACCESS_BIT != 0 {
            usage |= MTLTextureUsage::ShaderWrite;
        }
        descriptor.setUsage(usage);
        descriptor.setStorageMode(MTLStorageMode::Private);
        let device = s.queue.0.device();
        let images = (0..IMAGES)
            .map(|_| {
                device
                    .newTextureWithDescriptor(&descriptor)
                    .ok_or(XrResult::XR_ERROR_OUT_OF_MEMORY)
            })
            .collect::<Result<Vec<_>, _>>()?;
        *out = handle(OBJECTS.swapchains.insert(Swapchain {
            session: id(session_handle),
            images,
            state: Mutex::default(),
        }));
        OK
    })
}

pub unsafe extern "C" fn xr_destroy_swapchain(swapchain: XrSwapchain) -> XrResult {
    ffi::call(|| {
        OBJECTS
            .swapchains
            .remove(id(swapchain))
            .ok_or(XrResult::XR_ERROR_HANDLE_INVALID)?;
        OK
    })
}

pub unsafe extern "C" fn xr_enumerate_swapchain_images(
    swapchain: XrSwapchain,
    capacity: u32,
    count: *mut u32,
    images: *mut XrSwapchainImageBaseHeader,
) -> XrResult {
    ffi::call(|| {
        let chain = lookup(swapchain)?;
        let images = images as *mut XrSwapchainImageMetalKHR;
        // SAFETY: forwarded caller buffers; XR_KHR_metal_enable makes them XrSwapchainImageMetalKHR.
        Ok(unsafe {
            ffi::two_call(&chain.images, capacity, count, images, |d, texture| {
                ffi::expect_type(d.type_, XrStructureType::XR_TYPE_SWAPCHAIN_IMAGE_METAL_KHR)?;
                d.texture = Retained::as_ptr(texture) as *mut c_void;
                Ok(())
            })
        })
    })
}

pub unsafe extern "C" fn xr_acquire_swapchain_image(
    swapchain: XrSwapchain,
    _info: *const XrSwapchainImageAcquireInfo,
    index: *mut u32,
) -> XrResult {
    ffi::call(|| {
        let chain = lookup(swapchain)?;
        // SAFETY: caller-provided output.
        let index = unsafe { ffi::write(index) }?;
        let mut state = chain.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.acquired.len() >= chain.images.len() {
            return Err(XrResult::XR_ERROR_CALL_ORDER_INVALID);
        }
        let next = state.next;
        state.next = (next + 1) % chain.images.len() as u32;
        state.acquired.push_back(next);
        *index = next;
        OK
    })
}

pub unsafe extern "C" fn xr_wait_swapchain_image(
    swapchain: XrSwapchain,
    _info: *const XrSwapchainImageWaitInfo,
) -> XrResult {
    ffi::call(|| {
        let chain = lookup(swapchain)?;
        let mut state = chain.state.lock().unwrap_or_else(|e| e.into_inner());
        // The compositor reads on the app's own queue, so a released image is free once the
        // app's next commands are queued behind that read; no CPU wait is needed.
        if state.acquired.is_empty() || state.waited {
            return Err(XrResult::XR_ERROR_CALL_ORDER_INVALID);
        }
        state.waited = true;
        OK
    })
}

pub unsafe extern "C" fn xr_release_swapchain_image(
    swapchain: XrSwapchain,
    _info: *const XrSwapchainImageReleaseInfo,
) -> XrResult {
    ffi::call(|| {
        let chain = lookup(swapchain)?;
        let mut state = chain.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.waited {
            return Err(XrResult::XR_ERROR_CALL_ORDER_INVALID);
        }
        state.waited = false;
        state.released = state.acquired.pop_front();
        OK
    })
}
