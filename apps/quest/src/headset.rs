//! The headset side of OpenXR: session lifecycle, tracking and input upload, and presenting
//! each decoded frame with the pose it was rendered for so the compositor reprojects it.

use std::collections::HashMap;
use std::error::Error;
use std::time::Instant;

use openxr as xr;
use tether_protocol::{
    ControllerState, DeviceProfile, Fov, Haptics, Message, Pose, Tracking, Velocity, View, button,
};

use crate::decoder::Decoder;
use crate::egl::Egl;
use crate::menu::{self, Command, Menu, Shape, Status};
use crate::net::{Event, Net};
use crate::panel::Panel;
use crate::passthrough::{self, Passthrough};
use crate::render::Renderer;
use crate::video::VideoTexture;

const VIEW_TYPE: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;
const GL_SRGB8_ALPHA8: u32 = 0x8C43;
const TOUCH: &str = "/interaction_profiles/oculus/touch_controller";
const PREFERRED_REFRESH: f32 = 90.0;

struct Eye {
    swapchain: xr::Swapchain<xr::OpenGlEs>,
    images: Vec<u32>,
    width: u32,
    height: u32,
}

struct Input {
    set: xr::ActionSet,
    hands: [xr::Path; 2],
    grip_spaces: [xr::Space; 2],
    aim_spaces: [xr::Space; 2],
    grip: xr::Action<xr::Posef>,
    trigger: xr::Action<f32>,
    squeeze: xr::Action<f32>,
    thumbstick: xr::Action<xr::Vector2f>,
    /// `(action, protocol bit, counts as a touch rather than a press)`.
    flags: Vec<(xr::Action<bool>, u32, bool)>,
    haptic: xr::Action<xr::Haptic>,
}

pub struct Headset {
    renderer: Renderer,
    video: VideoTexture,
    eyes: Vec<Eye>,
    input: Input,
    menu: Menu,
    menu_open: bool,
    /// The room from the headset cameras, shown instead of the stream while it exists.
    passthrough: Option<Passthrough>,
    panel: Panel,
    status: Status,
    view_space: xr::Space,
    /// The tracking space every pose on the wire is in: floor at y = 0.
    space: xr::Space,
    frame_stream: xr::FrameStream<xr::OpenGlEs>,
    frame_waiter: xr::FrameWaiter,
    session: xr::Session<xr::OpenGlEs>,
    instance: xr::Instance,
    events: xr::EventDataBuffer,
    running: bool,
    net: Net,
    decoder: Option<Decoder>,
    /// Render views of frames sent but not yet shown, by frame index.
    pending: HashMap<u64, [View; 2]>,
    /// Render views of the frame in the video texture.
    shown: Option<[View; 2]>,
    // Declared last so it is dropped after the session that uses it.
    _egl: Egl,
}

fn pose(p: xr::Posef) -> Pose {
    let (o, v) = (p.orientation, p.position);
    Pose {
        position: [v.x, v.y, v.z],
        orientation: [o.x, o.y, o.z, o.w],
    }
}

fn posef(p: &Pose) -> xr::Posef {
    let ([x, y, z], [qx, qy, qz, qw]) = (p.position, p.orientation);
    xr::Posef {
        orientation: xr::Quaternionf {
            x: qx,
            y: qy,
            z: qz,
            w: qw,
        },
        position: xr::Vector3f { x, y, z },
    }
}

fn fov(f: xr::Fovf) -> Fov {
    Fov {
        left: f.angle_left,
        right: f.angle_right,
        up: f.angle_up,
        down: f.angle_down,
    }
}

fn fovf(f: &Fov) -> xr::Fovf {
    xr::Fovf {
        angle_left: f.left,
        angle_right: f.right,
        angle_up: f.up,
        angle_down: f.down,
    }
}

fn velocity(v: &xr::SpaceVelocity) -> Velocity {
    let (l, a) = (v.linear_velocity, v.angular_velocity);
    Velocity {
        linear: [l.x, l.y, l.z],
        angular: [a.x, a.y, a.z],
    }
}

