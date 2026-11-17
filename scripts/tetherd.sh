#!/usr/bin/env bash
# Builds, signs and runs tetherd as a per-user launchd agent (dev.tether.tetherd). Every command
# is safe to repeat. See docs/ARCHITECTURE.md § Runtime ↔ tetherd.
#   scripts/tetherd.sh install [--release]  build, sign, install and (re)start it
#   scripts/tetherd.sh ensure [--release]   the same, but leaves an up-to-date running tetherd
#                                           (and the headset's link) alone; scripts/run.sh uses it
#   scripts/tetherd.sh restart              relaunch it (launchctl kickstart -k)
#   scripts/tetherd.sh stop                 unload it until the next install or ensure
#   scripts/tetherd.sh status               launchd's view of it
#   scripts/tetherd.sh log                  follow its log
#   scripts/tetherd.sh uninstall            unload it and remove what install put in place
#   scripts/tetherd.sh sign <from> <to>     copy a tetherd binary and sign it as tetherd
#   scripts/tetherd.sh audio-permission [--prompt-answered] [--ignore-alerts]
#                                           once, at the Mac: the System Audio Recording prompt
#   scripts/tetherd.sh audio-check [--prompt-answered] [--ignore-alerts]
#                                           capture a 3 s test tone through the installed tetherd
# TETHER_USB_PORT, when set at install, moves its headset listener off 47800 (and adb is then
# left alone); TETHER_AUDIO (tap, synthetic or off) sets where the app's sound comes from.
# TETHER_SIGN_IDENTITY overrides the signing certificate.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

label=dev.tether.tetherd
domain="gui/$(id -u)"
identity="${TETHER_SIGN_IDENTITY:-Apple Development: sohamjaiswal10@gmail.com (69Y69F7X3F)}"
# tether_ipc::TETHERD_REQUIREMENT: what the runtime checks before it talks to tetherd.
requirement='identifier "dev.tether.tetherd" and anchor apple generic and certificate leaf[subject.OU] = "7F2T2WK6DR"'
support="$HOME/Library/Application Support/Tether"
binary="$support/tetherd"
stamp="$support/tetherd.sha256"
plist="$HOME/Library/LaunchAgents/$label.plist"
logs="$HOME/Library/Logs/Tether"
log="$logs/tetherd.log"

say() { echo "tetherd.sh: $*" >&2; }

# Signs a copy, so cargo's own output keeps its linker signature and a running tetherd is never
# replaced by a half-written file. The hardened runtime and the audio-input entitlement match
# what the System Audio Recording permission was researched under (packages/rust/audiotap).
sign() {
  local from="$1" to="$2" tmp="$2.signing"
  mkdir -p "$(dirname "$to")"
  cp "$from" "$tmp"
  codesign --force --sign "$identity" --identifier "$label" --options runtime \
    --entitlements "$root/apps/tetherd/tetherd.entitlements" --timestamp=none "$tmp" 2>/dev/null
  codesign --verify --strict -R="$requirement" "$tmp"
  mv -f "$tmp" "$to"
}

build() {
  local profile=dev out_dir=debug
  if [[ "${1:-}" == "--release" ]]; then
    profile=release
    out_dir=release
  fi
  cargo build --manifest-path "$root/Cargo.toml" -p tetherd --profile "$profile" >&2
  local target
  target="$(cd "${CARGO_TARGET_DIR:-$root/target}" && pwd)"
  built="$target/$out_dir/tetherd"
}

render_plist() {
  local args="    <string>$binary</string>"
  if [[ -n "${TETHER_USB_PORT:-}" ]]; then
    args+=$'\n'"    <string>--usb-port</string><string>$TETHER_USB_PORT</string>"
  fi
  if [[ -n "${TETHER_AUDIO:-}" ]]; then
    args+=$'\n'"    <string>--audio</string><string>$TETHER_AUDIO</string>"
  fi
  local env=""
  if [[ -n "${ANDROID_HOME:-}" ]]; then
    env="  <key>EnvironmentVariables</key><dict><key>ANDROID_HOME</key><string>$ANDROID_HOME</string></dict>"
  fi
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
$args
  </array>
  <key>MachServices</key><dict><key>$label</key><true/></dict>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>ThrottleInterval</key><integer>1</integer>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
$env
  <key>StandardOutPath</key><string>$log</string>
  <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
EOF
}

loaded() { launchctl print "$domain/$label" >/dev/null 2>&1; }

pid() { launchctl print "$domain/$label" 2>/dev/null | awk '/^\tpid = / { print $3; exit }'; }

bootstrap() { launchctl bootstrap "$domain" "$plist"; }

