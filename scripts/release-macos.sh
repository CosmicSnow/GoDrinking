#!/bin/sh
set -eu

root_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root_dir"

version=$(node -p "require('./package.json').version")
tag="v${version}"
repo="CosmicSnow/GoDrinking"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "release:macos must run on a Mac."
  exit 1
fi

echo "Building goDrinking ${tag} (macOS)…"
npm exec tauri build -- --bundles app,dmg --ci

app_dir="$root_dir/src-tauri/target/release/bundle/macos"
app_path="$app_dir/goDrinking.app"
if [ ! -d "$app_path" ]; then
  echo "missing $app_path"
  exit 1
fi

identity=$(security find-identity -p codesigning | awk -F'"' '/goDrinking Dev|GoLive Local Dev/{print $2; exit}')
if [ -z "$identity" ]; then
  identity=$(security find-identity -p codesigning -v | awk -F'"' '/Apple Development: Jouy|Apple Development: jouydurao|Developer ID Application: Jouy/{print $2; exit}')
fi
if [ -z "$identity" ]; then
  identity="-"
fi
if ! codesign --force --deep --sign "$identity" --identifier com.cosmicsnow.godrinking --entitlements "$root_dir/src-tauri/Entitlements.plist" --timestamp=none "$app_path"; then
  echo "Identity '$identity' failed; signing ad-hoc."
  identity="-"
  codesign --force --deep --sign "$identity" --identifier com.cosmicsnow.godrinking --entitlements "$root_dir/src-tauri/Entitlements.plist" --timestamp=none "$app_path"
fi

zip_path="$root_dir/src-tauri/target/release/bundle/goDrinking-${version}-macos-universal.zip"
# Prefer the actual arch in the filename if lipo isn't universal.
arch=$(uname -m)
zip_path="$root_dir/src-tauri/target/release/bundle/goDrinking-${version}-macos-${arch}.zip"
rm -f "$zip_path"
ditto -c -k --keepParent "$app_path" "$zip_path"

ensure_release() {
  if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
    return 0
  fi
  gh release create "$tag" --repo "$repo" --title "goDrinking $tag" --notes "goDrinking $tag"
}

ensure_release

assets="$zip_path"
dmg=$(ls "$root_dir/src-tauri/target/release/bundle/dmg/"*.dmg 2>/dev/null | head -1 || true)
if [ -n "${dmg:-}" ]; then
  assets="$assets $dmg"
fi
# shellcheck disable=SC2086
gh release upload "$tag" $assets --repo "$repo" --clobber

echo "Uploaded macOS $tag → https://github.com/${repo}/releases/tag/${tag}"
echo "Signed with: $identity"
