#!/usr/bin/env bash
# Generate Swift / Kotlin / Python bindings from a host build of the cdylib.
#
#   ./bindings/generate.sh              # all three languages
#   ./bindings/generate.sh python       # just one
#
# UniFFI's `--library` mode introspects a BUILT artifact rather than parsing the
# source, so the cdylib has to exist first. That is what the `bindgen` feature and
# the in-crate `uniffi-bindgen` binary are for.
#
# Output lands in bindings/out/<language>/, which is gitignored.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

LANGS=("$@")
if [ ${#LANGS[@]} -eq 0 ]; then
  LANGS=(swift kotlin python)
fi

echo "==> building the cdylib with --features bindgen"
cargo build --release --features bindgen

# macOS builds a .dylib, Linux a .so.
LIB="target/release/libluma_core.dylib"
[ -f "$LIB" ] || LIB="target/release/libluma_core.so"
[ -f "$LIB" ] || { echo "no cdylib at target/release/libluma_core.{dylib,so}" >&2; exit 1; }
echo "    library: $LIB"

for lang in "${LANGS[@]}"; do
  OUT="bindings/out/$lang"
  echo "==> $lang -> $OUT"
  rm -rf "$OUT"
  mkdir -p "$OUT"
  cargo run --release --features bindgen --bin uniffi-bindgen -- \
    generate \
    --library "$LIB" \
    --language "$lang" \
    --config bindings/uniffi.toml \
    --no-format \
    --out-dir "$OUT"
done

echo
echo "done. To use the Python bindings:"
echo "  cp $LIB bindings/out/python/"
echo "  PYTHONPATH=bindings/out/python python3 -c 'import luma_core; print(luma_core.glasses_gatt())'"
