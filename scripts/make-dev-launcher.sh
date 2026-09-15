#!/usr/bin/env bash
#
# Create ./Trove Dev.app — a double-clickable launcher for the live-reload
# dev build (it just runs scripts/dev.sh with the right PATH). No terminal
# needed: double-click, the Tauri dev window opens in about a minute, edit
# code, the window hot-reloads. Close the window and everything exits.
#
# The launcher is machine-specific and gitignored; rerun this to regenerate it.
# Output goes to ~/Library/Logs/trove/dev.log.
set -euo pipefail
cd "$(dirname "$0")/.."

APP="Trove Dev.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp src-tauri/icons/icon.icns "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Trove Dev</string>
  <key>CFBundleDisplayName</key><string>Trove Dev</string>
  <key>CFBundleIdentifier</key><string>com.davidwills.trove-dev-launcher</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>trove-dev</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>LSUIElement</key><true/>
  <key>LSArchitecturePriority</key><array><string>arm64</string></array>
  <key>LSRequiresNativeExecution</key><true/>
</dict>
</plist>
PLIST

cat > "$APP/Contents/MacOS/trove-dev" <<'SH'
#!/bin/bash
# Launcher for the Trove dev build. Finder gives us a bare environment, so set
# up PATH for node/npm and cargo ourselves. Repo root is three levels up from
# this file; fall back to the canonical checkout if the launcher was moved.
# Finder may start script bundles under Rosetta, which makes universal node
# load the x86_64 tauri CLI binding; force the native arch on Apple Silicon.
if [[ "$(uname -m)" == "x86_64" ]] && arch -arm64 true 2>/dev/null; then
  exec arch -arm64 "$0" "$@"
fi
export PATH="/usr/local/bin:/opt/homebrew/bin:$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin"
source "$HOME/.cargo/env" 2>/dev/null || true
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
[[ -x "$ROOT/scripts/dev.sh" ]] || ROOT="$HOME/Local/trove"
mkdir -p "$HOME/Library/Logs/trove"
cd "$ROOT"
exec scripts/dev.sh >> "$HOME/Library/Logs/trove/dev.log" 2>&1
SH
chmod +x "$APP/Contents/MacOS/trove-dev"
codesign --force --sign - "$APP" >/dev/null 2>&1 || true
echo "==> created ./$APP (logs: ~/Library/Logs/trove/dev.log)"
