//! The menu as an OpenXR quad layer: the canvas is uploaded to a texture when it changes and
//! drawn into the layer's swapchain each frame the menu is open.

use std::error::Error;
use std::num::NonZeroU32;

use glow::HasContext;
use openxr as xr;

use crate::render::{VERTEX, program};
use crate::ui::Canvas;

const GL_SRGB8_ALPHA8: u32 = 0x8C43;

// The canvas is premultiplied and its rows run top-down; GL textures run bottom-up. highp, since
// mediump (fp16 on Adreno) cannot address 1600 texels one by one.
const FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform sampler2D u_canvas;
in vec2 v_pos;
out vec4 color;
void main() {
    color = texture(u_canvas, vec2(v_pos.x, 1.0 - v_pos.y));
}
"#;

pub struct Panel {
    swapchain: xr::Swapchain<xr::OpenGlEs>,
    images: Vec<u32>,
    width: u32,
    height: u32,
    canvas: glow::Texture,
    program: glow::Program,
    vertex_array: glow::VertexArray,
    framebuffer: glow::Framebuffer,
}

impl Panel {
    pub fn new(
        session: &xr::Session<xr::OpenGlEs>,
        gl: &glow::Context,
        format: u32,
        (width, height): (usize, usize),
    ) -> Result<Self, Box<dyn Error>> {
        let (width, height) = (width as u32, height as u32);
        let swapchain = session.create_swapchain(&xr::SwapchainCreateInfo {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT,
            format,
            sample_count: 1,
            width,
            height,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        })?;
        let images = swapchain.enumerate_images()?;
        // SAFETY: the GLES context is current.
        unsafe {
            let canvas = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(canvas));
            gl.tex_storage_2d(
                glow::TEXTURE_2D,
                1,
                GL_SRGB8_ALPHA8,
                width as i32,
                height as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            Ok(Self {
                swapchain,
                images,
                width,
                height,
                canvas,
                program: program(gl, VERTEX, FRAGMENT)?,
                vertex_array: gl.create_vertex_array()?,
                framebuffer: gl.create_framebuffer()?,
            })
        }
    }

    pub fn upload(&self, gl: &glow::Context, canvas: &Canvas) {
        // SAFETY: the texture was allocated at the canvas size; the context is current.
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.canvas));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                canvas.width as i32,
                canvas.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&canvas.pixels)),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    pub fn draw(&mut self, gl: &glow::Context) -> Result<(), xr::sys::Result> {
        let image = self.swapchain.acquire_image()?;
        self.swapchain.wait_image(xr::Duration::INFINITE)?;
        let target = NonZeroU32::new(self.images[image as usize]).map(glow::NativeTexture);
        // SAFETY: `target` is a live swapchain texture; the context is current.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                target,
                0,
            );
            gl.viewport(0, 0, self.width as i32, self.height as i32);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vertex_array));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.canvas));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.flush();
        }
        self.swapchain.release_image()
    }

    fn sub_image(&self) -> xr::SwapchainSubImage<'_, xr::OpenGlEs> {
        xr::SwapchainSubImage::new()
            .swapchain(&self.swapchain)
            .image_rect(xr::Rect2Di {
                offset: xr::Offset2Di { x: 0, y: 0 },
                extent: xr::Extent2Di {
                    width: self.width as i32,
                    height: self.height as i32,
                },
            })
    }

    // Layer alpha is premultiplied, the OpenXR default without
    // XR_COMPOSITION_LAYER_UNPREMULTIPLIED_ALPHA_BIT.

    pub fn quad<'a>(
        &'a self,
        space: &'a xr::Space,
        pose: xr::Posef,
        (width, height): (f32, f32),
    ) -> xr::CompositionLayerQuad<'a, xr::OpenGlEs> {
        xr::CompositionLayerQuad::new()
            .layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
            .space(space)
            .eye_visibility(xr::EyeVisibility::BOTH)
            .sub_image(self.sub_image())
            .pose(pose)
            .size(xr::Extent2Df { width, height })
    }

    /// `pose` is the cylinder's centre; the image covers `central_angle` of its inside.
    pub fn cylinder<'a>(
        &'a self,
        space: &'a xr::Space,
        pose: xr::Posef,
        radius: f32,
        central_angle: f32,
        aspect_ratio: f32,
    ) -> xr::CompositionLayerCylinderKHR<'a, xr::OpenGlEs> {
        xr::CompositionLayerCylinderKHR::new()
            .layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
            .space(space)
            .eye_visibility(xr::EyeVisibility::BOTH)
            .sub_image(self.sub_image())
            .pose(pose)
            .radius(radius)
            .central_angle(central_angle)
            .aspect_ratio(aspect_ratio)
    }
}
