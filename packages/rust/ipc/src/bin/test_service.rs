//! The `tetherd` stand-in for tether-ipc's two-process test, run by launchd:
//! `tether-ipc-test-service <mach service>`.

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;

    use tether_ipc::control::Listener;
    use tether_ipc::testing::TestService;

    let Some(name) = std::env::args().nth(1) else {
        eprintln!("usage: tether-ipc-test-service <mach service>");
        std::process::exit(2);
    };
    let _listener = Listener::mach_service(&name, Arc::new(TestService::default()));
    eprintln!("test service {} listening on {name}", std::process::id());
    loop {
        std::thread::park();
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
