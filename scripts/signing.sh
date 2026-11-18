# Sourced by OpenLoop's scripts: the certificate that signs openloopd, and its Apple team.
#   OPENLOOP_SIGN_IDENTITY  the "Apple Development: ..." identity to sign with; by default the only one
#                         in the keychain (Xcode, Settings, Accounts, Manage Certificates).
#   OPENLOOP_TEAM_ID        that certificate's team (its subject OU); by default read from it.
# Either may be set in the environment or, to keep a choice, in the per-user file below.
# openloop_signing exports both, so cargo builds openloop-ipc with the team the runtime requires of
# openloopd (packages/rust/ipc/build.rs). openloop_team exports the team only when one can be found, for
# builds that sign nothing.

openloop_signing_file="$HOME/Library/Application Support/OpenLoop/signing.env"

openloop_signing() {
  if [[ -z "${OPENLOOP_SIGN_IDENTITY:-}" && -f "$openloop_signing_file" ]]; then
    # shellcheck source=/dev/null
    . "$openloop_signing_file"
  fi
  if [[ -z "${OPENLOOP_SIGN_IDENTITY:-}" ]]; then
    local found count
    found="$(security find-identity -v -p codesigning 2>/dev/null \
      | sed -n 's/^ *[0-9][0-9]*) [0-9A-F]\{40\} "\(Apple Development: .*\)"$/\1/p')"
    count="$(printf '%s' "$found" | grep -c . || true)"
    if [[ "$count" -eq 0 ]]; then
      echo "No Apple Development certificate in the keychain: add one in Xcode (Settings," \
        "Accounts, Manage Certificates) or set OPENLOOP_SIGN_IDENTITY." >&2
      return 1
    elif [[ "$count" -gt 1 ]]; then
      echo "Several Apple Development certificates; set OPENLOOP_SIGN_IDENTITY (or write it to" \
        "$openloop_signing_file) as one of:" >&2
      printf '%s\n' "$found" | sed 's/^/  /' >&2
      return 1
    fi
    OPENLOOP_SIGN_IDENTITY="$found"
  fi
  if [[ -z "${OPENLOOP_TEAM_ID:-}" ]]; then
    OPENLOOP_TEAM_ID="$(security find-certificate -c "$OPENLOOP_SIGN_IDENTITY" -p 2>/dev/null \
      | openssl x509 -noout -subject 2>/dev/null \
      | sed -n 's/.*OU *= *\([A-Z0-9]\{10\}\).*/\1/p')"
    if [[ -z "$OPENLOOP_TEAM_ID" ]]; then
      echo "Could not read the team of \"$OPENLOOP_SIGN_IDENTITY\"; set OPENLOOP_TEAM_ID." >&2
      return 1
    fi
  fi
  export OPENLOOP_SIGN_IDENTITY OPENLOOP_TEAM_ID
}

openloop_team() {
  [[ -n "${OPENLOOP_TEAM_ID:-}" ]] || openloop_signing 2>/dev/null || true
}
