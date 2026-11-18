# Sourced by Tether's scripts: the certificate that signs tetherd, and its Apple team.
#   TETHER_SIGN_IDENTITY  the "Apple Development: ..." identity to sign with; by default the only one
#                         in the keychain (Xcode, Settings, Accounts, Manage Certificates).
#   TETHER_TEAM_ID        that certificate's team (its subject OU); by default read from it.
# Either may be set in the environment or, to keep a choice, in the per-user file below.
# tether_signing exports both, so cargo builds tether-ipc with the team the runtime requires of
# tetherd (packages/rust/ipc/build.rs). tether_team exports the team only when one can be found, for
# builds that sign nothing.

tether_signing_file="$HOME/Library/Application Support/Tether/signing.env"

tether_signing() {
  if [[ -z "${TETHER_SIGN_IDENTITY:-}" && -f "$tether_signing_file" ]]; then
    # shellcheck source=/dev/null
    . "$tether_signing_file"
  fi
  if [[ -z "${TETHER_SIGN_IDENTITY:-}" ]]; then
    local found count
    found="$(security find-identity -v -p codesigning 2>/dev/null \
      | sed -n 's/^ *[0-9][0-9]*) [0-9A-F]\{40\} "\(Apple Development: .*\)"$/\1/p')"
    count="$(printf '%s' "$found" | grep -c . || true)"
    if [[ "$count" -eq 0 ]]; then
      echo "No Apple Development certificate in the keychain: add one in Xcode (Settings," \
        "Accounts, Manage Certificates) or set TETHER_SIGN_IDENTITY." >&2
      return 1
    elif [[ "$count" -gt 1 ]]; then
      echo "Several Apple Development certificates; set TETHER_SIGN_IDENTITY (or write it to" \
        "$tether_signing_file) as one of:" >&2
      printf '%s\n' "$found" | sed 's/^/  /' >&2
      return 1
    fi
    TETHER_SIGN_IDENTITY="$found"
  fi
  if [[ -z "${TETHER_TEAM_ID:-}" ]]; then
    TETHER_TEAM_ID="$(security find-certificate -c "$TETHER_SIGN_IDENTITY" -p 2>/dev/null \
      | openssl x509 -noout -subject 2>/dev/null \
      | sed -n 's/.*OU *= *\([A-Z0-9]\{10\}\).*/\1/p')"
    if [[ -z "$TETHER_TEAM_ID" ]]; then
      echo "Could not read the team of \"$TETHER_SIGN_IDENTITY\"; set TETHER_TEAM_ID." >&2
      return 1
    fi
  fi
  export TETHER_SIGN_IDENTITY TETHER_TEAM_ID
}

tether_team() {
  [[ -n "${TETHER_TEAM_ID:-}" ]] || tether_signing 2>/dev/null || true
}
