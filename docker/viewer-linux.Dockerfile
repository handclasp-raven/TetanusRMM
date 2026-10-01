# Toolchain for building the Linux viewer that the support TUI downloads.
# Debian 12's glibc (2.36) is older than most desktops', so the build runs
# on them; a viewer built on the server host would need that host's glibc
# or newer. g++ and nasm are for OpenH264.
#
# Used by scripts/build-clients.sh, which mounts the source tree. The image
# holds no source and no keys.

FROM rust:1-slim-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends g++ nasm \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
