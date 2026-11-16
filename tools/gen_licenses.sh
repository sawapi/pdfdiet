#!/usr/bin/env bash
# Regenerate npm/THIRD_PARTY.md from the crates that end up inside the
# published WebAssembly module.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 tools/gen_licenses.py > /tmp/pdfdiet_licenses.md
{
  sed -n '1,9p' npm/THIRD_PARTY.md
  cat /tmp/pdfdiet_licenses.md
} > npm/THIRD_PARTY.md.new && mv npm/THIRD_PARTY.md.new npm/THIRD_PARTY.md
echo "wrote npm/THIRD_PARTY.md"
