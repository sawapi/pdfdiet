#!/usr/bin/env bash
# Build the landing page (web/) into web/dist as a static site: the page plus
# the browser build of the wasm from build-npm.sh. Serve web/dist with any
# static server, e.g. `python3 -m http.server -d web/dist`.
set -euo pipefail
cd "$(dirname "$0")"

if [[ "${SKIP_WASM:-}" != 1 ]]; then
  ./build-npm.sh
fi
[[ -f npm/web/pdfdiet_wasm_bg.wasm ]] || { echo "npm/web is missing; run ./build-npm.sh" >&2; exit 1; }

rm -rf web/dist
mkdir -p web/dist
cp web/index.html web/style.css web/app.js web/worker.js web/favicon.svg web/dist/
cp npm/web/pdfdiet_wasm.js npm/web/pdfdiet_wasm_bg.wasm web/dist/
ls -lh web/dist
echo "OK: site ready in web/dist"
