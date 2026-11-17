//! Embeds Info.plist as `__TEXT,__info_plist`: a single-file agent's bundle identifier, and the
//! NSAudioCaptureUsageDescription TCC requires before it prompts for audio capture. The code
//! signature then covers it (docs/ARCHITECTURE.md § Identity).

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let plist = manifest.join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
