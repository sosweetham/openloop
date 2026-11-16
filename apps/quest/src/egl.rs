use std::ffi::{CString, c_void};

use khronos_egl as egl;

const OPENGL_ES3_BIT: egl::Int = 0x40;

/// A headless GLES 3 context; OpenXR swapchains are the only render targets.
pub struct Egl {
    pub instance: egl::DynamicInstance<egl::EGL1_4>,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
    _surface: egl::Surface,
}

impl Egl {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: loads the system libEGL.so, which conforms to EGL 1.4.
        let instance = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required() }?;
        // SAFETY: EGL_DEFAULT_DISPLAY is always a valid argument.
        let display =
            unsafe { instance.get_display(egl::DEFAULT_DISPLAY) }.ok_or(egl::Error::BadDisplay)?;
        instance.initialize(display)?;
        let attributes = [
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::RENDERABLE_TYPE,
            OPENGL_ES3_BIT,
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::NONE,
        ];
        let config = instance
            .choose_first_config(display, &attributes)?
            .ok_or(egl::Error::BadConfig)?;
        let context = instance.create_context(
            display,
            config,
            None,
            &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
        )?;
        let surface = instance.create_pbuffer_surface(
            display,
            config,
            &[egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE],
        )?;
        instance.make_current(display, Some(surface), Some(surface), Some(context))?;
        Ok(Self {
            instance,
            display,
            config,
            context,
            _surface: surface,
        })
    }

    pub fn gl(&self) -> glow::Context {
        // SAFETY: the context is current on this thread for the life of the client.
        unsafe { glow::Context::from_loader_function(|name| self.proc_address(name)) }
    }

    pub fn proc_address(&self, name: &str) -> *const c_void {
        if let Some(f) = self.instance.get_proc_address(name) {
            return f as *const c_void;
        }
        // Some drivers only expose core GLES entry points through the library itself.
        let name = CString::new(name).expect("GL names have no NUL");
        // SAFETY: dlopen/dlsym with valid C strings; the handle is intentionally never closed.
        unsafe {
            let lib = libc::dlopen(c"libGLESv3.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                std::ptr::null()
            } else {
                libc::dlsym(lib, name.as_ptr())
            }
        }
    }
}
