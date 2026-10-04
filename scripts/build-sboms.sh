#!/bin/sh
# Write a software bill of materials for each thing a release ships, with
# Syft, into target/sbom/ as CycloneDX (.cdx.json) and SPDX (.spdx.json):
#
#   - the agent and quick assist for Windows, the viewer for Linux and
#     Windows and the server for Linux: the crates each one is built from on
#     that platform, taken from Cargo.lock (scripts/prune-cargo-lock.py);
#   - the TUI: the packages pip installs with it. The TUI has no lockfile,
#     so these are the versions its requirements resolve to today, for this
#     host's Python.
#
#   scripts/build-sboms.sh [VERSION]
#
# VERSION defaults to the workspace's version. Needs syft (or SYFT set to
# its path), cargo and python3 with pip; nothing is compiled. The TUI is
# taken from the wheel scripts/build-clients.sh built, if there is one, else
# from ./tui.
set -eu
cd "$(dirname "$0")/.."
. scripts/lib.sh

version=${1:-$(crate_version server)}
syft=${SYFT:-syft}
out=target/sbom
work=target/tmp/sbom
rm -rf "$out" "$work"
mkdir -p "$out" "$work"
out=$(realpath "$out")

# scan DIR NAME SOURCE_NAME
scan() {
    (cd "$work" && "$syft" scan "dir:$1" -q \
        --source-name "$3" --source-version "$version" \
        -o "cyclonedx-json=$out/$2.cdx.json" \
        -o "spdx-json=$out/$2.spdx.json")
}

# rust CRATE TARGET_TRIPLE PLATFORM
rust() {
    mkdir "$work/$1-$3"
    python3 scripts/prune-cargo-lock.py "$1" "$2" "$work/$1-$3/Cargo.lock"
    scan "$1-$3" "rmm-$1-$3" "rmm-$1"
}

rust agent x86_64-pc-windows-msvc windows-x86_64
rust assist x86_64-pc-windows-msvc windows-x86_64
rust viewer x86_64-pc-windows-msvc windows-x86_64
rust viewer x86_64-unknown-linux-gnu linux-x86_64
rust server x86_64-unknown-linux-gnu linux-x86_64

# The TUI and what it needs to run, in an environment with nothing else in
# it (no pip, no development tools).
tui=./tui
for wheel in target/clients/tetanus_rmm-"$version"-*.whl; do
    [ -f "$wheel" ] && tui=$wheel
done
python3 -m venv --without-pip "$work/tui"
python3 -m pip --python "$work/tui/bin/python" install --quiet \
    --disable-pip-version-check "$tui"
scan tui tetanus-rmm-tui tetanus-rmm

echo "wrote the SBOMs for $version to target/sbom/ (TUI from $tui)"
