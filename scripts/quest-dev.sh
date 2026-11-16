#!/usr/bin/env bash
# Prepares the connected Quest for a USB session: installs build/tether-quest.apk,
# forwards Tether's port (tether_protocol::USB_PORT) through adb. The
# client is then launched from the headset (Library > Unknown Sources > Tether).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
adb="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
port=47800

"$adb" install -r "$root/build/tether-quest.apk"
"$adb" reverse "tcp:$port" "tcp:$port"
