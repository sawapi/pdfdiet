#!/usr/bin/env bash
# Build the landing page (web/) into web/dist as a static site: the page plus
# the browser build of the wasm from build-npm.sh. Serve web/dist with any
# static server, e.g. `python3 -m http.server -d web/dist`.
set -euo pipefail
cd "$(dirname "$0")"

if [[ "${SKIP_WASM:-}" != 1 ]]; then
  ./build-npm.sh
fi
[[ -f npm/web/pdfdiet_wasm_bg.wasm && -f npm/dist/web.mjs ]] || { echo "npm/web or npm/dist is missing; run ./build-npm.sh" >&2; exit 1; }

rm -rf web/dist
mkdir -p web/dist/pkg/dist web/dist/pkg/web
cp web/index.html web/style.css web/app.js web/favicon.svg web/dist/
# The page uses the npm package's worker entry point, laid out as in the
# package so its relative imports resolve.
cp npm/dist/worker.mjs npm/dist/worker-main.mjs npm/dist/protocol.mjs npm/dist/web.mjs npm/dist/core.mjs web/dist/pkg/dist/
cp npm/web/pdfdiet_wasm.js npm/web/pdfdiet_wasm_bg.wasm web/dist/pkg/web/
ls -lhR web/dist
echo "OK: site ready in web/dist"
