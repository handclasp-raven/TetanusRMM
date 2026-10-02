# Shared by the scripts in this directory. Source it from the repository root.

# The server CLI (gen-certs, sign-update, publish-*, ...). With a Rust
# toolchain it runs from source. Without one, or with RMM_SERVER_CLI=docker,
# it runs from the compose image (RMM_IMAGE if set, else the one from
# `docker compose build server`) as the calling user, so the files it writes
# belong to them. Paths must be relative to the repository root.
server() {
    if [ "${RMM_SERVER_CLI:-}" != docker ] && command -v cargo >/dev/null 2>&1; then
        cargo run -q -p server -- "$@"
    else
        docker run --rm -i -u "$(id -u):$(id -g)" -v "$PWD":/work -w /work \
            "${RMM_IMAGE:-${RMM_SERVER_IMAGE:-rmm-server:dev}}" "$@"
    fi
}

# crate_version NAME: the crate's own version, or the workspace's.
crate_version() {
    v=$(sed -n 's/^version = "\(.*\)"/\1/p' "crates/$1/Cargo.toml" | head -n 1)
    [ -n "$v" ] || v=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
    echo "$v"
}
