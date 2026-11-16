#!/usr/bin/env bash
# Builds the Quest client and packages it as build/tether-quest.apk with the SDK
# command-line tools (no Gradle). Pass --install to install it on the connected
# headset.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
sdk="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
ndk="${ANDROID_NDK_HOME:-$(ls -d "$sdk"/ndk/* | sort -V | tail -1)}"
build_tools="$sdk/build-tools/$(ls "$sdk/build-tools" | grep -v rc | sort -V | tail -1)"
android_jar="$sdk/platforms/$(ls "$sdk/platforms" | grep -E '^android-[0-9]+$' | sort -V | tail -1)/android.jar"
toolchain="$ndk/toolchains/llvm/prebuilt/darwin-x86_64/bin"

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$toolchain/aarch64-linux-android29-clang"
export CC_aarch64_linux_android="$toolchain/aarch64-linux-android29-clang"
export AR_aarch64_linux_android="$toolchain/llvm-ar"
cargo build --manifest-path "$root/Cargo.toml" -p tether-quest --target aarch64-linux-android --release

loader="$root/build/quest-loader/src/loader/libopenxr_loader.so"
[[ -f "$loader" ]] || "$root/scripts/quest-loader.sh"

stage="$root/build/quest-apk"
rm -rf "$stage"
mkdir -p "$stage/lib/arm64-v8a"
cp "$root/target/aarch64-linux-android/release/libtether_quest.so" "$loader" "$stage/lib/arm64-v8a/"

"$build_tools/aapt2" link -o "$stage/unaligned.apk" --manifest "$root/apps/quest/AndroidManifest.xml" -I "$android_jar"
(cd "$stage" && zip -qr unaligned.apk lib)
"$build_tools/zipalign" -f -p 4 "$stage/unaligned.apk" "$stage/aligned.apk"
"$build_tools/apksigner" sign --ks "$HOME/.android/debug.keystore" --ks-pass pass:android \
  --out "$root/build/tether-quest.apk" "$stage/aligned.apk"
echo "built $root/build/tether-quest.apk"

if [[ "${1:-}" == "--install" ]]; then
  "$sdk/platform-tools/adb" install -r "$root/build/tether-quest.apk"
fi
