# Toolchain for cross-compiling the Windows agent on Linux: Rust's
# x86_64-pc-windows-msvc target, built with clang/lld via cargo-xwin against
# Microsoft's CRT and Windows SDK (downloaded once, into the image; by
# building this image you accept Microsoft's license for them).
#
# Used by scripts/build-windows-agent.sh, which mounts the source tree. The
# image holds no source and no keys.

FROM rust:1-slim-trixie

RUN apt-get update \
    && apt-get install -y --no-install-recommends clang lld llvm \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-pc-windows-msvc \
    && cargo install --locked cargo-xwin

ENV XWIN_ACCEPT_LICENSE=1 \
    XWIN_CACHE_DIR=/opt/xwin

# Fetch the CRT and SDK now (a throwaway build), not on every agent build.
RUN cargo new --quiet /tmp/warm \
    && cd /tmp/warm \
    && cargo xwin build --quiet --release --target x86_64-pc-windows-msvc \
    && rm -rf /tmp/warm "$CARGO_HOME/registry" \
    && chmod -R a+rX /opt/xwin

WORKDIR /src