bootout() {
  if loaded; then
    launchctl bootout "$domain/$label"
    # bootout returns before launchd has forgotten the label; bootstrap fails until it has.
    for _ in $(seq 50); do loaded || break; sleep 0.1; done
  fi
}

wait_running() {
  for _ in $(seq 50); do
    if [[ -n "$(pid)" ]]; then
      say "running, pid $(pid); log: $log"
      return 0
    fi
    sleep 0.1
  done
  say "launchd has not started it; see $log and: scripts/tetherd.sh status"
  return 1
}

# install: always restarts. ensure: restarts only for a new binary or plist.
deploy() {
  local mode="$1"
  shift
  build "$@"
  mkdir -p "$support" "$logs" "$(dirname "$plist")"

  local binary_changed=0 plist_changed=0
  local hash
  hash="$(shasum -a 256 "$built" | cut -d' ' -f1)"
  if [[ ! -x "$binary" || "$(cat "$stamp" 2>/dev/null)" != "$hash" ]]; then
    sign "$built" "$binary"
    echo "$hash" >"$stamp"
    binary_changed=1
  fi
  local rendered
  rendered="$(render_plist)"
  if [[ ! -f "$plist" || "$(cat "$plist")" != "$rendered" ]]; then
    printf '%s\n' "$rendered" >"$plist"
    plist_changed=1
  fi

  if ((plist_changed)); then
    bootout
    bootstrap
  elif ! loaded; then
    bootstrap
  elif ((binary_changed)) || [[ "$mode" == install ]]; then
    launchctl kickstart -k "$domain/$label"
  elif [[ -n "$(pid)" ]]; then
    say "up to date, pid $(pid)"
    return 0
  fi
  wait_running
}

# Runs the installed tetherd once as a launchd job, `tetherd audio-permission` or `audio-check`,
# so TCC holds tetherd itself responsible and the grant lands on the binary the agent runs. A tap
# operation issued while a capture prompt was pending once wedged coreaudiod, and audio for every
# app on the Mac, for hours: nothing here retries, and a run that stalls or hears nothing leaves
# the block file that keeps tetherd and later runs away from Core Audio until --prompt-answered.
audio_job() {
  local mode="$1"
  shift
  local prompt_answered=0 ignore_alerts=0 arg
  for arg in "$@"; do
    case "$arg" in
      --prompt-answered) prompt_answered=1 ;;
      --ignore-alerts) ignore_alerts=1 ;;
      *) say "usage: audio-$mode [--prompt-answered] [--ignore-alerts]"; exit 64 ;;
    esac
  done
  local blocked="$support/audio-blocked" job="$label.audio-$mode"
  [[ "$(id -u)" != 0 ]] || { say "run this as yourself, not root"; exit 1; }
  [[ -x "$binary" ]] || { say "tetherd is not installed; run scripts/tetherd.sh install first"; exit 1; }
  if [[ -e "$blocked" ]]; then
    if ((prompt_answered)); then
      say "clearing the block an earlier capture recorded: $(cat "$blocked")"
      rm -f "$blocked"
      if loaded; then
        # Its own block lives on in its memory until it restarts.
        launchctl kickstart -k "$domain/$label"
      fi
    else
      say "refusing: an earlier capture recorded a block: $(cat "$blocked")"
      say "Answer any System Audio Recording prompt on screen, check System Settings > Privacy &"
      say "Security > Screen & System Audio Recording, then rerun with --prompt-answered."
      exit 2
    fi
  fi
  local other
  for other in "$label.audio-permission" "$label.audio-check"; do
    if launchctl print "$domain/$other" >/dev/null 2>&1; then
      say "refusing: $other is still loaded"
      exit 1
    fi
  done
  # A pending prompt, for any app, makes every new IOProc registration queue inside coreaudiod.
  local alerts
  alerts="$(osascript -l JavaScript -e '
ObjC.import("CoreGraphics");
const windows = ObjC.castRefToObject($.CGWindowListCopyWindowInfo(1, 0));
let alerts = 0;
for (let i = 0; i < windows.count; i++) {
  if (ObjC.unwrap(windows.objectAtIndex(i).objectForKey("kCGWindowOwnerName")) === "UserNotificationCenter") alerts++;
}
alerts;' 2>/dev/null || echo unknown)"
  if [[ "$alerts" != 0 ]] && ((!ignore_alerts)); then
    say "refusing: a system alert is on screen (UserNotificationCenter: $alerts) and may be a pending"
    say "permission prompt. Answer it first, or pass --ignore-alerts if it is unrelated."
    exit 2
  fi

  local run_dir
  run_dir="$logs/audio/$mode-$(date +%Y%m%d-%H%M%S)"
  case "$binary$run_dir$support" in *[\&\<\>]*) say "paths with & < > are not supported"; exit 1 ;; esac
  mkdir -p "$run_dir"
  job_plist="$run_dir/$job.plist"
  cat >"$job_plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$job</string>
  <key>ProgramArguments</key>
  <array>
    <string>$binary</string>
    <string>audio-$mode</string>
    <string>--out</string><string>$run_dir</string>
    <string>--audio-state</string><string>$support</string>
  </array>
  <key>WorkingDirectory</key><string>$run_dir</string>
  <key>StandardOutPath</key><string>$run_dir/stdout.log</string>
  <key>StandardErrorPath</key><string>$run_dir/stderr.log</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
