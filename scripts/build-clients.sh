#!/bin/sh
# Build what support staff run, so they only have to install the TUI and
# sign in:
#
#   - the viewer for Linux and Windows, published to ./updates/<platform>/
#     (the TUI downloads the one for its machine from the server);
#   - the TUI as a wheel, published to ./updates/tui/ (staff download it
#     from the server's install page, https://<server>/install).
#
#   scripts/build-clients.sh [--no-publish] [VERSION]
#
# VERSION defaults to the viewer crate's version. Both viewers are built in
# Docker: Linux against an old glibc (docker/viewer-linux.Dockerfile),
# Windows with the agent's cross toolchain (docker/agent-windows.Dockerfile).
# Build outputs and the cargo caches live in Docker volumes.
set -eu
cd "$(dirname "$0")/.."
. scripts/lib.sh

publish=yes
if [ "${1:-}" = "--no-publish" ]; then publish=no; shift; fi
version=${1:-$(crate_version viewer)}
out=target/clients
mkdir -p "$out"

# build IMAGE DOCKERFILE VOLUME_PREFIX OUTPUT_NAME COMMAND
build() {
    docker image inspect "$1" >/dev/null 2>&1 \
        || docker build -f "docker/$2" -t "$1" docker/
    docker run --rm \
        -v "$PWD":/src:ro \
        -v "$3-target":/target \
        -v "$3-cargo":/usr/local/cargo/registry \
        -v "$(realpath "$out")":/out \
        -e CARGO_TARGET_DIR=/target \
        -e HOST_IDS="$(id -u):$(id -g)" \
        "$1" sh -c "$5 && chown \"\$HOST_IDS\" /out/$4"
}

build rmm-viewer-linux-builder viewer-linux.Dockerfile rmm-viewer-linux rmm-viewer '
    cargo build --locked --release -p viewer &&
    cp /target/release/viewer /out/rmm-viewer'
build rmm-agent-windows-builder agent-windows.Dockerfile rmm-xwin rmm-viewer.exe '
    cargo xwin build --locked --release -p viewer --target x86_64-pc-windows-msvc &&
    cp /target/x86_64-pc-windows-msvc/release/viewer.exe /out/rmm-viewer.exe'

rm -f "$out"/tetanus_rmm-*.whl
if python3 -m pip --version >/dev/null 2>&1; then
    python3 -m pip wheel --quiet --no-deps --wheel-dir "$out" ./tui
else
    # No Python (or pip) on this host: build the wheel in a container, from
    # a copy, since the build writes into the source directory.
    docker run --rm \
        -v "$PWD/tui":/src:ro \
        -v "$(realpath "$out")":/out \
        -e HOST_IDS="$(id -u):$(id -g)" \
        python:3-slim sh -c '
            cp -r /src /tmp/tui &&
            pip wheel --quiet --no-deps --wheel-dir /out /tmp/tui &&
            chown "$HOST_IDS" /out/tetanus_rmm-*.whl'
fi
wheel=$(ls "$out"/tetanus_rmm-*.whl)

if [ "$publish" = yes ]; then
    server publish-viewer "$out/rmm-viewer" \
        --platform linux-x86_64 --version "$version"
    server publish-viewer "$out/rmm-viewer.exe" \
        --platform windows-x86_64 --version "$version"
    server publish-tui "$wheel"
    echo "published viewer $version (linux-x86_64, windows-x86_64) and $(basename "$wheel")"
    echo "staff install from the server's /install page"
else
    echo "built $out/rmm-viewer, $out/rmm-viewer.exe ($version) and $wheel, not published"
fi
