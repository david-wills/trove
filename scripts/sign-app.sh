#!/usr/bin/env bash
#
# Code-sign the Trove GUI app binary with a stable identity so macOS TCC
# (Full Disk Access / Screen Recording) grants survive rebuilds.
#
# The app is built by `npm run tauri dev` / `tauri build`, which re-sign the
# binary ad-hoc every time — that makes the TCC designated requirement a bare
# cdhash that changes each build, revoking the grant. This re-signs with a
# stable identity + the app's bundle identifier so the grant sticks.
#
# Run this AFTER a Rust rebuild of the app, then restart the app so the running
# process is the signed one. (Frontend-only changes don't rebuild the binary,
# so you only need this when src-tauri / crates change.)
#
#   scripts/sign-app.sh
#
# See also scripts/build-troved.sh (same idea for the headless daemon) and the
# repo memory note "troved build signing".
set -euo pipefail
cd "$(dirname "$0")/.."

# Must match src-tauri/tauri.conf.json "identifier".
SIGN_ID="${TROVED_SIGN_ID:-}"
if [[ -z "$SIGN_ID" ]]; then
  echo "TROVED_SIGN_ID is not set. Export the SHA-1 of a stable code-signing identity" >&2
  echo "(list yours with: security find-identity -v -p codesigning)." >&2
  exit 1
fi
IDENTIFIER="com.davidwills.trove"

signed_any=0
for bin in target/debug/trove-app target/release/trove-app; do
  [[ -f "$bin" ]] || continue
  echo "==> signing $bin with $IDENTIFIER"
  codesign --force --sign "$SIGN_ID" --identifier "$IDENTIFIER" "$bin"
  codesign -d -r- "$bin" 2>&1 | grep -q "certificate leaf" \
    && echo "    OK — identity-based requirement (TCC grant survives rebuilds)" \
    || { echo "    WARNING: requirement is not identity-based"; exit 1; }
  signed_any=1
done

if [[ "$signed_any" == 0 ]]; then
  echo "no trove-app binary found — build it first (npm run tauri dev / tauri build)"
  exit 1
fi

echo "==> done — restart the app so the running process is the signed binary"
