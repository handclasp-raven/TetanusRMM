# ---- build ----------------------------------------------------------------
FROM rust:1-slim-trixie AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor

# Plain build (no BuildKit cache mounts) so it works with any Docker builder.
RUN cargo build --release --locked -p server \
    && cp target/release/server /server

# ---- runtime --------------------------------------------------------------
# distroless/cc: glibc + libgcc only, no shell or package manager.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /server /usr/local/bin/server

# 4433/udp: agents (QUIC). 8443/tcp: HTTPS API. 3478/udp: STUN (direct paths).
EXPOSE 4433/udp 8443/tcp 3478/udp
ENTRYPOINT ["/usr/local/bin/server"]
CMD ["serve"]
