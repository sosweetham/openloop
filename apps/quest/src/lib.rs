//! Tether's headset client for Meta Quest. See `docs/ARCHITECTURE.md` § Quest client.
#![cfg(target_os = "android")]

mod decoder;
mod egl;
mod headset;
mod net;
mod render;
mod video;

use std::error::Error;
use std::time::Duration;

use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("Tether"),
    );
    if let Err(e) = run(&app) {
        log::error!("{e}");
    }
}

fn run(app: &AndroidApp) -> Result<(), Box<dyn Error>> {
    // Meta's runtime registers an activity-lifecycle listener inside xrCreateSession and only
    // reports the activity ready (which ends the shell's launch transition) when it then sees
    // the activity resume. The session must therefore exist before the first event is pumped:
    // the activity cannot resume until this thread acknowledges the lifecycle events.
    let mut headset = headset::Headset::new(app.activity_as_ptr())?;
    let mut destroyed = false;
    while !destroyed {
        let timeout = if headset.is_running() { Duration::ZERO } else { Duration::from_millis(100) };
        app.poll_events(Some(timeout), |event| {
            if let PollEvent::Main(event) = event {
                if let MainEvent::Destroy = event {
                    destroyed = true;
                }
                log::info!("activity {event:?}");
            }
        });
        // NativeActivity must acknowledge every input event or Android declares the app
        // unresponsive; VR input arrives through OpenXR instead.
        if let Ok(mut inputs) = app.input_events_iter() {
            while inputs.next(|_| InputStatus::Unhandled) {}
        }
        if !headset.poll()? {
            break;
        }
        if headset.is_running() {
            headset.frame()?;
        }
    }
    Ok(())
}
