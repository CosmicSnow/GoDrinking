#!/usr/bin/env bash
# Run the built macOS release app with opt-in numeric media diagnostics.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/app/target/release/bundle/macos/goDrinking.app/Contents/MacOS/goDrinking}"
TRACE_DIR="${GOLIVE_TRACE_DIR:-$ROOT/e2e-artifacts/media-trace}"
if [[ ! -x "$BIN" ]]; then
  echo "Build the frontend, then run cargo tauri build in app/ first." >&2
  exit 1
fi
mkdir -p "$TRACE_DIR"
TRACE_DIR="$(cd "$TRACE_DIR" && pwd)"
echo "Local numeric media trace: $TRACE_DIR"
exec env GOLIVE_TRACE_DIR="$TRACE_DIR" "$BIN"
