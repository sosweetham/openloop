#!/usr/bin/env bash
# Builds tether-audiotap, signs it as tetherd, and runs it ONCE as a one-shot launchd job, so
# TCC holds the tool itself responsible for the capture. A process started from a shell is
# attributed to the shell's app, which would then get the prompt and the grant.
#
#   run-once.sh build                  build, sign and verify; runs nothing
#   run-once.sh permission [options]   taps a silent afplay: the System Audio Recording prompt
#   run-once.sh check      [options]   captures a 3 s tone from afplay and measures it
#
#   --prompt-answered   a person has dealt with the prompt; clears a recorded block
#   --ignore-alerts     run even though a system alert window is on screen
#
# A tap operation issued while a capture prompt was pending once wedged coreaudiod, and audio
# for every app on the Mac, for hours. So nothing here retries: a run that sees a stall or no
# audio records a block in build/audiotap/blocked, and later runs refuse until
# --prompt-answered. Never start this from a loop.
set -euo pipefail

usage() {
  echo "usage: $0 build | permission | check [--prompt-answered] [--ignore-alerts]" >&2
  exit 64
}
mode="${1:-}"
[[ "$mode" =~ ^(build|permission|check)$ ]] || usage
shift
prompt_answered=0
ignore_alerts=0
for arg in "$@"; do
  case "$arg" in
    --prompt-answered) prompt_answered=1 ;;
    --ignore-alerts) ignore_alerts=1 ;;
    *) usage ;;
  esac
done
if [[ "$(id -u)" == 0 ]]; then
  echo "run this as yourself, not root: the job belongs in your GUI session" >&2
  exit 1
fi

crate="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$crate/../../.." && pwd)"
out_root="$root/build/audiotap"
bin="$out_root/tether-audiotap"
marker="$out_root/blocked"
label="dev.tether.audiotap.$mode"
domain="gui/$(id -u)"
# shellcheck source=../../../../scripts/signing.sh
. "$root/scripts/signing.sh"
tether_signing || exit 1
identity="$TETHER_SIGN_IDENTITY"
team="$TETHER_TEAM_ID"

# The Command Line Tools can ship an SDK newer than their own linker understands.
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
target="${CARGO_TARGET_DIR:-$root/target}"

mkdir -p "$out_root"
cargo build --manifest-path "$root/Cargo.toml" --release -p tether-audiotap --bin tether-audiotap >&2

# TCC keys a bare binary's grant on its path plus its code requirement (identifier and signing
# certificate), so the signed copy always lands at the same path. Sign a copy and rename it into
# place: rewriting a signed binary that is running would kill it.
cp "$target/release/tether-audiotap" "$bin.new"
codesign --force --sign "$identity" --identifier dev.tether.tetherd --options runtime \
  --entitlements "$crate/audiotap.entitlements" --timestamp=none "$bin.new"
codesign --verify --strict "$bin.new"
details="$(codesign -dvv "$bin.new" 2>&1)"
grep -qx 'Identifier=dev.tether.tetherd' <<<"$details" || { echo "wrong signing identifier" >&2; exit 1; }
grep -qx "TeamIdentifier=$team" <<<"$details" || { echo "not signed by team $team" >&2; exit 1; }
grep -q '^Info.plist entries=' <<<"$details" || { echo "Info.plist is not bound to the signature" >&2; exit 1; }
embedded="$(launchctl plist __TEXT,__info_plist "$bin.new")"
grep -q '"CFBundleIdentifier" = "dev.tether.tetherd"' <<<"$embedded" || { echo "embedded Info.plist lacks the bundle identifier" >&2; exit 1; }
grep -q 'NSAudioCaptureUsageDescription' <<<"$embedded" || { echo "embedded Info.plist lacks NSAudioCaptureUsageDescription" >&2; exit 1; }
codesign -d --entitlements - --xml "$bin.new" 2>/dev/null | grep -q 'com.apple.security.device.audio-input' \
  || { echo "missing the audio-input entitlement" >&2; exit 1; }
mv -f "$bin.new" "$bin"

if [[ "$mode" == build ]]; then
  grep -E '^(Identifier|Format|CodeDirectory|Authority=Apple Development|TeamIdentifier|Info.plist)' <<<"$details"
  echo "signed: $bin"
  exit 0
fi

if [[ -e "$marker" ]]; then
  if (( prompt_answered )); then
    echo "clearing the block an earlier run recorded: $(cat "$marker")"
    rm -f "$marker"
  else
    {
      echo "refusing: an earlier run recorded a capture block:"
      sed 's/^/  /' "$marker"
      echo "Answer any System Audio Recording prompt on screen, check System Settings >"
      echo "Privacy & Security > Screen & System Audio Recording, then rerun with --prompt-answered."
    } >&2
    exit 2
  fi
