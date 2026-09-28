#!/usr/bin/env bash
# Local release: web build (first) + macOS .dmg + optional Windows exe workflow + gh release upload.
#
# Usage:
#   bash scripts/release.sh [--skip-macos] [--skip-windows] [--skip-upload]
#
# Artifacts:
#   - macOS: app/target/release/bundle/dmg/goDrinking_${VERSION}_aarch64.dmg
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
  HOST_TRIPLE="$(cd "$ROOT/app" && rustc -vV | awk '/^host:/ { print $2 }')"
  if [ "$HOST_TRIPLE" != "aarch64-apple-darwin" ]; then
    echo "release: macOS release requires host target aarch64-apple-darwin (found: ${HOST_TRIPLE:-unknown})" >&2
    exit 1
  fi
  DMG="$ROOT/app/target/release/bundle/dmg/goDrinking_${VERSION}_aarch64.dmg"
  SIDECAR_DIR="$ROOT/app/target/release/sidecars"
  mkdir -p "$SIDECAR_DIR"
  echo "release: building golive-video helper for $HOST_TRIPLE..."
  (cd "$ROOT/app" && cargo build --release --locked --bin golive-video --features video-helper-bin)
  install -m 755 "$ROOT/app/target/release/golive-video" "$SIDECAR_DIR/goDrinking-video-$HOST_TRIPLE"
  # Remove only the exact expected artifact. Never accidentally publish a stale or
  # differently named DMG left by a previous build.
  rm -f "$DMG"
  echo "release: bundling signed DMG with the goDrinking-video sidecar..."
  (cd "$ROOT/app" && cargo tauri build --bundles dmg --config '{"bundle":{"externalBin":["target/release/sidecars/goDrinking-video"]}}')
  if [ ! -f "$DMG" ]; then
    echo "release: expected macOS .dmg was not produced: $DMG" >&2
    exit 1
  fi

  # Inspect the actual mounted app before allowing the artifact to be published.
  MOUNT_POINT="$(mktemp -d "${TMPDIR:-/tmp}/godrinking-release.XXXXXX")"
  MOUNTED=false
  cleanup_dmg_mount() {
    if [ "$MOUNTED" = true ]; then hdiutil detach "$MOUNT_POINT" -quiet || true; fi
    rmdir "$MOUNT_POINT" 2>/dev/null || true
  }
  trap cleanup_dmg_mount EXIT
  hdiutil attach "$DMG" -readonly -nobrowse -mountpoint "$MOUNT_POINT" >/dev/null
  MOUNTED=true
  if [ ! -x "$MOUNT_POINT/goDrinking.app/Contents/MacOS/goDrinking-video" ]; then
    echo "release: mounted DMG is missing executable goDrinking-video sidecar" >&2
    exit 1
  fi
  if find "$MOUNT_POINT" -exec basename {} \; | grep -qi 'golive'; then
    echo "release: mounted DMG contains a basename with forbidden 'golive' text" >&2
    exit 1
  fi
  hdiutil detach "$MOUNT_POINT" -quiet
  MOUNTED=false
  rmdir "$MOUNT_POINT"
  trap - EXIT
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
  # Snapshot existing run ids BEFORE triggering: the new run can appear
  # within seconds, and snapshotting after would miss it entirely.
  BEFORE="$(cd "$ROOT" && gh run list --workflow release-windows.yml --limit 10 --json databaseId --jq '.[].databaseId' || true)"
  (cd "$ROOT" && gh workflow run release-windows.yml --ref main -f tag="$TAG" -f upload="$UPLOAD_FLAG")
  # Locate the run just triggered: wait for an id not in the snapshot.
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

# A mac-only publish must attach to the already-existing release without
# creating or moving a tag. Confirm that the local tag and remote tag resolve
# to precisely the same commit before touching release assets.
MAC_ONLY=false
if [ "$SKIP_WINDOWS" = true ] && [ "$SKIP_MACOS" = false ]; then MAC_ONLY=true; fi
if [ "$MAC_ONLY" = true ]; then
  LOCAL_TAG_COMMIT="$(git -C "$ROOT" rev-parse "$TAG^{commit}" 2>/dev/null || true)"
  REMOTE_TAGS="$(git -C "$ROOT" ls-remote origin "refs/tags/$TAG" "refs/tags/$TAG^{}")"
  REMOTE_TAG_COMMIT="$(printf '%s\n' "$REMOTE_TAGS" | awk -v ref="refs/tags/$TAG^{}" '$2 == ref { print $1 }')"
  if [ -z "$REMOTE_TAG_COMMIT" ]; then
    REMOTE_TAG_COMMIT="$(printf '%s\n' "$REMOTE_TAGS" | awk -v ref="refs/tags/$TAG" '$2 == ref { print $1 }')"
  fi
  if [ -z "$LOCAL_TAG_COMMIT" ] || [ -z "$REMOTE_TAG_COMMIT" ] || [ "$LOCAL_TAG_COMMIT" != "$REMOTE_TAG_COMMIT" ]; then
    echo "release: refusing mac-only publish: local and remote $TAG tags must exist and resolve to the same commit" >&2
    exit 1
  fi
  if [ "${#ARTIFACTS[@]}" -ne 1 ] || [ "${ARTIFACTS[0]}" != "$DMG" ]; then
    echo "release: refusing mac-only publish: only the verified DMG may be uploaded" >&2
    exit 1
  fi
  if ! (cd "$ROOT" && gh release view "$TAG" >/dev/null 2>&1); then
    echo "release: refusing mac-only publish: GitHub release $TAG does not exist" >&2
    exit 1
  fi
  EXISTING_ASSETS="$(cd "$ROOT" && gh release view "$TAG" --json assets --jq '.assets[].name')"
  if printf '%s\n' "$EXISTING_ASSETS" | grep -qxF "$(basename "$DMG")"; then
    echo "release: refusing to overwrite existing DMG asset $(basename "$DMG") on $TAG" >&2
    exit 1
  fi
else
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
fi

echo "release: uploading ${#ARTIFACTS[@]} artifact(s) to $TAG"
if [ "$MAC_ONLY" = true ]; then
  (cd "$ROOT" && gh release upload "$TAG" "${ARTIFACTS[@]}")
else
  (cd "$ROOT" && gh release upload "$TAG" "${ARTIFACTS[@]}" --clobber)
fi
echo "release: done TAG=$TAG"
for f in "${ARTIFACTS[@]}"; do ls -lh "$f"; done