impl Headset {
    /// `activity` is the Activity's JNI object. `ndk_context` holds the Application instead,
    /// and Meta's runtime only binds a session to an activity's window, and ends the shell's
    /// launch transition, when it is given the Activity.
    pub fn new(activity: *mut std::ffi::c_void) -> Result<Self, Box<dyn Error>> {
        let context = ndk_context::android_context();
        // SAFETY: android-activity publishes the process JavaVM and keeps a global reference to
        // the Activity alive while the app runs.
        let platform = unsafe { xr::AndroidPlatformInfo::new(context.vm(), activity) };
        // SAFETY: the APK ships the Khronos loader built from the pinned SDK.
        let entry = unsafe { xr::Entry::load(&platform) }?;
        let available = entry.enumerate_extensions()?;

        let mut extensions = xr::ExtensionSet::default();
        extensions.khr_android_create_instance = true;
        extensions.khr_opengl_es_enable = true;
        extensions.fb_display_refresh_rate = available.fb_display_refresh_rate;
        extensions.khr_convert_timespec_time = available.khr_convert_timespec_time;
        extensions.khr_composition_layer_cylinder = available.khr_composition_layer_cylinder;
        extensions.fb_passthrough = available.fb_passthrough;
        // OpenXR 1.1 makes LOCAL_FLOOR core; older runtimes get 1.0. The patch must be the
        // headers' own: Meta's runtime treats a low patch as a legacy app and switches on
        // compatibility behaviour, which disables STAGE among other things.
        let patch = xr::CURRENT_API_VERSION.patch();
        let app = |minor| xr::ApplicationInfo {
            application_name: "Tether",
            application_version: 1,
            engine_name: "Tether",
            engine_version: 1,
            api_version: xr::Version::new(1, minor, patch),
        };
        let instance = entry
            .create_instance(&app(1), &extensions, &[], &platform)
            .or_else(|_| entry.create_instance(&app(0), &extensions, &[], &platform))?;
        let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)?;
        let system_name = instance.system_properties(system)?.system_name;
        log::info!("OpenXR system: {system_name}");

        // xrCreateSession requires the requirements call first (XR_KHR_opengl_es_enable).
        instance.graphics_requirements::<xr::OpenGlEs>(system)?;
        let egl = Egl::new()?;
        // SAFETY: the EGL handles live in `egl` for longer than the session.
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

