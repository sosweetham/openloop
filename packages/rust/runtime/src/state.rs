//! Process-wide runtime state: one registry per handle type and the headset link.

use std::sync::{Arc, LazyLock, OnceLock};

use tether_stream::{Headset, UsbTransport};

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

/// The headset link, started with the first instance and kept for the life of the process.
pub fn headset() -> &'static Arc<Headset> {
    static HEADSET: OnceLock<Arc<Headset>> = OnceLock::new();
    HEADSET.get_or_init(|| match UsbTransport::bind() {
        Ok(transport) => Headset::start(Box::new(transport)),
        Err(e) => {
            crate::log!("USB listener unavailable ({e}); running without a headset");
            Headset::detached()
        }
    })
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[tether] {}", format_args!($($arg)*)) };
}
