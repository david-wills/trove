#!/usr/bin/env bash
#
# Build the Trove GUI as a real double-clickable Mac app, sign it with a stable
# identity, and install it at /Applications/Trove.app and ./Trove.app.
#
# Why sign? macOS TCC (Full Disk Access / Screen Recording / Calendars) keys a
# grant to the binary's designated requirement. `tauri build` signs ad-hoc, so
# the requirement is a bare cdhash that changes every build and the grants get
# revoked. Signing with an Apple Development identity makes the requirement
# identity-based, so you grant once. trove-collector's scripts/build.sh does the same.
#
# Usage:
#   scripts/build-app.sh                 # build + sign + install
#   scripts/build-app.sh --no-install    # build + sign only (leaves target/release/bundle)
#
# Identity: TROVE_SIGN_ID (SHA-1 from `security find-identity -v -p codesigning`;
# TROVED_SIGN_ID is still honoured as the old name).
# If unset, the first "Apple Development" identity in the keychain is used.
# Rebuild cost after the first build is a few minutes — Cargo caches everything
# you didn't touch; frontend-only changes don't recompile Rust at all.
set -euo pipefail
cd "$(dirname "$0")/.."

SIGN_ID="${TROVE_SIGN_ID:-${TROVED_SIGN_ID:-}}"
if [[ -z "$SIGN_ID" ]]; then
  SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
    | grep 'Apple Development' | head -1 | awk '{print $2}')"
  if [[ -z "$SIGN_ID" ]]; then
    echo "No Apple Development signing identity found and TROVE_SIGN_ID is unset." >&2
    echo "List yours with: security find-identity -v -p codesigning" >&2
    exit 1
  fi
  echo "==> using signing identity $SIGN_ID (set TROVE_SIGN_ID to pin one)"
fi

BUNDLE="target/release/bundle/macos/Trove.app"

echo "==> building Trove.app (release)"
source "$HOME/.cargo/env" 2>/dev/null || true
export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"
npm run tauri build -- --bundles app

# The vault MCP server ships inside the bundle (docs/roadmap.md, M1): a
# second binary next to the app's, so `claude mcp add trove -- <path>` points
# at something that survives rebuilds and gets the same signature.
echo "==> building trove-mcp (release)"
cargo build --release -p trove-mcp
ditto target/release/trove-mcp "$BUNDLE/Contents/MacOS/trove-mcp"

echo "==> code-signing $BUNDLE"
codesign --force --deep --sign "$SIGN_ID" "$BUNDLE"
codesign -d -r- "$BUNDLE" 2>&1 | grep -q "certificate leaf" \
  && echo "    OK — identity-based requirement (TCC grants survive rebuilds)" \
  || { echo "    WARNING: requirement is not identity-based; check the signing identity"; exit 1; }

if [[ "${1:-}" == "--no-install" ]]; then
  echo "==> done (not installed): $BUNDLE"
  exit 0
fi

# Replace the installed copies. Quit a running instance first so we don't
# swap the bundle out from under it.
pkill -f '/Trove.app/Contents/MacOS/trove-app' 2>/dev/null || true
sleep 1
for dest in "/Applications/Trove.app" "./Trove.app"; do
  echo "==> installing $dest"
  rm -rf "$dest"
  ditto "$BUNDLE" "$dest"
done

echo "==> done — launch Trove from /Applications or double-click ./Trove.app"
echo "    MCP server: /Applications/Trove.app/Contents/MacOS/trove-mcp"
echo "    register:   claude mcp add trove -- /Applications/Trove.app/Contents/MacOS/trove-mcp"