        // STAGE needs a boundary and cannot be located without one; LOCAL_FLOOR always can.
        let available_spaces = session.enumerate_reference_spaces()?;
        let space_type = [
            xr::ReferenceSpaceType::LOCAL_FLOOR,
            xr::ReferenceSpaceType::LOCAL,
        ]
        .into_iter()
        .find(|t| available_spaces.contains(t))
        .unwrap_or(xr::ReferenceSpaceType::LOCAL);
        let space = session.create_reference_space(space_type, xr::Posef::IDENTITY)?;
        let view_space =
            session.create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)?;
        let formats = session.enumerate_swapchain_formats()?;
        let format = if formats.contains(&GL_SRGB8_ALPHA8) {
            GL_SRGB8_ALPHA8
        } else {
            formats[0]
        };
        let config_views = instance.enumerate_view_configuration_views(system, VIEW_TYPE)?;
        let eyes = config_views
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

        let (refresh_rates, refresh_rate) = if instance.exts().fb_display_refresh_rate.is_some() {
            let rates = session.enumerate_display_refresh_rates()?;
            if rates.contains(&PREFERRED_REFRESH) {
                session.request_display_refresh_rate(PREFERRED_REFRESH)?;
            }
            (rates, PREFERRED_REFRESH)
        } else {
            (vec![PREFERRED_REFRESH], PREFERRED_REFRESH)
        };

        let input = Input::new(&instance, &session)?;
        let gl = egl.gl();
        let video = VideoTexture::new(&egl, &gl)?;
        let renderer = Renderer::new(gl)?;
        let panel = Panel::new(&session, &renderer.gl, format, menu::PIXELS)?;
        let shape = if instance.exts().khr_composition_layer_cylinder.is_some() {
            Shape::Curved
        } else {
            Shape::Flat
        };
        log::info!("menu panel: {shape:?}");
        let profile = DeviceProfile {
            name: system_name,
            eye_width: config_views[0].recommended_image_rect_width,
            eye_height: config_views[0].recommended_image_rect_height,
            refresh_rates,
            refresh_rate,
            interaction_profile: TOUCH.into(),
            supports_hevc: true,
        };
        Ok(Self {
            renderer,
            video,
            eyes,
            input,
            menu: Menu::new(shape),
            menu_open: false,
            panel,
            status: Status {
                connected: false,
                video: None,
                passthrough: false,
            },
            passthrough: None,
            view_space,
            space,
            frame_stream,
            frame_waiter,
            session,
            instance,
            events: xr::EventDataBuffer::new(),
            running: false,
            net: Net::start(profile),
            decoder: None,
            pending: HashMap::new(),
            shown: None,
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
                xr::Event::ReferenceSpaceChangePending(e)
                    if matches!(
                        e.reference_space_type(),
                        xr::ReferenceSpaceType::LOCAL | xr::ReferenceSpaceType::LOCAL_FLOOR
                    ) =>
                {
                    log::info!("system recenter");
                    self.net.send(&Message::Recenter);
                }
                xr::Event::InstanceLossPending(_) => return Ok(false),
                _ => {}
            }
        }
        Ok(true)
    }

    fn pump_network(&mut self) {
        while let Ok(event) = self.net.events.try_recv() {
            match event {
                Event::Connected => {
                    log::info!("streaming link up");
                    self.status.connected = true;
                }
                Event::Disconnected => {
                    log::info!("streaming link down");
                    self.status.connected = false;
                    self.status.video = None;
                    self.decoder = None;
                    self.pending.clear();
                    self.shown = None;
                }
                Event::Message(Message::ServerHello(hello)) => {
                    log::info!("video {}x{}", hello.video_width, hello.video_height);
                    self.status.video = Some((hello.video_width, hello.video_height));
                    self.pending.clear();
                    self.decoder = Decoder::new(hello.video_width, hello.video_height, true)
                        .map_err(|e| log::error!("decoder: {e}"))
                        .ok();
                }
                Event::Message(Message::VideoFrame(frame)) => {
                    if let Some(decoder) = &self.decoder {
                        self.pending.insert(frame.frame_index, frame.views);
                        self.pending.retain(|&i, _| i + 240 > frame.frame_index);
                        decoder.submit(frame.frame_index, frame.keyframe, frame.data);
                    }
                }
                Event::Message(Message::Haptics(h)) => self.input.haptics(&self.session, &h),
                Event::Message(_) => {}
            }
        }
        if let Some(decoder) = &self.decoder
            && decoder
                .needs_keyframe
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.net.send(&Message::KeyframeRequest);
        }
    }

    fn latest_video(&mut self) {
        let Some((image, index)) = self.decoder.as_ref().and_then(Decoder::latest) else {
            return;
        };
        let Some(views) = self.pending.remove(&index) else {
            return;
        };
        match self.video.bind(&self.renderer.gl, image) {
            Ok(()) => self.shown = Some(views),
            Err(e) => log::warn!("video frame: {e}"),
        }
    }

    pub fn frame(&mut self) -> Result<(), Box<dyn Error>> {
        self.pump_network();
        let state = self.frame_waiter.wait()?;
        self.frame_stream.begin()?;
        let time = state.predicted_display_time;
        self.send_tracking(time)?;
        if !state.should_render {
            self.frame_stream
                .end(time, xr::EnvironmentBlendMode::OPAQUE, &[])?;
            return Ok(());
        }

        self.latest_video();
        let views: [View; 2] = match self.shown {
            Some(views) => views,
            None => {
                let (_, located) = self.session.locate_views(VIEW_TYPE, time, &self.space)?;
                [0, 1].map(|i| View {
                    pose: pose(located[i].pose),
                    fov: fov(located[i].fov),
                })
            }
        };
        for (index, eye) in self.eyes.iter_mut().enumerate() {
            let image = eye.swapchain.acquire_image()?;
            eye.swapchain.wait_image(xr::Duration::INFINITE)?;
            let target = eye.images[image as usize];
            if self.shown.is_some() {
                self.renderer
                    .eye(target, eye.width, eye.height, self.video.texture, index);
            } else {
                self.renderer.placeholder(target, eye.width, eye.height);
            }
            eye.swapchain.release_image()?;
        }
        let projection: Vec<_> = self
            .eyes
            .iter()
            .zip(&views)
            .map(|(eye, view)| {
                xr::CompositionLayerProjectionView::new()
                    .pose(posef(&view.pose))
                    .fov(fovf(&view.fov))
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
        let video = xr::CompositionLayerProjection::new()
            .space(&self.space)
            .views(&projection);
        let room = self.passthrough.as_ref().map(Passthrough::layer);
        let mut layers: Vec<&xr::CompositionLayerBase<'_, xr::OpenGlEs>> = Vec::with_capacity(2);
        match &room {
            Some(room) => layers.push(passthrough::as_base(room)),
            None => layers.push(&*video),
        }
        let Some(panel_pose) = self.menu.panel() else {
            self.frame_stream
                .end(time, xr::EnvironmentBlendMode::OPAQUE, &layers)?;
            return Ok(());
        };
        if self.menu.redraw(self.status) {
            self.panel.upload(&self.renderer.gl, &self.menu.canvas);
        }
        self.panel.draw(&self.renderer.gl)?;
        let pose = posef(&panel_pose);
        let cylinder;
        let quad;
        match self.menu.shape() {
            Shape::Curved => {
                cylinder = self.panel.cylinder(
                    &self.space,
                    pose,
                    menu::RADIUS_M,
                    menu::CENTRAL_ANGLE,
                    menu::ASPECT,
                );
                layers.push(&*cylinder);
            }
            Shape::Flat => {
                quad = self.panel.quad(&self.space, pose, Shape::Flat.size_m());
                layers.push(&*quad);
            }
        }
        self.frame_stream
            .end(time, xr::EnvironmentBlendMode::OPAQUE, &layers)?;
        Ok(())
    }

    fn toggle_passthrough(&mut self) {
        if self.passthrough.take().is_some() {
            log::info!("passthrough off");
        } else {
            match Passthrough::start(&self.session) {
                Ok(p) => {
                    log::info!("passthrough on");
                    self.passthrough = Some(p);
                }
                Err(e) => log::error!("passthrough: {e}"),
            }
        }
        self.status.passthrough = self.passthrough.is_some();
    }

    fn send_tracking(&mut self, time: xr::Time) -> Result<(), xr::sys::Result> {
        let (head, head_velocity) = self.view_space.relate(&self.space, time)?;
        let (view_state, eyes) = self
            .session
            .locate_views(VIEW_TYPE, time, &self.view_space)?;
        // An invalid location's pose is undefined (Horizon OS reports one around every
        // recenter). Skipping the sample makes the Mac hold the last good one.
        let head_valid = head.location_flags.contains(
            xr::SpaceLocationFlags::ORIENTATION_VALID | xr::SpaceLocationFlags::POSITION_VALID,
        );
        let eyes_valid = view_state
            .contains(xr::ViewStateFlags::ORIENTATION_VALID | xr::ViewStateFlags::POSITION_VALID);
        if !head_valid || !eyes_valid {
            return Ok(());
        }
        let mut controllers = self.input.sample(&self.session, &self.space, time)?;
        let outcome = self
            .menu
            .input(&mut controllers, &pose(head.pose), Instant::now());
        // The app pauses and hides its controllers while the menu holds input focus.
        let open = self.menu.panel().is_some();
        if open != self.menu_open {
            self.menu_open = open;
            self.net.send(&Message::InputFocus { focused: !open });
        }
        if outcome.command == Some(Command::TogglePassthrough) {
            self.toggle_passthrough();
        }
        if outcome.command == Some(Command::Recenter) {
            log::info!("recenter from the menu");
            self.net.send(&Message::Recenter);
        }
        if let Some(hand) = outcome.buzz {
            let tick = Haptics {
                hand: hand as u8,
                amplitude: 0.5,
                duration_ns: 20_000_000,
                frequency: 0.0,
            };
            self.input.haptics(&self.session, &tick);
        }
        let tracking = Tracking {
            sample_time_ns: self.to_monotonic(time),
            head: pose(head.pose),
            head_velocity: velocity(&head_velocity),
            views: [0, 1].map(|i| View {
                pose: pose(eyes[i].pose),
                fov: fov(eyes[i].fov),
            }),
            controllers,
        };
        self.net.send(&Message::Tracking(Box::new(tracking)));
        Ok(())
    }

    /// `XrTime` on the wire clock (CLOCK_MONOTONIC). Without XR_KHR_convert_timespec_time the
    /// two are assumed equal, which holds for runtimes that use CLOCK_MONOTONIC for `XrTime`.
    fn to_monotonic(&self, time: xr::Time) -> i64 {
        let Some(ext) = self.instance.exts().khr_convert_timespec_time else {
            return time.as_nanos();
        };
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid instance and out-pointer; the function belongs to the enabled extension.
        let result =
            unsafe { (ext.convert_time_to_timespec_time)(self.instance.as_raw(), time, &mut ts) };
        if result == xr::sys::Result::SUCCESS {
            ts.tv_sec * 1_000_000_000 + ts.tv_nsec
        } else {
            time.as_nanos()
        }
    }
}

impl Input {
    fn new(
        instance: &xr::Instance,
        session: &xr::Session<xr::OpenGlEs>,
    ) -> Result<Self, xr::sys::Result> {
        let path = |p: &str| instance.string_to_path(p);
        let hands = [path("/user/hand/left")?, path("/user/hand/right")?];
        let set = instance.create_action_set("tether", "Tether", 0)?;
        let grip = set.create_action::<xr::Posef>("grip", "Grip", &hands)?;
        let aim = set.create_action::<xr::Posef>("aim", "Aim", &hands)?;
        let trigger = set.create_action::<f32>("trigger", "Trigger", &hands)?;
        let squeeze = set.create_action::<f32>("squeeze", "Squeeze", &hands)?;
        let thumbstick = set.create_action::<xr::Vector2f>("thumbstick", "Thumbstick", &hands)?;
        let haptic = set.create_action::<xr::Haptic>("haptic", "Haptic", &hands)?;
        let flag = |name: &str| set.create_action::<bool>(name, name, &hands);
        let flags = vec![
            (flag("primary")?, button::PRIMARY, false),
            (flag("secondary")?, button::SECONDARY, false),
            (flag("thumbstick_click")?, button::THUMBSTICK, false),
            (flag("menu")?, button::MENU, false),
            (flag("primary_touch")?, button::PRIMARY, true),
            (flag("secondary_touch")?, button::SECONDARY, true),
            (flag("thumbstick_touch")?, button::THUMBSTICK, true),
            (flag("trigger_touch")?, button::TRIGGER, true),
            (flag("thumbrest_touch")?, button::THUMBREST, true),
        ];

        let mut bindings = Vec::new();
        for side in ["left", "right"] {
            let at = |c: &str| path(&format!("/user/hand/{side}/{c}"));
            let (primary, secondary) = if side == "left" {
                ("x", "y")
            } else {
                ("a", "b")
            };
            bindings.push(xr::Binding::new(&grip, at("input/grip/pose")?));
            bindings.push(xr::Binding::new(&aim, at("input/aim/pose")?));
            bindings.push(xr::Binding::new(&trigger, at("input/trigger/value")?));
            bindings.push(xr::Binding::new(&squeeze, at("input/squeeze/value")?));
            bindings.push(xr::Binding::new(&thumbstick, at("input/thumbstick")?));
            bindings.push(xr::Binding::new(&haptic, at("output/haptic")?));
            let components = [
                format!("input/{primary}/click"),
                format!("input/{secondary}/click"),
                "input/thumbstick/click".into(),
                "input/menu/click".into(),
                format!("input/{primary}/touch"),
                format!("input/{secondary}/touch"),
                "input/thumbstick/touch".into(),
                "input/trigger/touch".into(),
                "input/thumbrest/touch".into(),
            ];
            for ((action, _, _), component) in flags.iter().zip(&components) {
                // The right controller has no menu button on Touch.
                if side == "right" && component == "input/menu/click" {
                    continue;
                }
                bindings.push(xr::Binding::new(action, at(component)?));
            }
        }
        instance.suggest_interaction_profile_bindings(path(TOUCH)?, &bindings)?;
        session.attach_action_sets(&[&set])?;
        let space =
            |a: &xr::Action<xr::Posef>, hand| a.create_space(session, hand, xr::Posef::IDENTITY);
        Ok(Self {
            grip_spaces: [space(&grip, hands[0])?, space(&grip, hands[1])?],
            aim_spaces: [space(&aim, hands[0])?, space(&aim, hands[1])?],
            set,
            hands,
            grip,
            trigger,
            squeeze,
            thumbstick,
            flags,
            haptic,
        })
    }

    fn sample(
        &self,
        session: &xr::Session<xr::OpenGlEs>,
        space: &xr::Space,
        time: xr::Time,
    ) -> Result<[ControllerState; 2], xr::sys::Result> {
        session.sync_actions(&[xr::ActiveActionSet::new(&self.set)])?;
        let mut out = [ControllerState::default(); 2];
        for (hand, state) in out.iter_mut().enumerate() {
            let path = self.hands[hand];
            if !self.grip.is_active(session, path)? {
                continue;
            }
            let (grip, grip_velocity) = self.grip_spaces[hand].relate(space, time)?;
            let aim = self.aim_spaces[hand].locate(space, time)?;
            let valid =
                xr::SpaceLocationFlags::ORIENTATION_VALID | xr::SpaceLocationFlags::POSITION_VALID;
            let tracked = grip.location_flags.contains(valid) && aim.location_flags.contains(valid);
            state.active = tracked;
            state.grip = pose(grip.pose);
            state.aim = pose(aim.pose);
            state.grip_velocity = velocity(&grip_velocity);
            state.trigger = self.trigger.state(session, path)?.current_state;
            state.squeeze = self.squeeze.state(session, path)?.current_state;
            let stick = self.thumbstick.state(session, path)?.current_state;
            state.thumbstick = [stick.x, stick.y];
            for (action, bit, touch) in &self.flags {
                if action.state(session, path)?.current_state {
                    if *touch {
                        state.touches |= bit
                    } else {
                        state.buttons |= bit
                    }
                }
            }
            if state.trigger > 0.5 {
                state.buttons |= button::TRIGGER;
            }
            if state.squeeze > 0.5 {
                state.buttons |= button::SQUEEZE;
            }
        }
        Ok(out)
    }

    fn haptics(&self, session: &xr::Session<xr::OpenGlEs>, h: &Haptics) {
        let Some(&hand) = self.hands.get(h.hand as usize) else {
            return;
        };
        let result = if h.duration_ns == 0 {
            self.haptic.stop_feedback(session, hand)
        } else {
            let vibration = xr::HapticVibration::new()
                .amplitude(h.amplitude)
                .frequency(h.frequency)
                .duration(xr::Duration::from_nanos(h.duration_ns));
            self.haptic.apply_feedback(session, hand, &vibration)
        };
        if let Err(e) = result {
            log::warn!("haptics: {e}");
        }
    }
}
