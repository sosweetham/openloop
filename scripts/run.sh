#!/usr/bin/env bash
# Runs an OpenXR app against this checkout's runtime: builds it, writes a
# manifest into build/ and points the loader at it. Nothing is installed.
#   scripts/run.sh [--release] <app> [args...]
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
profile=dev
out_dir=debug
if [[ "${1:-}" == "--release" ]]; then
  profile=release
  out_dir=release
  shift
fi

cargo build --manifest-path "$root/Cargo.toml" -p tether-runtime --profile "$profile" >&2

manifest="$root/build/tether_runtime.json"
mkdir -p "$root/build"
cat >"$manifest" <<EOF
{
  "file_format_version": "1.0.0",
  "runtime": {
    "name": "Tether",
    "library_path": "$root/target/$out_dir/libtether_runtime.dylib"
  }
}
EOF

# The headset reaches the runtime through `adb reverse` (tether_protocol::USB_PORT), which
# Android drops whenever USB or adb resets, so it is re-established for as long as the app runs.
adb="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
port=47800
if [[ -x "$adb" ]]; then
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
