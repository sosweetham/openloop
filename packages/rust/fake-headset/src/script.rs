//! What the fake headset does over a session: how the head and controllers move, and timed cues
//! that press buttons, move sticks and send requests to the Mac.

use std::f32::consts::{PI, TAU};
use std::time::Duration;

use tether_protocol::{ControllerState, Fov, Pose, Velocity, View, button};

/// Quest 3-like eyes: 63 mm apart, each field of view wider towards the temple.
const IPD_M: f32 = 0.063;
const LEFT_FOV: Fov = Fov {
    left: -0.942,
    right: 0.698,
    up: 0.768,
    down: -0.873,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Motion {
    /// Head and controllers hold still.
    Still,
    /// The head looks around and the controllers circle, slowly enough to be a person.
    #[default]
    Sway,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hand {
    Left = 0,
    Right = 1,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Holds `button` (a `tether_protocol::button` bit) on `hand` for `hold`.
    Press {
        hand: Hand,
        button: u32,
        hold: Duration,
    },
    Trigger {
        hand: Hand,
        value: f32,
    },
    Squeeze {
        hand: Hand,
        value: f32,
    },
    Thumbstick {
        hand: Hand,
        x: f32,
        y: f32,
    },
    Recenter,
    RequestKeyframe,
    Focus(bool),
    /// Stops reading the link, so the Mac's sends back up as they would behind a stalled headset.
    PauseReading(Duration),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    /// From the start of the session.
    pub at: Duration,
    pub action: Action,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script {
    pub motion: Motion,
    pub cues: Vec<Cue>,
}

/// The head, eyes and controllers at one instant, before the inputs are applied.
pub struct Sample {
    pub head: Pose,
    pub head_velocity: Velocity,
    pub views: [View; 2],
    pub controllers: [ControllerState; 2],
}

impl Script {
    /// Where everything is `t` seconds into the session.
    pub fn sample(&self, t: f64) -> Sample {
        const DT: f64 = 1e-3;
        let (now, next) = (self.rig(t), self.rig(t + DT));
        let controllers = [0, 1].map(|hand| {
            let (grip, grip_next) = (&now.hands[hand], &next.hands[hand]);
            ControllerState {
                active: true,
                grip: grip.pose(),
                aim: grip.aim(),
                grip_velocity: grip.velocity(grip_next, DT),
                ..Default::default()
            }
        });
        Sample {
            head: now.head.pose(),
            head_velocity: now.head.velocity(&next.head, DT),
            views: eyes(),
            controllers,
        }
    }

    fn rig(&self, t: f64) -> Rig {
        let wave = |hz: f64, phase: f32| ((TAU as f64 * hz * t) as f32 + phase).sin();
        match self.motion {
            Motion::Still => Rig {
                head: Body::at([0.0, 1.6, 0.0], 0.0, 0.0),
                hands: [-1.0, 1.0].map(|side| Body::at([0.2 * side, 1.25, -0.35], 0.0, -0.3)),
            },
            Motion::Sway => Rig {
                head: Body::at(
                    [
                        0.05 * wave(0.1, 0.0),
                        1.6 + 0.01 * wave(0.5, 0.0),
                        0.03 * wave(0.07, 0.0),
                    ],
                    0.3 * wave(0.2, 0.0),
                    0.1 * wave(0.13, 0.0),
                ),
                hands: [-1.0, 1.0].map(|side| {
                    let phase = if side < 0.0 { 0.0 } else { PI };
                    Body::at(
                        [
                            0.2 * side + 0.08 * wave(0.5, phase + PI / 2.0),
                            1.25 + 0.08 * wave(0.5, phase),
                            -0.35,
                        ],
                        0.2 * side * wave(0.3, 0.0),
                        -0.3 + 0.2 * wave(0.4, phase),
                    )
                }),
            },
        }
    }
}

/// Eyes relative to the head, mirrored left and right.
pub fn eyes() -> [View; 2] {
    [-1.0f32, 1.0].map(|side| View {
        pose: Pose {
            position: [side * IPD_M / 2.0, 0.0, 0.0],
            ..Pose::IDENTITY
        },
        fov: if side < 0.0 {
            LEFT_FOV
        } else {
            Fov {
                left: -LEFT_FOV.right,
                right: -LEFT_FOV.left,
                ..LEFT_FOV
            }
        },
    })
}

struct Rig {
    head: Body,
    hands: [Body; 2],
}

/// A rigid body turned by yaw (about +Y) and then pitch (about +X).
struct Body {
    position: [f32; 3],
    yaw: f32,
    pitch: f32,
}

impl Body {
    fn at(position: [f32; 3], yaw: f32, pitch: f32) -> Self {
        Self {
            position,
            yaw,
            pitch,
        }
    }

    fn orientation(&self) -> [f32; 4] {
        let (sy, cy) = (self.yaw / 2.0).sin_cos();
        let (sp, cp) = (self.pitch / 2.0).sin_cos();
        [cy * sp, sy * cp, -sy * sp, cy * cp]
    }

    fn pose(&self) -> Pose {
        Pose {
            position: self.position,
            orientation: self.orientation(),
        }
    }

    /// The pointing ray starts a little ahead of the grip, along the grip's forward (-Z).
    fn aim(&self) -> Pose {
        let forward = rotate(self.orientation(), [0.0, 0.0, -0.05]);
        Pose {
            position: [0, 1, 2].map(|i| self.position[i] + forward[i]),
            orientation: self.orientation(),
        }
    }

    /// Linear and angular velocity in the tracking space, from this body and the same body `dt`
    /// seconds later.
    fn velocity(&self, next: &Body, dt: f64) -> Velocity {
        let dt = dt as f32;
        let (yaw_rate, pitch_rate) = ((next.yaw - self.yaw) / dt, (next.pitch - self.pitch) / dt);
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        Velocity {
            linear: [0, 1, 2].map(|i| (next.position[i] - self.position[i]) / dt),
            angular: [pitch_rate * cos_yaw, yaw_rate, -pitch_rate * sin_yaw],
        }
    }
}

fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let u = [x, y, z];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let t = cross(u, v).map(|c| 2.0 * c);
    let ut = cross(u, t);
    [0, 1, 2].map(|i| v[i] + w * t[i] + ut[i])
}

/// The buttons and axes a cue or the caller has set, applied on top of the script's poses.
#[derive(Clone, Copy, Debug, Default)]
pub struct HandInput {
    pub trigger: f32,
    pub squeeze: f32,
    pub thumbstick: [f32; 2],
    /// Per button bit, the session time in seconds until which it is held.
    held_until: [f64; 8],
}

impl HandInput {
    pub fn press(&mut self, button: u32, until: f64) {
        for (bit, held) in self.held_until.iter_mut().enumerate() {
            if button & (1 << bit) != 0 {
                *held = held.max(until);
            }
        }
    }

    /// Writes this input into `state` as the Quest client reports it: analog values past half
    /// way also set their button bit, and a pressed button is a touched one.
    pub fn apply(&self, t: f64, state: &mut ControllerState) {
        let pressed = (0..8)
            .filter(|&bit| self.held_until[bit] > t)
            .fold(0, |bits, bit| bits | 1 << bit);
        state.trigger = self.trigger;
        state.squeeze = self.squeeze;
        state.thumbstick = self.thumbstick;
        state.buttons = pressed;
        if self.trigger > 0.5 {
            state.buttons |= button::TRIGGER;
        }
        if self.squeeze > 0.5 {
            state.buttons |= button::SQUEEZE;
        }
        state.touches = pressed
            & (button::PRIMARY | button::SECONDARY | button::THUMBSTICK | button::THUMBREST);
        if self.trigger > 0.0 {
            state.touches |= button::TRIGGER;
        }
    }
}

/// Parses a cue as `SECONDS:ACTION`, the command line's form. Actions: `recenter`, `keyframe`,
/// `focus`, `unfocus`, `pause:MS`, `press:HAND:BUTTON[:MS]`, `trigger:HAND:VALUE`,
/// `squeeze:HAND:VALUE`, `stick:HAND:X:Y`; HAND is `left` or `right`, BUTTON one of `primary`,
/// `secondary`, `thumbstick`, `menu`, `trigger`, `squeeze`, `thumbrest`.
pub fn parse_cue(text: &str) -> Result<Cue, String> {
    let bad = || format!("cue {text:?} is not SECONDS:ACTION (see --help)");
    let mut parts = text.split(':');
    let at = parts
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .ok_or_else(bad)?;
    let words: Vec<&str> = parts.collect();
    let number = |s: &str| s.parse::<f32>().ok().filter(|v| v.is_finite());
    let millis = |s: &str| s.parse::<u64>().ok().map(Duration::from_millis);
    let hand = |s: &str| match s {
        "left" => Some(Hand::Left),
        "right" => Some(Hand::Right),
        _ => None,
    };
    let action = match words.as_slice() {
        ["recenter"] => Some(Action::Recenter),
        ["keyframe"] => Some(Action::RequestKeyframe),
        ["focus"] => Some(Action::Focus(true)),
        ["unfocus"] => Some(Action::Focus(false)),
        ["pause", ms] => millis(ms).map(Action::PauseReading),
        ["press", h, b, rest @ ..] if rest.len() <= 1 => {
            let hold = rest
                .first()
                .map_or(Some(Duration::from_millis(100)), |ms| millis(ms));
            match (hand(h), button_named(b), hold) {
                (Some(hand), Some(button), Some(hold)) => {
                    Some(Action::Press { hand, button, hold })
                }
                _ => None,
            }
        }
        ["trigger", h, v] => hand(h)
            .zip(number(v))
            .map(|(hand, value)| Action::Trigger { hand, value }),
        ["squeeze", h, v] => hand(h)
            .zip(number(v))
            .map(|(hand, value)| Action::Squeeze { hand, value }),
        ["stick", h, x, y] => match (hand(h), number(x), number(y)) {
            (Some(hand), Some(x), Some(y)) => Some(Action::Thumbstick { hand, x, y }),
            _ => None,
        },
        _ => None,
    };
    Ok(Cue {
        at: Duration::from_secs_f64(at),
        action: action.ok_or_else(bad)?,
    })
}

fn button_named(name: &str) -> Option<u32> {
    Some(match name {
        "primary" => button::PRIMARY,
        "secondary" => button::SECONDARY,
        "thumbstick" => button::THUMBSTICK,
        "menu" => button::MENU,
        "trigger" => button::TRIGGER,
        "squeeze" => button::SQUEEZE,
        "thumbrest" => button::THUMBREST,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(q: [f32; 4]) -> bool {
        (q.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5
    }

    #[test]
    fn every_pose_is_a_rigid_transform_and_moves_smoothly() {
        let script = Script::default();
        for i in 0..2000 {
            let t = i as f64 * 0.011;
            let s = script.sample(t);
            assert!(unit(s.head.orientation), "head at {t}");
            for c in &s.controllers {
                assert!(c.active && unit(c.grip.orientation) && unit(c.aim.orientation));
            }
            let speed = s
                .head_velocity
                .linear
                .iter()
                .map(|v| v * v)
                .sum::<f32>()
                .sqrt();
            assert!(
                speed < 0.5,
                "a person's head, not a projectile: {speed} m/s"
            );
        }
    }

    #[test]
    fn velocity_predicts_the_next_sample() {
        let script = Script::default();
        let (a, b) = (script.sample(3.0), script.sample(3.011));
        for i in 0..3 {
            let predicted = a.head.position[i] + a.head_velocity.linear[i] * 0.011;
            assert!((predicted - b.head.position[i]).abs() < 1e-4);
        }
    }

    #[test]
    fn still_does_not_move() {
        let script = Script {
            motion: Motion::Still,
            cues: Vec::new(),
        };
        let (a, b) = (script.sample(0.0), script.sample(7.5));
        assert_eq!(a.head, b.head);
        assert_eq!(a.controllers, b.controllers);
        assert_eq!(a.head_velocity, Velocity::default());
    }

    #[test]
    fn eyes_straddle_the_head() {
        let [left, right] = eyes();
        assert!(left.pose.position[0] < 0.0 && right.pose.position[0] > 0.0);
        assert_eq!(left.fov.left, -right.fov.right);
    }

    #[test]
    fn presses_release_on_time_and_analog_values_set_their_bits() {
        let mut input = HandInput::default();
        input.press(button::PRIMARY | button::MENU, 1.0);
        input.squeeze = 1.0;
        let mut state = ControllerState::default();
        input.apply(0.5, &mut state);
        assert_eq!(
            state.buttons,
            button::PRIMARY | button::MENU | button::SQUEEZE
        );
        assert_eq!(state.touches, button::PRIMARY);
        input.apply(1.0, &mut state);
        assert_eq!(state.buttons, button::SQUEEZE);
    }

    #[test]
    fn cues_parse_from_the_command_line_form() {
        let cue = |s| parse_cue(s).unwrap();
        assert_eq!(
            cue("2.5:press:left:menu"),
            Cue {
                at: Duration::from_millis(2500),
                action: Action::Press {
                    hand: Hand::Left,
                    button: button::MENU,
                    hold: Duration::from_millis(100)
                }
            }
        );
        assert_eq!(
            cue("1:squeeze:right:1").action,
            Action::Squeeze {
                hand: Hand::Right,
                value: 1.0
            }
        );
        assert_eq!(
            cue("0:stick:left:-0.5:0.25").action,
            Action::Thumbstick {
                hand: Hand::Left,
                x: -0.5,
                y: 0.25
            }
        );
        assert_eq!(
            cue("3:pause:500").action,
            Action::PauseReading(Duration::from_millis(500))
        );
        assert_eq!(cue("4:unfocus").action, Action::Focus(false));
        assert_eq!(cue("4:keyframe").action, Action::RequestKeyframe);
        for bad in [
            "",
            "x:recenter",
            "-1:recenter",
            "1:press:up:menu",
            "1:press:left:menu:x",
            "1:jump",
        ] {
            assert!(parse_cue(bad).is_err(), "{bad:?}");
        }
    }
}
