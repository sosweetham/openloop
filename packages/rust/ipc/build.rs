//! Compiles the NSXPC protocol (objc/TetherIPC.m) with Xcode's clang and SDK.

use std::env;
use std::process::Command;

const SOURCE: &str = "objc/TetherIPC.m";

// Pinned like scripts/openxr-sdk.sh: the Command Line Tools can ship an SDK newer than their own
// linker understands.
const XCODE: &str = "/Applications/Xcode.app/Contents/Developer";

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let developer_dir = env::var("DEVELOPER_DIR").unwrap_or_else(|_| XCODE.to_owned());
    let xcrun = |args: &[&str]| {
        let out = Command::new("xcrun")
            .env("DEVELOPER_DIR", &developer_dir)
            .args(args)
            .output()
            .expect("run xcrun");
        assert!(
            out.status.success(),
            "xcrun {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("xcrun output")
            .trim()
            .to_owned()
    };
    let sdk = xcrun(&["--sdk", "macosx", "--show-sdk-path"]);
    let clang = xcrun(&["--sdk", "macosx", "-f", "clang"]);
    let ar = xcrun(&["--sdk", "macosx", "-f", "ar"]);

    cc::Build::new()
        .file(SOURCE)
        .compiler(&clang)
        .archiver(&ar)
        .env("DEVELOPER_DIR", &developer_dir)
        .env("SDKROOT", &sdk)
        .flag("-fobjc-arc")
        .warnings(true)
        .extra_warnings(true)
        .compile("tether_ipc_objc");

    println!("cargo:rustc-link-lib=framework=Foundation");
}
