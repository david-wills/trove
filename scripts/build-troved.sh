#!/usr/bin/env bash
#
# Build, code-sign, and (re)install the troved daemon.
#
# Why sign? macOS TCC (Full Disk Access / Screen Recording) keys a grant to the
# binary's *designated requirement*. Cargo's default ad-hoc signature makes that
# requirement a bare cdhash, which changes on every build — so every rebuild
# revokes the grant. Signing with a stable identity makes the requirement
# identity-based, so the grant survives rebuilds. Grant the permissions ONCE.
#
# Usage:
#   scripts/build-troved.sh            # build + sign + restart the daemon
#   scripts/build-troved.sh --no-install   # build + sign only
#
set -euo pipefail
cd "$(dirname "$0")/.."

# Stable code-signing identity (SHA-1 of an Apple Development cert in your login
# keychain, passed via TROVED_SIGN_ID) and a stable bundle identifier. The TCC
# grant is tied to these two, not to the binary hash. If the cert is renewed or
# replaced, re-grant permissions once.
SIGN_ID="${TROVED_SIGN_ID:-}"
if [[ -z "$SIGN_ID" ]]; then
  echo "TROVED_SIGN_ID is not set. Export the SHA-1 of a stable code-signing identity" >&2
  echo "(list yours with: security find-identity -v -p codesigning)." >&2
  exit 1
fi
IDENTIFIER="com.davidwills.troved"
BIN="target/release/troved"

echo "==> building troved (release)"
source "$HOME/.cargo/env" 2>/dev/null || true
cargo build --release -p troved

echo "==> code-signing $BIN with $IDENTIFIER"
codesign --force --sign "$SIGN_ID" --identifier "$IDENTIFIER" "$BIN"

echo "==> verifying designated requirement (should be identity-based, not a cdhash)"
codesign -d -r- "$BIN" 2>&1 | grep -q "certificate leaf" \
  && echo "    OK — TCC grant will survive future rebuilds" \
  || { echo "    WARNING: requirement is not identity-based; check the signing identity"; exit 1; }

if [[ "${1:-}" != "--no-install" ]]; then
  echo "==> reinstalling launch agent (restarts on the new build)"
  "$BIN" install
fi

echo "==> done"
