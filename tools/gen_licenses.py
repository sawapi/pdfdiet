#!/usr/bin/env python3
"""Print the licence inventory of every crate linked into the published wasm.

Walks the dependency graph of the `pdfdiet-wasm` crate as resolved for the
wasm32-unknown-unknown target, skipping dev-only edges, and groups the
crates by SPDX expression. Used by tools/gen_licenses.sh to regenerate
npm/THIRD_PARTY.md.
"""
import collections
import json
import subprocess

meta = json.loads(subprocess.run(
    ["cargo", "metadata", "--format-version", "1",
     "--filter-platform", "wasm32-unknown-unknown"],
    capture_output=True, text=True, cwd="wasm", check=True).stdout)

pkgs = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
root = next(n for n in meta["resolve"]["nodes"]
            if pkgs[n["id"]]["name"] == "pdfdiet-wasm")

seen, stack = set(), [root["id"]]
while stack:
    cur = stack.pop()
    if cur in seen:
        continue
    seen.add(cur)
    for dep in nodes[cur]["deps"]:
        kinds = {k.get("kind") for k in dep.get("dep_kinds", [{}])}
        if kinds and kinds <= {"dev"}:
            continue
        stack.append(dep["pkg"])

# The workspace's own crates are covered by the top-level licence.
OWN = {"pdfdiet-wasm", "pdfdiet", "smallpeg"}
by_license = collections.defaultdict(list)
for pid in seen:
    pkg = pkgs[pid]
    if pkg["name"] in OWN:
        continue
    by_license[pkg.get("license") or "UNKNOWN"].append(
        f'{pkg["name"]} {pkg["version"]}')

for lic in sorted(by_license):
    print(f"### {lic}\n")
    for crate in sorted(by_license[lic]):
        print(f"- {crate}")
    print()
