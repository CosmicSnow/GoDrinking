#!/bin/sh
set -eu

root_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root_dir"

# Prefer the local goDrinking identity. Never pick an unrelated Apple Development
# cert just because it is the first valid one in the keychain.
identity=$(security find-identity -p codesigning | awk -F'"' '/goDrinking Dev|GoLive Local Dev/{print $2; exit}')
if [ -z "$identity" ]; then
  identity=$(security find-identity -p codesigning -v | awk -F'"' '/Apple Development: Jouy|Apple Development: jouydurao|Developer ID Application: Jouy/{print $2; exit}')
fi
if [ -z "$identity" ]; then
  identity="-"
fi

npm exec tauri build -- --debug --bundles app --no-sign --ci
app_path="$root_dir/src-tauri/target/debug/bundle/macos/goDrinking.app"
if ! codesign --force --deep --sign "$identity" --identifier com.cosmicsnow.godrinking --entitlements "$root_dir/src-tauri/Entitlements.plist" --timestamp=none "$app_path"; then
  echo "Identity '$identity' is not usable; signing ad-hoc so the app can launch."
  identity="-"
  codesign --force --deep --sign "$identity" --identifier com.cosmicsnow.godrinking --entitlements "$root_dir/src-tauri/Entitlements.plist" --timestamp=none "$app_path"
fi
codesign --verify --deep --verbose=2 "$app_path" || true
echo "Signed with: $identity"
