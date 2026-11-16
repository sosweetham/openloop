use std::num::NonZeroU32;

use glow::HasContext;

pub struct Renderer {
    gl: glow::Context,
    framebuffer: glow::Framebuffer,
}

impl Renderer {
    pub fn new(gl: glow::Context) -> Result<Self, String> {
        // SAFETY: the GLES context is current.
        let framebuffer = unsafe { gl.create_framebuffer() }?;
        Ok(Self { gl, framebuffer })
    }

    /// Shown while there is no video: a dim floor-to-sky gradient stand-in.
    pub fn placeholder(&self, target: u32, width: u32, height: u32) {
        let gl = &self.gl;
        // SAFETY: `target` is a live swapchain texture name; the context is current.
        unsafe {
            self.bind_target(target);
            gl.viewport(0, 0, width as i32, height as i32);
            gl.clear_color(0.06, 0.08, 0.11, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.flush();
        }
    }

    unsafe fn bind_target(&self, target: u32) {
        let gl = &self.gl;
        let texture = NonZeroU32::new(target).map(glow::NativeTexture);
        // SAFETY: forwarded from the caller.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                texture,
                0,
            );
        }
    }
}
