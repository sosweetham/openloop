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

XR_RUNTIME_JSON="$manifest" exec "$@"
