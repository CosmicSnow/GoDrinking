#!/bin/sh
set -eu

root_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root_dir"

version=$(node -p "require('./package.json').version")
tag="v${version}"
repo="CosmicSnow/GoDrinking"
target="x86_64-pc-windows-msvc"

make_portable_exe() {
  exe="$root_dir/src-tauri/target/${target}/release/godrinking.exe"
  if [ ! -f "$exe" ]; then
    exe="$root_dir/src-tauri/target/release/godrinking.exe"
  fi
  if [ ! -f "$exe" ]; then
    exe="$root_dir/target/release/godrinking.exe"
  fi
  if [ ! -f "$exe" ]; then
    echo "no godrinking.exe under src-tauri/target"
    exit 1
  fi
  bundle_dir="$root_dir/src-tauri/target/${target}/release/bundle"
  if [ ! -d "$bundle_dir" ]; then
    bundle_dir="$root_dir/src-tauri/target/release/bundle"
  fi
  portable_exe="$bundle_dir/goDrinking-${version}-windows-portable.exe"
  rm -f "$portable_exe"
  cp "$exe" "$portable_exe"
  echo "Portable exe: $portable_exe"
}

upload_windows_assets() {
  nsis=$(ls "$root_dir/src-tauri/target/${target}/release/bundle/nsis/"*.exe 2>/dev/null | head -1 || true)
  if [ -z "${nsis:-}" ]; then
    nsis=$(ls "$root_dir/src-tauri/target/release/bundle/nsis/"*.exe 2>/dev/null | head -1 || true)
  fi
  msi=$(ls "$root_dir/src-tauri/target/${target}/release/bundle/msi/"*.msi 2>/dev/null | head -1 || true)
  if [ -z "${msi:-}" ]; then
    msi=$(ls "$root_dir/src-tauri/target/release/bundle/msi/"*.msi 2>/dev/null | head -1 || true)
  fi
  if [ -z "${nsis:-}" ] && [ -z "${msi:-}" ]; then
    echo "no Windows installer under src-tauri/target"
    exit 1
  fi
  make_portable_exe
  if ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
    gh release create "$tag" --repo "$repo" --title "goDrinking $tag" --notes "goDrinking $tag"
  fi
  assets=""
  [ -n "${nsis:-}" ] && assets="$assets $nsis"
  [ -n "${msi:-}" ] && assets="$assets $msi"
  [ -n "${portable_exe:-}" ] && assets="$assets $portable_exe"
  # shellcheck disable=SC2086
  gh release upload "$tag" $assets --repo "$repo" --clobber
  echo "Uploaded Windows $tag → https://github.com/${repo}/releases/tag/${tag}"
}

os=$(uname -s 2>/dev/null || echo unknown)

case "$os" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    echo "Building goDrinking ${tag} (Windows native)…"
    npm exec tauri build -- --bundles nsis --ci
    upload_windows_assets
    ;;
  Darwin)
    if ! command -v makensis >/dev/null 2>&1; then
      echo "makensis not found. brew install nsis"
      exit 1
    fi
    if ! command -v cargo-xwin >/dev/null 2>&1 && ! cargo xwin --help >/dev/null 2>&1; then
      echo "cargo-xwin not found. cargo install cargo-xwin"
      exit 1
    fi
    rustup target add "$target" >/dev/null
    echo "Cross-compiling goDrinking ${tag} for ${target} with cargo-xwin + NSIS…"
    # cargo-xwin recreates its clang-cl/lld-link shims on every env call. When
    # a stale shim cannot be replaced (e.g. leftover com.apple.provenance),
    # the call fails; clear the shims so xwin rebuilds them and retry once.
    xwin_env_output=$(cargo xwin env --target "$target" 2>/dev/null) || {
      echo "cargo xwin env failed; clearing stale shims and retrying…"
      rm -f "${HOME}/Library/Caches/cargo-xwin/clang-cl" "${HOME}/Library/Caches/cargo-xwin/lld-link"
      xwin_env_output=$(cargo xwin env --target "$target") || {
        echo "cargo xwin env still failing after shim cleanup"
        exit 1
      }
    }
    eval "$xwin_env_output"
    unset CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER
    export PATH="/opt/homebrew/opt/llvm/bin:${PATH}"
    # Opus's bundled CMakeLists still declares cmake_minimum_required < 3.5.
    export CMAKE_POLICY_VERSION_MINIMUM=3.5
    # clang-cl cross from macOS does not pass SSE4.1 into Opus SIMD files.
    export CFLAGS_x86_64_pc_windows_msvc="${CFLAGS_x86_64_pc_windows_msvc:-} -msse4.1 -mssse3"
    export CXXFLAGS_x86_64_pc_windows_msvc="${CXXFLAGS_x86_64_pc_windows_msvc:-} -msse4.1 -mssse3"
    npm exec tauri build -- --target "$target" --bundles nsis --ci
    upload_windows_assets
    ;;
  *)
    echo "Unsupported host OS: $os"
    exit 1
    ;;
esac
