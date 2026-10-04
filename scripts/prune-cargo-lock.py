#!/usr/bin/env python3
"""Write a Cargo.lock holding only what one crate ships for one target: its
normal and build dependencies (no dev-dependencies), as cargo resolves them
for that platform. The workspace's Cargo.lock covers every crate, target and
test at once, so an SBOM taken from it would list far more than any one
binary contains.

    scripts/prune-cargo-lock.py CRATE TARGET_TRIPLE OUTPUT

Run from the repository root. Used by scripts/build-sboms.sh.
"""
import json
import subprocess
import sys
import tomllib

crate, triple, out = sys.argv[1:4]
meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--locked",
     "--filter-platform", triple]))
pkgs = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
root = next(i for i in meta["workspace_members"] if pkgs[i]["name"] == crate)

keep, todo = set(), [root]
while todo:
    i = todo.pop()
    if i in keep:
        continue
    keep.add(i)
    for dep in nodes[i]["deps"]:
        if any(k["kind"] in (None, "build") for k in dep["dep_kinds"]):
            todo.append(dep["pkg"])

kept = {(pkgs[i]["name"], pkgs[i]["version"]) for i in keep}
names = {name for name, _ in kept}
with open("Cargo.lock", "rb") as f:
    lock = tomllib.load(f)


def dep_kept(dep):
    # "name", "name version" or "name version (source)"
    parts = dep.split()
    return (parts[0], parts[1]) in kept if len(parts) > 1 else parts[0] in names


lines = [f"# Pruned from the workspace Cargo.lock: {crate} for {triple}",
         f"version = {lock['version']}", ""]
count = 0
for p in lock["package"]:
    if (p["name"], p["version"]) not in kept:
        continue
    count += 1
    lines.append("[[package]]")
    for key in ("name", "version", "source", "checksum"):
        if key in p:
            lines.append(f"{key} = {json.dumps(p[key])}")
    deps = [d for d in p.get("dependencies", []) if dep_kept(d)]
    if deps:
        lines.append("dependencies = [")
        lines += [f" {json.dumps(d)}," for d in deps]
        lines.append("]")
    lines.append("")
if count != len(kept):
    sys.exit(f"{crate} {triple}: Cargo.lock has {count} of the {len(kept)} packages cargo resolved")
with open(out, "w") as f:
    f.write("\n".join(lines))
print(f"{crate} {triple}: {count} of {len(lock['package'])} packages", file=sys.stderr)
