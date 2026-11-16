use std::error::Error;

use openxr as xr;

use crate::egl::Egl;
use crate::render::Renderer;

const VIEW_TYPE: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;
const GL_SRGB8_ALPHA8: u32 = 0x8C43;

struct Eye {
    swapchain: xr::Swapchain<xr::OpenGlEs>,
    images: Vec<u32>,
    width: u32,
    height: u32,
}

/// The headset side of OpenXR: session lifecycle, tracking space, eye swapchains.
pub struct Headset {
    renderer: Renderer,
    eyes: Vec<Eye>,
    stage: xr::Space,
    frame_stream: xr::FrameStream<xr::OpenGlEs>,
    frame_waiter: xr::FrameWaiter,
    session: xr::Session<xr::OpenGlEs>,
    instance: xr::Instance,
    events: xr::EventDataBuffer,
    running: bool,
    // Dropped after the session that uses it.
    _egl: Egl,
}

impl Headset {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let context = ndk_context::android_context();
        // SAFETY: android-activity publishes the process JavaVM and this activity.
        let platform = unsafe { xr::AndroidPlatformInfo::new(context.vm(), context.context()) };
        // SAFETY: the APK ships the Khronos loader built from the pinned SDK.
        let entry = unsafe { xr::Entry::load(&platform) }?;
        let available = entry.enumerate_extensions()?;

        let mut extensions = xr::ExtensionSet::default();
        extensions.khr_android_create_instance = true;
        extensions.khr_opengl_es_enable = true;
        extensions.fb_display_refresh_rate = available.fb_display_refresh_rate;
        let app = xr::ApplicationInfo {
            application_name: "Tether",
            application_version: 1,
            engine_name: "Tether",
            engine_version: 1,
            api_version: xr::Version::new(1, 0, 0),
        };
        let instance = entry.create_instance(&app, &extensions, &[], &platform)?;
        let props = instance.properties()?;
        log::info!(
            "OpenXR runtime {} {}",
            props.runtime_name,
            props.runtime_version
        );

        let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)?;
        // xrCreateSession requires the requirements call first (XR_KHR_opengl_es_enable).
        instance.graphics_requirements::<xr::OpenGlEs>(system)?;
        let egl = Egl::new()?;
        // SAFETY: the EGL handles stay alive in `_egl` for longer than the session.
        let (session, frame_waiter, frame_stream) = unsafe {
            instance.create_session::<xr::OpenGlEs>(
                system,
                &xr::opengles::SessionCreateInfo::Android {
                    display: egl.display.as_ptr(),
                    config: egl.config.as_ptr(),
                    context: egl.context.as_ptr(),
                },
            )
        }?;

        let stage =
            session.create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)?;
        let formats = session.enumerate_swapchain_formats()?;
        let format = if formats.contains(&GL_SRGB8_ALPHA8) {
            GL_SRGB8_ALPHA8
        } else {
            formats[0]
        };
        let eyes = instance
            .enumerate_view_configuration_views(system, VIEW_TYPE)?
            .iter()
            .map(|view| -> Result<Eye, xr::sys::Result> {
                let (width, height) = (
                    view.recommended_image_rect_width,
                    view.recommended_image_rect_height,
                );
                let swapchain = session.create_swapchain(&xr::SwapchainCreateInfo {
                    create_flags: xr::SwapchainCreateFlags::EMPTY,
                    usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                        | xr::SwapchainUsageFlags::SAMPLED,
                    format,
                    sample_count: 1,
                    width,
                    height,
                    face_count: 1,
                    array_size: 1,
                    mip_count: 1,
                })?;
                let images = swapchain.enumerate_images()?;
                Ok(Eye {
                    swapchain,
                    images,
                    width,
                    height,
                })
            })
            .collect::<Result<_, _>>()?;

        let renderer = Renderer::new(egl.gl())?;
        Ok(Self {
            renderer,
            eyes,
            stage,
            frame_stream,
            frame_waiter,
            session,
            instance,
            events: xr::EventDataBuffer::new(),
            running: false,
            _egl: egl,
        })
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Handles pending OpenXR events; `false` once the session is over.
    pub fn poll(&mut self) -> Result<bool, xr::sys::Result> {
        while let Some(event) = self.instance.poll_event(&mut self.events)? {
            match event {
                xr::Event::SessionStateChanged(e) => {
                    log::info!("session state {:?}", e.state());
                    match e.state() {
                        xr::SessionState::READY => {
                            self.session.begin(VIEW_TYPE)?;
                            self.running = true;
                        }
                        xr::SessionState::STOPPING => {
                            self.session.end()?;
                            self.running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            return Ok(false);
                        }
                        _ => {}
                    }
                }
                xr::Event::InstanceLossPending(_) => return Ok(false),
                _ => {}
            }
        }
        Ok(true)
    }

    pub fn frame(&mut self) -> Result<(), xr::sys::Result> {
        let state = self.frame_waiter.wait()?;
        self.frame_stream.begin()?;
        if !state.should_render {
            return self.frame_stream.end(
                state.predicted_display_time,
                xr::EnvironmentBlendMode::OPAQUE,
                &[],
            );
        }
        let (_, views) =
            self.session
                .locate_views(VIEW_TYPE, state.predicted_display_time, &self.stage)?;
        for eye in &mut self.eyes {
            let index = eye.swapchain.acquire_image()?;
            eye.swapchain.wait_image(xr::Duration::INFINITE)?;
            self.renderer
                .placeholder(eye.images[index as usize], eye.width, eye.height);
            eye.swapchain.release_image()?;
        }
        let projection: Vec<_> = self
            .eyes
            .iter()
            .zip(&views)
            .map(|(eye, view)| {
                xr::CompositionLayerProjectionView::new()
                    .pose(view.pose)
                    .fov(view.fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&eye.swapchain)
                            .image_rect(xr::Rect2Di {
                                offset: xr::Offset2Di { x: 0, y: 0 },
                                extent: xr::Extent2Di {
                                    width: eye.width as i32,
                                    height: eye.height as i32,
                                },
                            }),
                    )
            })
            .collect();
        let layer = xr::CompositionLayerProjection::new()
            .space(&self.stage)
            .views(&projection);
        self.frame_stream.end(
            state.predicted_display_time,
            xr::EnvironmentBlendMode::OPAQUE,
            &[&layer],
        )
    }
}
