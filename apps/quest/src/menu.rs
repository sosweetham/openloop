//! Tether's in-headset menu. Horizon OS keeps the Meta button to itself, so holding the left
//! controller's menu button opens it instead; a short press still reaches the app, on release.
//! Either controller points at it and its trigger chooses. The app gets no button input while
//! the menu is open.
//!
//! The panel is a curved section of a cylinder around the viewer (XR_KHR_composition_layer_cylinder),
//! or a flat quad on runtimes without that extension.

use std::time::{Duration, Instant};

use glam::{Quat, Vec3};
use tether_protocol::{ControllerState, Pose, button};

use crate::ui::{Canvas, Rgba};

const HOLD: Duration = Duration::from_millis(700);
/// How long a short press is shown to the app after the button is released.
const PULSE: Duration = Duration::from_millis(80);
const LEFT: usize = 0;
const TRIGGER_DOWN: f32 = 0.6;

pub const PIXELS: (usize, usize) = (1600, 902);
/// Layout units: drawing and hit testing use these, the canvas scales them to pixels.
const LAYOUT: (f32, f32) = (1024.0, 1024.0 * PIXELS.1 as f32 / PIXELS.0 as f32);
pub const RADIUS_M: f32 = 1.2;
pub const ARC_M: f32 = 1.1;
pub const ASPECT: f32 = PIXELS.0 as f32 / PIXELS.1 as f32;
pub const CENTRAL_ANGLE: f32 = ARC_M / RADIUS_M;
const HEIGHT_M: f32 = ARC_M / ASPECT;
const BELOW_EYES_M: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Curved,
    Flat,
}

