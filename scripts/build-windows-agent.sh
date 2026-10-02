#!/bin/sh
# Cross-compile the Windows agent on Linux, sign it, and publish it to
# ./updates/windows-x86_64 (served for download links, MSIs and updates).
# No Windows machine needed.
#
#   scripts/build-windows-agent.sh [--no-publish | --unsigned] [VERSION]
#
# VERSION defaults to the agent crate's version. The signing key stays on
# this machine: the build container only sees the source tree and the
# public key. --unsigned builds the agent that releases ship: no public key
# baked in (it pins the one its server publishes) and no signature, since
# each server signs it with its own key (scripts/install.sh). Build outputs and the cargo cache live in Docker volumes
# (rmm-xwin-target, rmm-xwin-cargo); the signed build ends up in
# target/windows-x86_64/rmm-agent.exe.
set -eu
cd "$(dirname "$0")/.."
. scripts/lib.sh

publish=yes
sign=yes
case ${1:-} in
    --no-publish) publish=no; shift ;;
    --unsigned) publish=no; sign=no; shift ;;
esac
version=${1:-$(crate_version agent)}
image=rmm-agent-windows-builder
out=target/windows-x86_64

if [ "$sign" = no ]; then
    pubkey=
elif [ -f update-keys/update.pub ]; then
    pubkey=$(cat update-keys/update.pub)
else
    echo "update-keys/update.pub missing: run 'cargo run -p server -- gen-update-key' (or scripts/install.sh) first" >&2
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
    ${pubkey:+-e RMM_UPDATE_PUBKEY="$pubkey"} \
    -e HOST_IDS="$(id -u):$(id -g)" \
    "$image" sh -c '
        cargo xwin build --locked --release -p agent --target x86_64-pc-windows-msvc &&
        cp /target/x86_64-pc-windows-msvc/release/agent.exe /out/rmm-agent.exe &&
        chown "$HOST_IDS" /out/rmm-agent.exe'

if [ "$sign" = no ]; then
    echo "built $out/rmm-agent.exe ($version), without a key and unsigned"
    exit 0
fi
server sign-update "$out/rmm-agent.exe" \
    --platform windows-x86_64 --version "$version"
if [ "$publish" = yes ]; then
    server publish-update "$out/rmm-agent.exe" \
        --platform windows-x86_64 --version "$version"
    echo "published windows-x86_64 $version"
else
    echo "built and signed $out/rmm-agent.exe ($version), not published"
fi
