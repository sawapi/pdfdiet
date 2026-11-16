#!/usr/bin/env bash
# Build the wasm for the npm package, targeting both Node (npm/node) and
# browsers and bundlers (npm/web).
set -euo pipefail
cd "$(dirname "$0")"

WANT=$(grep -A1 'name = "wasm-bindgen"' Cargo.lock | grep version | head -1 | cut -d'"' -f2)
HAVE=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
if [[ "$HAVE" != "$WANT" ]]; then
  echo "wasm-bindgen CLI $WANT is required (found: ${HAVE:-none})." >&2
  echo "Install it with: cargo install wasm-bindgen-cli --version $WANT --locked" >&2
  exit 1
fi

CRATE=$(grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2)
PKG=$(node -p "require('./npm/package.json').version")
if [[ "$CRATE" != "$PKG" ]]; then
  echo "npm/package.json version $PKG does not match Cargo.toml $CRATE" >&2
  exit 1
fi

cargo build -p pdfdiet-wasm --target wasm32-unknown-unknown --release
WASM=target/wasm32-unknown-unknown/release/pdfdiet_wasm.wasm

rm -rf npm/node npm/web
wasm-bindgen --target nodejs --out-dir npm/node "$WASM"
wasm-bindgen --target web --out-dir npm/web "$WASM"
# The web glue is an ES module; say so, or Node reparses it with a warning.
echo '{ "type": "module" }' > npm/web/package.json

# Shrink further when Binaryen's wasm-opt is installed (optional locally,
# always done in CI and releases). The feature flags match what the Rust
# wasm32 target emits.
if command -v wasm-opt >/dev/null; then
  for f in npm/node/pdfdiet_wasm_bg.wasm npm/web/pdfdiet_wasm_bg.wasm; do
    wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int \
      --enable-sign-ext --enable-reference-types --enable-multivalue \
      --enable-mutable-globals -o "$f" "$f"
  done
else
  echo "note: wasm-opt not found; the .wasm files are not size-optimized" >&2
fi

cp LICENSE-MIT LICENSE-APACHE npm/
ls -l npm/node/*.wasm npm/web/*.wasm
echo "OK: npm package ready in npm/"