fi
for other in dev.tether.audiotap.permission dev.tether.audiotap.check; do
  if launchctl print "$domain/$other" >/dev/null 2>&1; then
    echo "refusing: $other is still loaded" >&2
    exit 1
  fi
done
if pgrep -f "$bin" >/dev/null; then
  echo "refusing: tether-audiotap is already running" >&2
  exit 1
fi
# A pending prompt, for any app, makes every new IOProc registration queue inside coreaudiod.
alerts="$(osascript -l JavaScript -e '
ObjC.import("CoreGraphics");
const windows = ObjC.castRefToObject($.CGWindowListCopyWindowInfo(1, 0));
let alerts = 0;
for (let i = 0; i < windows.count; i++) {
  if (ObjC.unwrap(windows.objectAtIndex(i).objectForKey("kCGWindowOwnerName")) === "UserNotificationCenter") alerts++;
}
alerts;' 2>/dev/null || echo unknown)"
if [[ "$alerts" != 0 ]] && (( ! ignore_alerts )); then
  echo "refusing: a system alert window is on screen (UserNotificationCenter: $alerts) and may be a" >&2
  echo "pending permission prompt. Answer it first, or pass --ignore-alerts if it is unrelated." >&2
  exit 2
fi

run_dir="$out_root/runs/$mode-$(date +%Y%m%d-%H%M%S)"
case "$bin$run_dir$marker" in *[\&\<\>]*) echo "paths with & < > are not supported" >&2; exit 1 ;; esac
mkdir -p "$run_dir"
plist="$run_dir/$label.plist"
cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$label</string>
	<key>ProgramArguments</key>
	<array>
		<string>$bin</string>
		<string>$mode</string>
		<string>--out</string><string>$run_dir</string>
		<string>--marker</string><string>$marker</string>
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
plutil -lint "$plist" >/dev/null

job_field() {
  launchctl print "$domain/$label" 2>/dev/null | awk -F' = ' -v key="$1" '$1 == "\t" key { print $2; exit }'
}
tailer=""
cleanup() {
  [[ -n "$tailer" ]] && kill "$tailer" 2>/dev/null || true
  launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
  rm -f "$plist"
}
trap cleanup EXIT

if [[ "$mode" == permission ]]; then
  echo "A System Audio Recording prompt for \"tether-audiotap\" should appear on this Mac within"
  echo "seconds. Click Allow. (The tool waits for the answer; if none comes it stops without retrying.)"
fi
: >"$run_dir/stdout.log"
tail -n +1 -f "$run_dir/stdout.log" &
tailer=$!
launchctl bootstrap "$domain" "$plist"

if [[ "$mode" == permission ]]; then limit=300; else limit=120; fi
deadline=$(( $(date +%s) + limit ))
state=""
while :; do
  state="$(job_field state)"
  runs="$(job_field runs)"
  if [[ "${runs:-0}" -ge 1 && "$state" == "not running" ]]; then
    break
  fi
  if (( $(date +%s) >= deadline )); then
    break
  fi
  sleep 0.5
done
if [[ "$state" != "not running" ]]; then
  # A tool still running this long is inside a HAL call that has not returned, which is what
  # a pending prompt looks like. SIGTERM's default action ends it at once, with no teardown
  # and so no further HAL call; coreaudiod reclaims a dead client's private tap and device.
  echo "still running after ${limit}s; terminating the job" >&2
  [[ -e "$marker" ]] || echo "run-once.sh terminated a $mode run still inside a HAL call after ${limit}s; a prompt may be pending" >"$marker"
  launchctl kill SIGTERM "$domain/$label" >/dev/null 2>&1 || true
  for _ in $(seq 20); do
    [[ "$(job_field state)" == "not running" ]] && break
    sleep 0.5
  done
  [[ "$(job_field state)" == "not running" ]] || launchctl kill SIGKILL "$domain/$label" >/dev/null 2>&1 || true
fi
code="$(job_field "last exit code")"
[[ "$code" =~ ^[0-9]+$ ]] || code=1
sleep 0.2
cleanup
trap - EXIT

if [[ -s "$run_dir/stderr.log" ]]; then
  echo "--- stderr"
  cat "$run_dir/stderr.log"
fi
echo "--- $label exited with $code; logs in $run_dir"
case "$code" in
  0)
    [[ "$mode" == permission ]] && echo "Granted. Next: $0 check" || echo "PASS"
    ;;
  2)
    echo "Blocked: no further capture until a person acts (see the OUTCOME line above)."
    echo "Answer any prompt on screen and check System Settings > Privacy & Security >"
    echo "Screen & System Audio Recording; then run again with --prompt-answered."
    ;;
  *)
    echo "Failed; see the log above."
    ;;
esac
exit "$code"
