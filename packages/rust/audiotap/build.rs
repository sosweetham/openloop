//! Embeds Info.plist in the tool as `__TEXT,__info_plist`, which is how a single-file binary
//! carries a bundle identifier and the NSAudioCaptureUsageDescription that TCC requires before
//! it will prompt (Xcode's CREATE_INFOPLIST_SECTION_IN_BINARY does the same).

use std::env;
use std::path::PathBuf;

fn main() {
    let plist = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo")).join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
