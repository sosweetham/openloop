#!/usr/bin/env bash
# Runs an OpenXR app against this checkout's runtime: builds it, makes sure this checkout's
# tetherd is installed and running (scripts/tetherd.sh ensure), writes a manifest into build/
# and points the loader at it. Only the tetherd launch agent is installed.
#   scripts/run.sh [--release] <app> [args...]
# TETHER_INPROCESS=1 runs the headset link inside the app instead, as before tetherd; stop
# tetherd first (scripts/tetherd.sh stop) so the app can take the headset's port.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
profile=dev
out_dir=debug
release=()
if [[ "${1:-}" == "--release" ]]; then
  profile=release
  out_dir=release
  release=(--release)
  shift
fi

export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
# shellcheck source=signing.sh
. "$root/scripts/signing.sh"
tether_team
cargo build --manifest-path "$root/Cargo.toml" -p tether-runtime --profile "$profile" >&2
target="$(cd "${CARGO_TARGET_DIR:-$root/target}" && pwd)"

manifest="$root/build/tether_runtime.json"
mkdir -p "$root/build"
cat >"$manifest" <<JSON
{
  "file_format_version": "1.0.0",
  "runtime": {
    "name": "Tether",
    "library_path": "$target/$out_dir/libtether_runtime.dylib"
  }
}
JSON

if [[ "${TETHER_INPROCESS:-}" != 1 ]]; then
  # tetherd owns the headset, including its adb reverse tunnel.
  "$root/scripts/tetherd.sh" ensure ${release[@]+"${release[@]}"}
  exec env XR_RUNTIME_JSON="$manifest" "$@"
fi

# In-process, the headset reaches the app through `adb reverse` (tether_protocol::USB_PORT),
# which Android drops whenever USB or adb resets, so it is re-established while the app runs.
# An app moved off that port with TETHER_USB_PORT is isolated from the headset: adb is left alone.
adb="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
port=47800
if [[ -x "$adb" && "${TETHER_USB_PORT:-$port}" == "$port" ]]; then
  (
    while true; do
      "$adb" reverse --list 2>/dev/null | grep -q "tcp:$port tcp:$port" ||
        "$adb" reverse "tcp:$port" "tcp:$port" >/dev/null 2>&1 || true
      sleep 2
    done
  ) &
  tunnel=$!
  trap 'kill "$tunnel" 2>/dev/null' EXIT
fi

XR_RUNTIME_JSON="$manifest" "$@"