EOF
  plutil -lint "$job_plist" >/dev/null

  job_field() {
    launchctl print "$domain/$job" 2>/dev/null | awk -F' = ' -v key="$1" '$1 == "\t" key { print $2; exit }'
  }
  tailer=""
  # shellcheck disable=SC2329 # runs from the EXIT trap
  finish_job() {
    if [[ -n "$tailer" ]]; then kill "$tailer" 2>/dev/null || true; fi
    launchctl bootout "$domain/$job" >/dev/null 2>&1 || true
    rm -f "$job_plist"
  }
  trap finish_job EXIT

  if [[ "$mode" == permission ]]; then
    say "A System Audio Recording prompt for \"tetherd\" should appear on this Mac within seconds."
    say "Click Allow. (tetherd waits for the answer; if none comes it stops without retrying.)"
  fi
  : >"$run_dir/stdout.log"
  tail -n +1 -f "$run_dir/stdout.log" &
  tailer=$!
  launchctl bootstrap "$domain" "$job_plist"

  local limit=120 state="" runs
  [[ "$mode" == permission ]] && limit=300
  local deadline=$(($(date +%s) + limit))
  while :; do
    state="$(job_field state)"
    runs="$(job_field runs)"
    [[ "${runs:-0}" -ge 1 && "$state" == "not running" ]] && break
    (($(date +%s) >= deadline)) && break
    sleep 0.5
  done
  if [[ "$state" != "not running" ]]; then
    # Still inside a HAL call this long is what a pending prompt looks like. SIGTERM's default
    # action ends it with no teardown, so no further HAL call; coreaudiod reclaims a dead
    # client's private tap and aggregate device.
    say "still running after ${limit}s; terminating the job"
    [[ -e "$blocked" ]] ||
      echo "tetherd.sh terminated an audio-$mode run still inside a Core Audio call after ${limit}s; a prompt may be pending" >"$blocked"
    launchctl kill SIGTERM "$domain/$job" >/dev/null 2>&1 || true
    for _ in $(seq 20); do
      [[ "$(job_field state)" == "not running" ]] && break
      sleep 0.5
    done
    [[ "$(job_field state)" == "not running" ]] || launchctl kill SIGKILL "$domain/$job" >/dev/null 2>&1 || true
  fi
  local code
  code="$(job_field "last exit code")"
  [[ "$code" =~ ^[0-9]+$ ]] || code=1
  sleep 0.2
  finish_job
  trap - EXIT

  if [[ -s "$run_dir/stderr.log" ]]; then
    echo "--- stderr"
    cat "$run_dir/stderr.log"
  fi
  echo "--- $job exited with $code; logs in $run_dir"
  case "$code" in
    0)
      if [[ "$mode" == permission ]]; then
        say "Granted: tetherd now plays the app in front in the headset. Next: scripts/tetherd.sh audio-check"
      else
        say "PASS"
      fi
      ;;
    2)
      say "Blocked: nothing touches Core Audio until a person acts (see the OUTCOME line above)."
      say "Answer any prompt on screen, check System Settings > Privacy & Security > Screen & System"
      say "Audio Recording, then run again with --prompt-answered."
      ;;
    *) say "Failed; see the log above." ;;
  esac
  exit "$code"
}

command="${1:-}"
shift || true
case "$command" in
  install) deploy install "$@" ;;
  ensure) deploy ensure "$@" ;;
  restart)
    launchctl kickstart -k "$domain/$label"
    wait_running
    ;;
  stop) bootout ;;
  status) launchctl print "$domain/$label" ;;
  log) tail -n 50 -F "$log" ;;
  uninstall)
    bootout
    rm -f "$plist" "$binary" "$stamp"
    say "removed; logs stay in $logs"
    ;;
  sign)
    [[ $# -eq 2 ]] || { say "usage: sign <from> <to>"; exit 2; }
    sign "$1" "$2"
    ;;
  audio-permission) audio_job permission "$@" ;;
  audio-check) audio_job check "$@" ;;
  *)
    sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
    ;;
esac
