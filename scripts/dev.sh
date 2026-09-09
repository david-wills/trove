#!/usr/bin/env bash
#
# Run `tauri dev` on a per-worktree port so multiple worktrees can run at once.
#
# The dev port lives in two places that must agree: Vite's server.port
# (vite.config.ts) and Tauri's devUrl (tauri.conf.json). Vite reads TROVE_PORT
# directly; we feed the matching devUrl to the Tauri CLI via --config so neither
# file needs a permanent per-worktree edit.
#
# Usage:
#   scripts/dev.sh            # default port 1420
#   TROVE_PORT=1430 scripts/dev.sh
#   scripts/dev.sh 1430       # positional shortcut for the port
#
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${1:-${TROVE_PORT:-1420}}"
export TROVE_PORT="$PORT"

exec npm run tauri dev -- --config "{\"build\":{\"devUrl\":\"http://localhost:$PORT\"}}"
