#!/usr/bin/env bash
# CI drift check: regenerate src/bindings.ts from the Rust commands and fail
# if it differs from what's committed. Regenerate locally with:
#   cargo test -p trove-app export_typescript_bindings
set -euo pipefail
cd "$(dirname "$0")/.."

[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

cargo test -p trove-app export_typescript_bindings --quiet

if ! git diff --exit-code -- src/bindings.ts; then
  echo "" >&2
  echo "src/bindings.ts is out of date with the Rust command definitions." >&2
  echo "Run: cargo test -p trove-app export_typescript_bindings" >&2
  exit 1
fi
echo "src/bindings.ts is up to date."
