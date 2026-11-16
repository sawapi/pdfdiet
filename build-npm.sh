#!/usr/bin/env bash
# Build the wasm for the npm package, targeting both Node and browsers.
set -euo pipefail
cd "$(dirname "$0")"

BINDGEN_VERSION=$(grep -A1 'name = "wasm-bindgen"' Cargo.lock | grep version | head -1 | cut -d'"' -f2)
BINDGEN="$HOME/Library/Caches/dev.trunkrs.trunk/wasm-bindgen-$BINDGEN_VERSION/wasm-bindgen"
if [[ ! -x "$BINDGEN" ]]; then
  if [[ "$(wasm-bindgen --version 2>/dev/null | awk '{print $2}')" == "$BINDGEN_VERSION" ]]; then
    BINDGEN=wasm-bindgen
  else
    echo "wasm-bindgen $BINDGEN_VERSION not found (cargo install wasm-bindgen-cli --version $BINDGEN_VERSION)" >&2
    exit 1
  fi
fi

cargo build -p pdfdiet-wasm --target wasm32-unknown-unknown --release
WASM=target/wasm32-unknown-unknown/release/pdfdiet_wasm.wasm

rm -rf npm/node npm/web
"$BINDGEN" --target nodejs --out-dir npm/node "$WASM"
"$BINDGEN" --target web --out-dir npm/web "$WASM"

# Shrink with wasm-opt if available (reusing trunk's cached binary).
WASMOPT=$(ls "$HOME"/Library/Caches/dev.trunkrs.trunk/wasm-opt-*/bin/wasm-opt 2>/dev/null | tail -1 || true)
if [[ -n "$WASMOPT" && -x "$WASMOPT" ]]; then
  for f in npm/node/pdfdiet_wasm_bg.wasm npm/web/pdfdiet_wasm_bg.wasm; do
    "$WASMOPT" -Os -o "$f" "$f"
  done
fi

cp LICENSE-MIT LICENSE-APACHE npm/
ls -lh npm/node npm/web
echo "OK: npm package ready in npm/"
