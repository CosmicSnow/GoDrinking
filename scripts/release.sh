#!/usr/bin/env bash
# Local release: web build (first) + macOS .dmg + Windows exe (xwin cross) + gh release upload.
#
# Usage:
#   bash scripts/release.sh [--skip-macos] [--skip-windows] [--skip-upload]
#
# Artifacts:
#   - macOS: app/target/release/bundle/dmg/*.dmg  (via `cargo tauri build --bundles dmg` in app/)
#   - Windows: app/target/x86_64-pc-windows-msvc/release/goDrinking.exe
#     (via `cargo xwin build --target x86_64-pc-windows-msvc --release
#      --features tauri/custom-protocol --bin goDrinking` in app/)
#
# Notes (AGENTS.md is law):
#   - Web build ALWAYS FIRST (`npm run build` in app/web/ -> web/dist).
#   - Builds outside the Tauri CLI need --features tauri/custom-protocol (Windows xwin path).
#   - No secrets are logged; only TAG, paths, and sizes are echoed.
set -euo pipefail

SKIP_MACOS=false
SKIP_WINDOWS=false
SKIP_UPLOAD=false

for arg in "$@"; do
  case "$arg" in
    --skip-macos) SKIP_MACOS=true ;;
    --skip-windows) SKIP_WINDOWS=true ;;
    --skip-upload) SKIP_UPLOAD=true ;;
    -h|--help)
      echo "Usage: bash scripts/release.sh [--skip-macos] [--skip-windows] [--skip-upload]"
      exit 0
      ;;
    *)
      echo "release: unknown flag: $arg (expected --skip-macos, --skip-windows, --skip-upload)" >&2
      exit 1
      ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(cd "$ROOT" && node -p "require('./package.json').version")"
TAG="v$VERSION"

echo "release: ROOT=$ROOT TAG=$TAG"

# --- deps ---
for cmd in node npm cargo gh; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "release: missing required tool: $cmd" >&2; exit 1; }
done
gh auth status || { echo "release: 'gh auth status' failed (run gh auth login)" >&2; exit 1; }

# --- web (always first) ---
echo "release: building web (app/web)..."
npm --prefix "$ROOT/app/web" run build

# --- macOS .dmg ---
DMG=""
if [ "$SKIP_MACOS" = true ]; then
  echo "release: skipping macOS build (--skip-macos)"
else
  echo "release: building macOS .dmg..."
  (cd "$ROOT/app" && cargo tauri build --bundles dmg)
  shopt -s nullglob
  DMGS=("$ROOT"/app/target/release/bundle/dmg/*.dmg)
  shopt -u nullglob
  if [ "${#DMGS[@]}" -eq 0 ]; then
    echo "release: macOS .dmg not found in app/target/release/bundle/dmg/*.dmg" >&2
    exit 1
  fi
  DMG="${DMGS[0]}"
  echo "release: dmg=$DMG"
  ls -lh "$DMG"
fi

# --- Windows exe (cross from macOS host) ---
EXE="$ROOT/app/target/x86_64-pc-windows-msvc/release/goDrinking.exe"
if [ "$SKIP_WINDOWS" = true ]; then
  echo "release: skipping Windows build (--skip-windows)"
else
  echo "release: building Windows goDrinking.exe (xwin cross)..."
  if ! (cd "$ROOT/app" && cargo xwin build --target x86_64-pc-windows-msvc --release --features tauri/custom-protocol --bin goDrinking); then
    echo "release: xwin build failed; requires cargo-xwin plus MSVC target deps (not installed automatically)" >&2
    exit 1
  fi
  if [ ! -f "$EXE" ]; then
    echo "release: Windows exe not found at app/target/x86_64-pc-windows-msvc/release/goDrinking.exe" >&2
    exit 1
  fi
  echo "release: exe=$EXE"
  ls -lh "$EXE"
fi

# --- publish ---
if [ "$SKIP_UPLOAD" = true ]; then
  echo "release: skipping upload (--skip-upload)"
  exit 0
fi

ARTIFACTS=()
if [ -n "$DMG" ] && [ -f "$DMG" ]; then ARTIFACTS+=("$DMG"); fi
if [ -f "$EXE" ]; then ARTIFACTS+=("$EXE"); fi
if [ "${#ARTIFACTS[@]}" -eq 0 ]; then
  echo "release: nothing to upload (both builds skipped?)" >&2
  exit 1
fi

if git -C "$ROOT" rev-parse "$TAG" >/dev/null 2>&1; then
  echo "release: tag $TAG already exists locally"
else
  echo "release: creating tag $TAG from HEAD"
  git -C "$ROOT" tag "$TAG"
fi
git -C "$ROOT" push origin "$TAG" || echo "release: note: git push origin $TAG failed (may already exist upstream), continuing"

if (cd "$ROOT" && gh release view "$TAG" >/dev/null 2>&1); then
  echo "release: gh release $TAG already exists"
else
  echo "release: creating gh release $TAG"
  (cd "$ROOT" && gh release create "$TAG" --title "$TAG" --generate-notes)
fi

echo "release: uploading ${#ARTIFACTS[@]} artifact(s) to $TAG"
(cd "$ROOT" && gh release upload "$TAG" "${ARTIFACTS[@]}" --clobber)
echo "release: done TAG=$TAG"
for f in "${ARTIFACTS[@]}"; do ls -lh "$f"; done
