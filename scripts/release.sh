#!/usr/bin/env bash
# Local release: web build (first) + macOS .dmg + Windows exe (xwin cross) + gh release upload.
#
# Usage:
#   bash scripts/release.sh [--skip-macos] [--skip-windows] [--skip-upload]
#
# Artifacts:
#   - macOS: app/target/release/bundle/dmg/*.dmg  (via `cargo tauri build --bundles dmg` in app/)
#   - Windows: goDrinking.exe built natively on windows-latest via
#     .github/workflows/release-windows.yml (triggered below) and uploaded
#     straight to the GitHub release — never downloaded locally.
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

# --- Windows exe (native build on GitHub Actions) ---
# Local `cargo xwin` cross broke on user-level RUSTFLAGS containing spaces
# (cargo-xwin rejects the separator), so the exe is built natively on
# windows-latest via .github/workflows/release-windows.yml, which uploads
# goDrinking.exe straight to the GitHub release. Nothing is downloaded back:
# the publish step below uploads only what exists locally (the .dmg).
# NOTE: the workflow file must already be on main for `gh workflow run --ref main`.
EXE="$ROOT/app/target/x86_64-pc-windows-msvc/release/goDrinking.exe"
if [ "$SKIP_WINDOWS" = true ]; then
  echo "release: skipping Windows build (--skip-windows)"
else
  echo "release: building Windows goDrinking.exe via GitHub Actions..."
  if git -C "$ROOT" rev-parse "$TAG" >/dev/null 2>&1; then
    echo "release: tag $TAG already exists locally"
  else
    echo "release: creating tag $TAG from HEAD"
    git -C "$ROOT" tag "$TAG"
  fi
  git -C "$ROOT" push origin "$TAG" || echo "release: note: git push origin $TAG failed (may already exist upstream), continuing"
  UPLOAD_FLAG=true
  if [ "$SKIP_UPLOAD" = true ]; then UPLOAD_FLAG=false; fi
  (cd "$ROOT" && gh workflow run release-windows.yml --ref main -f tag="$TAG" -f upload="$UPLOAD_FLAG")
  # Locate the run just triggered: snapshot existing ids, then wait for a new one.
  BEFORE="$(cd "$ROOT" && gh run list --workflow release-windows.yml --limit 10 --json databaseId --jq '.[].databaseId' || true)"
  RUN_ID=""
  for _ in $(seq 1 18); do
    sleep 10
    AFTER="$(cd "$ROOT" && gh run list --workflow release-windows.yml --limit 10 --json databaseId --jq '.[].databaseId' || true)"
    RUN_ID="$(comm -13 <(printf '%s\n' "$BEFORE" | sort -n) <(printf '%s\n' "$AFTER" | sort -n) | head -n 1 || true)"
    if [ -n "$RUN_ID" ]; then break; fi
  done
  if [ -z "$RUN_ID" ]; then
    echo "release: could not find the triggered release-windows.yml run" >&2
    exit 1
  fi
  echo "release: watching run $RUN_ID..."
  (cd "$ROOT" && gh run watch "$RUN_ID" --exit-status)
  if [ "$SKIP_UPLOAD" = true ]; then
    echo "release: skipping exe asset check (--skip-upload)"
  else
    ASSETS="$(cd "$ROOT" && gh release view "$TAG" --json assets --jq '.assets[].name' || true)"
    if ! printf '%s\n' "$ASSETS" | grep -qxF 'goDrinking.exe'; then
      echo "release: goDrinking.exe not found on release $TAG" >&2
      exit 1
    fi
    echo "release: goDrinking.exe present on release $TAG"
  fi
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
  # No local exe is produced anymore: the workflow uploads it directly.
  if [ "$SKIP_MACOS" = true ] && [ "$SKIP_WINDOWS" = false ]; then
    echo "release: nothing local to upload (exe was uploaded by the workflow)"
    exit 0
  fi
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
