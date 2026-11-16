//! Binds a decoded `AImage` to a GL external texture through an EGLImage; no pixel copies.

use std::ffi::c_void;

use glow::HasContext;
use ndk::media::image_reader::Image;

use crate::egl::Egl;

const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: i32 = 1;

type GetNativeClientBuffer = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type CreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageTargetTexture = unsafe extern "C" fn(u32, *mut c_void);

pub struct VideoTexture {
    pub texture: glow::Texture,
    display: *mut c_void,
    get_buffer: GetNativeClientBuffer,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    target_texture: ImageTargetTexture,
    /// The image being sampled; kept alive until the next one replaces it.
    current: Option<(Image, *mut c_void)>,
}

impl VideoTexture {
    pub fn new(egl: &Egl, gl: &glow::Context) -> Result<Self, String> {
        let load = |name: &str| {
            let f = egl.proc_address(name);
            if f.is_null() {
                Err(format!("missing {name}"))
            } else {
                Ok(f)
            }
        };
        // SAFETY: each pointer comes from eglGetProcAddress for the named function's signature.
        unsafe {
            Ok(Self {
                texture: gl.create_texture()?,
                display: egl.display.as_ptr(),
                get_buffer: std::mem::transmute::<*const c_void, GetNativeClientBuffer>(load(
                    "eglGetNativeClientBufferANDROID",
                )?),
                create_image: std::mem::transmute::<*const c_void, CreateImage>(load(
                    "eglCreateImageKHR",
                )?),
                destroy_image: std::mem::transmute::<*const c_void, DestroyImage>(load(
                    "eglDestroyImageKHR",
                )?),
                target_texture: std::mem::transmute::<*const c_void, ImageTargetTexture>(load(
                    "glEGLImageTargetTexture2DOES",
                )?),
                current: None,
            })
        }
    }

    /// Points the texture at `image`, releasing the previous one.
    pub fn bind(&mut self, gl: &glow::Context, image: Image) -> Result<(), String> {
        let buffer = image
            .hardware_buffer()
            .map_err(|e| format!("hardware buffer: {e:?}"))?;
        let attributes = [EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE];
        // SAFETY: valid display and AHardwareBuffer; the EGLImage is destroyed below or in Drop.
        let egl_image = unsafe {
            let client = (self.get_buffer)(buffer.as_ptr() as *const c_void);
            (self.create_image)(
                self.display,
                std::ptr::null_mut(),
                EGL_NATIVE_BUFFER_ANDROID,
                client,
                attributes.as_ptr(),
            )
        };
        if egl_image.is_null() {
            return Err("eglCreateImageKHR failed".into());
        }
        // SAFETY: current GL context; the texture exists.
        unsafe {
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.texture));
            (self.target_texture)(GL_TEXTURE_EXTERNAL_OES, egl_image);
            gl.tex_parameter_i32(
                GL_TEXTURE_EXTERNAL_OES,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                GL_TEXTURE_EXTERNAL_OES,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
        }
        if let Some((_, old)) = self.current.replace((image, egl_image)) {
            // SAFETY: the old EGLImage is no longer bound to the texture.
            unsafe { (self.destroy_image)(self.display, old) };
        }
        Ok(())
    }
}

impl Drop for VideoTexture {
    fn drop(&mut self) {
        if let Some((_, image)) = self.current.take() {
            // SAFETY: created by this object on this display.
            unsafe { (self.destroy_image)(self.display, image) };
        }
    }
}
