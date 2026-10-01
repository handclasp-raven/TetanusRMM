#!/bin/sh
# Cross-compile the Windows agent on Linux, sign it, and publish it to
# ./updates/windows-x86_64 (served for download links, MSIs and updates).
# No Windows machine needed.
#
#   scripts/build-windows-agent.sh [--no-publish] [VERSION]
#
# VERSION defaults to the agent crate's version. The signing key stays on
# this machine: the build container only sees the source tree and the
# public key. Build outputs and the cargo cache live in Docker volumes
# (rmm-xwin-target, rmm-xwin-cargo); the signed build ends up in
# target/windows-x86_64/rmm-agent.exe.
set -eu
cd "$(dirname "$0")/.."

publish=yes
if [ "${1:-}" = "--no-publish" ]; then publish=no; shift; fi
version=${1:-$(cargo pkgid -p agent | sed 's/.*[#@]//')}
image=rmm-agent-windows-builder
out=target/windows-x86_64

if [ ! -f update-keys/update.pub ]; then
    echo "update-keys/update.pub missing: run 'cargo run -p server -- gen-update-key' first" >&2
    exit 1
fi

docker image inspect "$image" >/dev/null 2>&1 \
    || docker build -f docker/agent-windows.Dockerfile -t "$image" docker/

mkdir -p "$out"
docker run --rm \
    -v "$PWD":/src:ro \
    -v rmm-xwin-target:/target \
    -v rmm-xwin-cargo:/usr/local/cargo/registry \
    -v "$(realpath "$out")":/out \
    -e CARGO_TARGET_DIR=/target \
    -e RMM_UPDATE_PUBKEY="$(cat update-keys/update.pub)" \
    -e HOST_IDS="$(id -u):$(id -g)" \
    "$image" sh -c '
        cargo xwin build --locked --release -p agent --target x86_64-pc-windows-msvc &&
        cp /target/x86_64-pc-windows-msvc/release/agent.exe /out/rmm-agent.exe &&
        chown "$HOST_IDS" /out/rmm-agent.exe'

cargo run -q -p server -- sign-update "$out/rmm-agent.exe" \
    --platform windows-x86_64 --version "$version"
if [ "$publish" = yes ]; then
    cargo run -q -p server -- publish-update "$out/rmm-agent.exe" \
        --platform windows-x86_64 --version "$version"
    echo "published windows-x86_64 $version"
else
    echo "built and signed $out/rmm-agent.exe ($version), not published"
fi
