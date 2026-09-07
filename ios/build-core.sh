#!/usr/bin/env bash
# Build the Rust protocol crate for iOS and generate its Swift bindings.
#
#   ./ios/build-core.sh              # both slices (device + simulator)
#   ./ios/build-core.sh --device     # device slice only, for a faster loop
#
# Produces a STATIC ARCHIVE per SDK plus generated Swift — deliberately not an
# `.xcframework` and not an SPM `.binaryTarget`. A `.a` plus plain Swift sources
# needs no binary-target plumbing and stays readable in a diff.
#
# Output (all gitignored — regenerate, never commit):
#
#   ios/LumaCore/Sources/LumaCore/Generated/LumaCore.swift
#   ios/LumaCore/Sources/LumaCoreFFI/include/LumaCoreFFI.h
#   ios/LumaCore/Sources/LumaCoreFFI/include/module.modulemap
#   ios/lib/ios-arm64/libluma_core.a          device
#   ios/lib/ios-arm64-sim/libluma_core.a      simulator
#
# The app links those two from its project.yml (`OTHER_LDFLAGS: -lluma_core`
# plus per-SDK `LIBRARY_SEARCH_PATHS`, both at PROJECT level).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# `xcode-select` frequently points at the Command Line Tools, whose sysroot is
# macOS-only — an iOS cross-compile that reaches a link step then fails with
# "building for 'iOS', but linking in dylib built for 'macOS'". Pinning
# DEVELOPER_DIR at the full Xcode gives cargo the iOS SDKs. An existing value wins.
if [ -z "${DEVELOPER_DIR:-}" ]; then
  SELECTED="$(xcode-select -p 2>/dev/null || true)"
  case "$SELECTED" in
    *Xcode.app*) export DEVELOPER_DIR="$SELECTED" ;;
    *)           export DEVELOPER_DIR="/Applications/Xcode.app/Contents/Developer" ;;
  esac
fi
echo "    DEVELOPER_DIR=$DEVELOPER_DIR"
[ -d "$DEVELOPER_DIR" ] || { echo "no Xcode at $DEVELOPER_DIR — install it or set DEVELOPER_DIR" >&2; exit 1; }

# Match the app's floor.
export IPHONEOS_DEPLOYMENT_TARGET=17.0

LIB=luma_core
SWIFT_GEN="ios/LumaCore/Sources/LumaCore/Generated"
FFI_INCLUDE="ios/LumaCore/Sources/LumaCoreFFI/include"

TARGETS=("aarch64-apple-ios:ios-arm64" "aarch64-apple-ios-sim:ios-arm64-sim")
if [ "${1:-}" = "--device" ]; then
  TARGETS=("aarch64-apple-ios:ios-arm64")
fi

# Fail early and actionably rather than on a confusing cargo error.
INSTALLED="$(rustup target list --installed)"
for entry in "${TARGETS[@]}"; do
  triple="${entry%%:*}"
  case "$INSTALLED" in
    *"$triple"*) ;;
    *) echo "missing Rust target $triple — run: rustup target add $triple" >&2; exit 1 ;;
  esac
done

# 1. Host build first. `uniffi-bindgen --library` introspects a real artifact to emit
#    bindings and needs one it can dlopen, so a host cdylib rather than an iOS slice.
#    `--features bindgen` on BOTH this and the `run` below keeps the crate on one
#    feature set, so it compiles once rather than once per combination.
echo "==> host cdylib (for bindgen introspection)"
cargo build --release --features bindgen
HOST_DYLIB="target/release/lib${LIB}.dylib"
[ -f "$HOST_DYLIB" ] || { echo "expected host cdylib at $HOST_DYLIB" >&2; exit 1; }

# 2. Generate the Swift bindings.
echo "==> swift bindings"
RAW="$(mktemp -d)"
trap 'rm -rf "$RAW"' EXIT
cargo run --release --features bindgen --bin uniffi-bindgen -- \
  generate --library "$HOST_DYLIB" \
  --language swift \
  --config bindings/uniffi.toml \
  --no-format \
  --out-dir "$RAW"

rm -rf "$SWIFT_GEN" "$FFI_INCLUDE"
mkdir -p "$SWIFT_GEN" "$FFI_INCLUDE"
# Keep the directories tracked: SPM resolution fails on a fresh clone if either is
# missing, and git will not track an empty directory.
touch "$SWIFT_GEN/.gitkeep" "$FFI_INCLUDE/.gitkeep"

# UniFFI emits the module map under its own name; SPM only honours a custom module map
# called `module.modulemap` and silently synthesises an umbrella one otherwise — which
# compiles, but drops the `use "Darwin"` declarations UniFFI emits.
cp "$RAW"/*.swift "$SWIFT_GEN/"
cp "$RAW"/*.h "$FFI_INCLUDE/"
cp "$RAW"/*.modulemap "$FFI_INCLUDE/module.modulemap"
[ -n "$(ls "$SWIFT_GEN"/*.swift 2>/dev/null)" ] || { echo "uniffi-bindgen emitted no .swift — bindings would be stale" >&2; exit 1; }
[ -n "$(ls "$FFI_INCLUDE"/*.h  2>/dev/null)" ] || { echo "uniffi-bindgen emitted no .h — the FFI module would be empty" >&2; exit 1; }

# 3. Per-slice static archives.
for entry in "${TARGETS[@]}"; do
  triple="${entry%%:*}"
  slice="${entry##*:}"
  echo "==> $slice ($triple)"
  cargo build --release --features uniffi --target "$triple"
  A="target/$triple/release/lib${LIB}.a"
  [ -f "$A" ] || { echo "expected static lib at $A" >&2; exit 1; }
  mkdir -p "ios/lib/$slice"
  cp "$A" "ios/lib/$slice/lib${LIB}.a"
  echo "    ✓ ios/lib/$slice/lib${LIB}.a ($(( $(stat -f%z "ios/lib/$slice/lib${LIB}.a") / 1024 )) KB unstripped)"
done

echo
echo "✓ done"
echo "  bindings → $SWIFT_GEN"
echo "  header   → $FFI_INCLUDE"
echo "  archives → ios/lib/"
echo
echo "Next: (cd ios && xcodegen) if project.yml changed, then open ios/LumaDemo.xcodeproj"
echo "and run on a PHYSICAL iPhone — the Simulator has no Bluetooth radio."
