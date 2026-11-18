//! OpenLoop's headset client for Meta Quest. See `docs/ARCHITECTURE.md` § Quest client.
#![cfg(target_os = "android")]

mod aaudio;
mod audio;
mod decoder;
mod egl;
mod headset;
mod menu;
mod net;
mod panel;
mod passthrough;
mod render;
mod ui;
mod video;

use std::error::Error;
use std::time::Duration;

use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("OpenLoop"),
    );
    if let Err(e) = run(&app) {
        log::error!("{e}");
    }
}

fn run(app: &AndroidApp) -> Result<(), Box<dyn Error>> {
    let mut headset = headset::Headset::new(app.activity_as_ptr())?;
    let mut destroyed = false;
    while !destroyed {
        let timeout = if headset.is_running() {
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        };
        app.poll_events(Some(timeout), |event| {
            if let PollEvent::Main(MainEvent::Destroy) = event {
                destroyed = true;
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