impl Shape {
    pub fn size_m(self) -> (f32, f32) {
        (ARC_M, HEIGHT_M)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Recenter,
    TogglePassthrough,
    ToggleSharpening,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    Recenter,
    Passthrough,
    Sharpening,
    Close,
}

const ITEMS: [(Item, &str); 4] = [
    (Item::Recenter, "Recenter view"),
    (Item::Passthrough, "Passthrough"),
    (Item::Sharpening, "Sharpening"),
    (Item::Close, "Close"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub connected: bool,
    pub video: Option<(u32, u32)>,
    pub passthrough: bool,
    /// Whether the headset's compositor sharpens the stream; `None` where it cannot.
    pub sharpening: Option<bool>,
}

#[derive(Default)]
pub struct Outcome {
    pub command: Option<Command>,
    /// Hand to give a short haptic tick.
    pub buzz: Option<usize>,
}

pub struct Menu {
    shape: Shape,
    pressed_at: Option<Instant>,
    /// The current press has already opened or closed the menu.
    press_toggled: bool,
    pulse_until: Option<Instant>,
    /// The layer's pose in the tracking space; `None` while closed. For the curved panel it is
    /// the cylinder's centre, at the viewer.
    panel: Option<Pose>,
    pointer: usize,
    /// In layout units.
    cursor: Option<(f32, f32)>,
    hover: Option<usize>,
    triggers: [bool; 2],
    drawn: Option<(Option<usize>, Status)>,
    pub canvas: Canvas,
}

impl Menu {
    pub fn new(shape: Shape) -> Self {
        Self {
            shape,
            pressed_at: None,
            press_toggled: false,
            pulse_until: None,
            panel: None,
            pointer: 1,
            cursor: None,
            hover: None,
            triggers: [false; 2],
            drawn: None,
            canvas: Canvas::new(PIXELS.0, PIXELS.1, PIXELS.0 as f32 / LAYOUT.0),
        }
    }

    pub fn shape(&self) -> Shape {
        self.shape
    }

    pub fn panel(&self) -> Option<Pose> {
        self.panel
    }

    fn close(&mut self) {
        self.panel = None;
        self.cursor = None;
        self.hover = None;
        self.drawn = None;
    }

    /// Runs the menu on this sample and edits `controllers` into what the app should see.
    pub fn input(
        &mut self,
        controllers: &mut [ControllerState; 2],
        head: &Pose,
        now: Instant,
    ) -> Outcome {
        let mut out = Outcome::default();
        let menu_down = controllers[LEFT].buttons & button::MENU != 0;
        controllers[LEFT].buttons &= !button::MENU;
        match (menu_down, self.pressed_at) {
            (true, None) => {
                self.pressed_at = Some(now);
                self.press_toggled = false;
            }
            (true, Some(at)) if !self.press_toggled && now - at >= HOLD => {
                self.press_toggled = true;
                if self.panel.is_some() {
                    self.close();
                } else {
                    self.panel = Some(self.place(head));
                }
                out.buzz = Some(LEFT);
            }
            (false, Some(_)) => {
                self.pressed_at = None;
                if !self.press_toggled {
                    if self.panel.is_some() {
                        self.close();
                    } else {
                        self.pulse_until = Some(now + PULSE);
                    }
                }
            }
            _ => {}
        }
        match self.pulse_until {
            Some(until) if now < until => controllers[LEFT].buttons |= button::MENU,
            _ => self.pulse_until = None,
        }

        let Some(panel) = self.panel else {
            return out;
        };
        let mut pulled = [false; 2];
        for (hand, c) in controllers.iter().enumerate() {
            let down = c.active && c.trigger > TRIGGER_DOWN;
            pulled[hand] = down && !self.triggers[hand];
            self.triggers[hand] = down;
            if pulled[hand] {
                self.pointer = hand;
            }
        }
        let aim = &controllers[self.pointer];
        self.cursor = aim
            .active
            .then(|| self.hit(&panel, &aim.aim))
            .flatten()
            .map(|(u, v)| (u * LAYOUT.0, v * LAYOUT.1));
        self.hover = self.cursor.and_then(item_at);
        if pulled[self.pointer]
            && let Some(index) = self.hover
        {
            out.buzz = Some(self.pointer);
            match ITEMS[index].0 {
                Item::Recenter => {
                    out.command = Some(Command::Recenter);
                    self.close();
                }
                // Both stay open so the result can be seen and switched back.
                Item::Passthrough => out.command = Some(Command::TogglePassthrough),
                Item::Sharpening => out.command = Some(Command::ToggleSharpening),
                Item::Close => self.close(),
            }
        }
        for c in controllers.iter_mut() {
            c.buttons = 0;
            c.touches = 0;
            c.trigger = 0.0;
            c.squeeze = 0.0;
            c.thumbstick = [0.0; 2];
        }
        out
    }

    /// Repaints the canvas if anything visible changed; `true` when it did.
    pub fn redraw(&mut self, status: Status) -> bool {
        // Redrawn only when what it shows changes: rasterising every frame stalls the frame loop.
        let key = (self.hover, status);
        if self.drawn == Some(key) {
            return false;
        }
        self.drawn = Some(key);
        const WHITE: Rgba = [240, 242, 246, 255];
        const MUTED: Rgba = [150, 158, 172, 255];
        let (w, h) = LAYOUT;
        let c = &mut self.canvas;
        c.clear();
        c.rounded_rect(0.0, 0.0, w, h, 28.0, [22, 25, 31, 238]);
        c.text(56.0, 96.0, 52.0, "Tether", WHITE);
        let status_line = match status {
            Status {
                connected: false, ..
            } => "Waiting for the Mac".to_owned(),
            Status {
                video: Some((vw, vh)),
                ..
            } => format!("Streaming from the Mac · {vw}×{vh}"),
            Status { video: None, .. } => "Connected to the Mac".to_owned(),
        };
        c.text(56.0, 138.0, 26.0, &status_line, MUTED);
        for (index, (item, label)) in ITEMS.iter().enumerate() {
            let (x, y, bw, bh) = button_rect(*item);
            let on = match item {
                Item::Passthrough => status.passthrough,
                Item::Sharpening => status.sharpening == Some(true),
                _ => false,
            };
            let fill = match (self.hover == Some(index), on) {
                (true, _) => [56, 108, 224, 255],
                (false, true) => [40, 140, 106, 255],
                (false, false) => [42, 47, 56, 255],
            };
            c.rounded_rect(x, y, bw, bh, 22.0, fill);
            let label = match (item, on) {
                (Item::Passthrough, true) => "Passthrough  ·  on",
                (Item::Passthrough, false) => "Passthrough  ·  off",
                (Item::Sharpening, true) => "Sharpening  ·  on",
                (Item::Sharpening, false) if status.sharpening.is_none() => "Sharpening  ·  n/a",
                (Item::Sharpening, false) => "Sharpening  ·  off",
                _ => label,
            };
            c.text(x + 36.0, y + 58.0, 36.0, label, WHITE);
        }
        c.text(
            56.0,
            h - 36.0,
            22.0,
            "Point and pull the trigger to choose  ·  hold Menu to close",
            MUTED,
        );
        true
    }

    /// Upright and facing the way the head looks. The curved panel's cylinder is centred on
    /// the viewer; the flat one sits where the curve's middle would be.
    fn place(&self, head: &Pose) -> Pose {
        let forward = rotation(head) * Vec3::NEG_Z;
        let flat = Vec3::new(forward.x, 0.0, forward.z)
            .try_normalize()
            .unwrap_or(Vec3::NEG_Z);
        let yaw = (-flat.x).atan2(-flat.z);
        let mut centre = Vec3::from_array(head.position) - Vec3::Y * BELOW_EYES_M;
        if self.shape == Shape::Flat {
            centre += flat * RADIUS_M;
        }
        Pose {
            position: centre.to_array(),
            orientation: Quat::from_rotation_y(yaw).to_array(),
        }
    }

    /// Where the aim ray meets the panel, as `(u, v)` in 0..1 from the top-left.
    fn hit(&self, panel: &Pose, aim: &Pose) -> Option<(f32, f32)> {
        let facing = rotation(panel);
        let to_local = facing.inverse();
        let origin = to_local * (Vec3::from_array(aim.position) - Vec3::from_array(panel.position));
        let direction = to_local * (rotation(aim) * Vec3::NEG_Z);
        let (u, y) = match self.shape {
            Shape::Curved => {
                // The ray starts inside the cylinder, so the far root is where it leaves it.
                let a = direction.x * direction.x + direction.z * direction.z;
                let b = 2.0 * (origin.x * direction.x + origin.z * direction.z);
                let c = origin.x * origin.x + origin.z * origin.z - RADIUS_M * RADIUS_M;
                let discriminant = b * b - 4.0 * a * c;
                if a < 1e-6 || discriminant < 0.0 {
                    return None;
                }
                let t = (-b + discriminant.sqrt()) / (2.0 * a);
                if t <= 0.0 {
                    return None;
                }
                let p = origin + direction * t;
                // The visible arc is centred on -Z (angle 0) and widens towards +X on the right.
                (p.x.atan2(-p.z) / CENTRAL_ANGLE + 0.5, p.y)
            }
            Shape::Flat => {
                // A quad layer faces its pose's +Z.
                if direction.z > -1e-4 {
                    return None;
                }
                let t = -origin.z / direction.z;
                if t <= 0.0 {
                    return None;
                }
                let p = origin + direction * t;
                (p.x / ARC_M + 0.5, p.y)
            }
        };
        let v = 0.5 - y / HEIGHT_M;
        ((0.0..1.0).contains(&u) && (0.0..1.0).contains(&v)).then_some((u, v))
    }
}

/// The two switches share the middle row.
fn button_rect(item: Item) -> (f32, f32, f32, f32) {
    let (left, width, gap) = (56.0, LAYOUT.0 - 112.0, 24.0);
    let half = (width - gap) / 2.0;
    let row = |r: f32| 176.0 + r * 104.0;
    match item {
        Item::Recenter => (left, row(0.0), width, 88.0),
        Item::Passthrough => (left, row(1.0), half, 88.0),
        Item::Sharpening => (left + half + gap, row(1.0), half, 88.0),
        Item::Close => (left, row(2.0), width, 88.0),
    }
}

fn item_at((x, y): (f32, f32)) -> Option<usize> {
    ITEMS.iter().position(|(item, _)| {
        let (bx, by, bw, bh) = button_rect(*item);
        (bx..bx + bw).contains(&x) && (by..by + bh).contains(&y)
    })
}

fn rotation(p: &Pose) -> Quat {
    Quat::from_array(p.orientation)
}
