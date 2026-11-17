use std::num::NonZeroU32;

use glow::HasContext;

const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

/// A full-screen triangle; `v_pos` runs 0..1 from the bottom-left.
pub const VERTEX: &str = r#"#version 300 es
out vec2 v_pos;
void main() {
    vec2 xy = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_pos = xy;
    gl_Position = vec4(xy * 2.0 - 1.0, 0.0, 1.0);
}
"#;

// Decoded video is gamma-encoded and the swapchain is sRGB, which re-encodes on write, so the
// shader linearises first. Video rows run top-down; GL framebuffers run bottom-up.
// Texture coordinates need highp: Adreno runs mediump as fp16, whose 11 bits cannot address the
// texels of a 3000-texel-wide video (in [0.5, 1) it steps 1/2048, so the right eye repeats
// columns). With highp and a swapchain the size of the eye, each pixel lands on a texel centre.
const FRAGMENT: &str = r#"#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision highp float;
uniform samplerExternalOES u_video;
uniform vec2 u_offset;
uniform vec2 u_scale;
in vec2 v_pos;
out vec4 color;
vec3 to_linear(vec3 c) {
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(0.04045, c));
}
void main() {
    vec2 uv = u_offset + vec2(v_pos.x, 1.0 - v_pos.y) * u_scale;
    color = vec4(to_linear(texture(u_video, uv).rgb), 1.0);
}
"#;

/// # Safety
/// The GLES context must be current.
pub unsafe fn program(
    gl: &glow::Context,
    vertex: &str,
    fragment: &str,
) -> Result<glow::Program, String> {
    // SAFETY: forwarded from the caller.
    unsafe {
        let program = gl.create_program()?;
        for (kind, source) in [
            (glow::VERTEX_SHADER, vertex),
            (glow::FRAGMENT_SHADER, fragment),
        ] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(format!("shader: {}", gl.get_shader_info_log(shader)));
            }
            gl.attach_shader(program, shader);
            gl.delete_shader(shader);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(format!("program: {}", gl.get_program_info_log(program)));
        }
        Ok(program)
    }
}

pub struct Renderer {
    pub gl: glow::Context,
    framebuffer: glow::Framebuffer,
    program: glow::Program,
    vertex_array: glow::VertexArray,
    offset: Option<glow::UniformLocation>,
    scale: Option<glow::UniformLocation>,
}

impl Renderer {
    pub fn new(gl: glow::Context) -> Result<Self, String> {
        // SAFETY: the GLES context is current.
        unsafe {
            let framebuffer = gl.create_framebuffer()?;
            let vertex_array = gl.create_vertex_array()?;
            let program = program(&gl, VERTEX, FRAGMENT)?;
            let offset = gl.get_uniform_location(program, "u_offset");
            let scale = gl.get_uniform_location(program, "u_scale");
            Ok(Self {
                gl,
                framebuffer,
                program,
                vertex_array,
                offset,
                scale,
            })
        }
    }

    /// Shown while there is no video.
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

    /// Draws one eye of the side-by-side video into `target`: `picture` is the decoded picture's
    /// `[u, v, width, height]` in the video texture, both eyes together.
    pub fn eye(
        &self,
        target: u32,
        width: u32,
        height: u32,
        video: glow::Texture,
        picture: [f32; 4],
        eye: usize,
    ) {
        let [u, v, w, h] = picture;
        let gl = &self.gl;
        // SAFETY: live swapchain texture and video texture; the context is current.
        unsafe {
            self.bind_target(target);
            gl.viewport(0, 0, width as i32, height as i32);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vertex_array));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(video));
            gl.uniform_2_f32(self.offset.as_ref(), u + eye as f32 * w * 0.5, v);
            gl.uniform_2_f32(self.scale.as_ref(), w * 0.5, h);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.flush();
        }
    }

    pub unsafe fn bind_target(&self, target: u32) {
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
