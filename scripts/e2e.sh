#!/usr/bin/env bash
# End-to-end check: Khronos's hello_xr renders on this checkout's runtime while
# openloop-fake-headset stands in for the Quest on a loopback port the kernel picks.
#   in-process: the headset link runs inside hello_xr (OPENLOOP_INPROCESS=1).
#   openloopd:    this checkout's openloopd runs under a throwaway launchd label and Mach
#               service, with a synthetic tone for each app's sound; one hello_xr is
#               killed mid-stream and a second takes over the same headset link.
# Fails unless the video arrives at the headset's refresh rate, every stream starts with
# a keyframe and has no gaps or errors; in openloopd mode also unless the tone arrives
# intact in continuous packets, spaced by their frames and due about the pipeline latency
# after capture. Never touches adb, port 47800, the installed openloopd, Core Audio or a
# running app. See docs/ARCHITECTURE.md § Testing.
#   scripts/e2e.sh [--release] [--inprocess | --openloopd] [--seconds <n>] [--keep]
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
profile=dev
out_dir=debug
modes=(inprocess openloopd)
args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --release) profile=release out_dir=release ;;
    --inprocess) modes=(inprocess) ;;
    --openloopd) modes=(openloopd) ;;
    *) args+=("$1") ;;
  esac
  shift
done

export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
target="$(mkdir -p "${CARGO_TARGET_DIR:-$root/target}" && cd "${CARGO_TARGET_DIR:-$root/target}" && pwd)"

hello_xr="$root/build/openxr-sdk/src/tests/hello_xr/hello_xr"
if [[ ! -x "$hello_xr" ]]; then
  "$root/scripts/openxr-sdk.sh" >&2
fi

# shellcheck source=signing.sh
. "$root/scripts/signing.sh"
openloop_team
cargo build --manifest-path "$root/Cargo.toml" --profile "$profile" \
  -p openloop-runtime -p openloop-fake-headset -p openloop-e2e -p openloopd >&2

e2e=("$target/$out_dir/openloop-e2e"
  --runtime "$target/$out_dir/libopenloop_runtime.dylib"
  --fake-headset "$target/$out_dir/openloop-fake-headset"
  --app "$hello_xr")

status=0
for mode in "${modes[@]}"; do
  case "$mode" in
    inprocess) "${e2e[@]}" ${args[@]+"${args[@]}"} || status=1 ;;
    openloopd)
      # The runtime talks only to a openloopd signed as openloopd.
      signed="$target/$out_dir/e2e/openloopd"
      "$root/scripts/openloopd.sh" sign "$target/$out_dir/openloopd" "$signed"
      "${e2e[@]}" --openloopd "$signed" ${args[@]+"${args[@]}"} || status=1
      ;;
  esac
done
exit "$status"
