//! Process-wide runtime state: one registry per handle type.

use std::sync::LazyLock;

use crate::actions::{Action, ActionSet};
use crate::handles::Registry;
use crate::instance::Instance;
use crate::session::Session;
use crate::space::Space;
use crate::swapchain::Swapchain;

#[derive(Default)]
pub struct Objects {
    pub instances: Registry<Instance>,
    pub sessions: Registry<Session>,
    pub spaces: Registry<Space>,
    pub swapchains: Registry<Swapchain>,
    pub action_sets: Registry<ActionSet>,
    pub actions: Registry<Action>,
}

pub static OBJECTS: LazyLock<Objects> = LazyLock::new(Objects::default);

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[tether] {}", format_args!($($arg)*)) };
}
