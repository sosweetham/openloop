#!/usr/bin/env bash
# Runs an OpenXR app against this checkout's runtime: builds it, makes sure this checkout's
# openloopd is installed and running (scripts/openloopd.sh ensure), writes a manifest into build/
# and points the loader at it. Only the openloopd launch agent is installed.
#   scripts/run.sh [--release] <app> [args...]
# OPENLOOP_INPROCESS=1 runs the headset link inside the app instead, as before openloopd; stop
# openloopd first (scripts/openloopd.sh stop) so the app can take the headset's port.
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
openloop_team
cargo build --manifest-path "$root/Cargo.toml" -p openloop-runtime --profile "$profile" >&2
target="$(cd "${CARGO_TARGET_DIR:-$root/target}" && pwd)"

manifest="$root/build/openloop_runtime.json"
mkdir -p "$root/build"
cat >"$manifest" <<JSON
{
  "file_format_version": "1.0.0",
  "runtime": {
    "name": "OpenLoop",
    "library_path": "$target/$out_dir/libopenloop_runtime.dylib"
  }
}
JSON

if [[ "${OPENLOOP_INPROCESS:-}" != 1 ]]; then
  # openloopd owns the headset, including its adb reverse tunnel.
  "$root/scripts/openloopd.sh" ensure ${release[@]+"${release[@]}"}
  exec env XR_RUNTIME_JSON="$manifest" "$@"
fi

# In-process, the headset reaches the app through `adb reverse` (openloop_protocol::USB_PORT),
# which Android drops whenever USB or adb resets, so it is re-established while the app runs.
# An app moved off that port with OPENLOOP_USB_PORT is isolated from the headset: adb is left alone.
adb="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
port=47800
if [[ -x "$adb" && "${OPENLOOP_USB_PORT:-$port}" == "$port" ]]; then
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
