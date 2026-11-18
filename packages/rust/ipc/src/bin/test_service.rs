//! The `openloopd` stand-in for openloop-ipc's two-process test, run by launchd:
//! `openloop-ipc-test-service <mach service>`.

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;

    use openloop_ipc::control::Listener;
    use openloop_ipc::testing::TestService;

    let Some(name) = std::env::args().nth(1) else {
        eprintln!("usage: openloop-ipc-test-service <mach service>");
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
