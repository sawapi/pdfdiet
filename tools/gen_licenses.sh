#!/usr/bin/env bash
# Regenerate npm/THIRD_PARTY.md from the crates that end up inside the
# published WebAssembly module.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
python3 tools/gen_licenses.py > "$tmp"
{
  sed -n '1,9p' npm/THIRD_PARTY.md
  cat "$tmp"
} > npm/THIRD_PARTY.md.new && mv npm/THIRD_PARTY.md.new npm/THIRD_PARTY.md
echo "wrote npm/THIRD_PARTY.md"
