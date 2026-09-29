# RMMTool

Remote monitoring and management tool. See [rmm-build-plan.md](rmm-build-plan.md)
for the architecture and phase plan.

## Workspace layout

| Crate | Kind | Purpose |
|---|---|---|
| `crates/protocol` | lib | Wire `Message` enum, length-delimited postcard framing, ALPN, close codes |
| `crates/common` | lib | Logging init, PEM loading, quinn/rustls mutual-TLS config, dev cert generation |
| `crates/server` | bin + lib | QUIC listener for agents |
| `crates/agent` | bin + lib | QUIC client that sends heartbeats |
| `crates/viewer` | bin | Placeholder until Phase 5 |

## Development quick start

Requires a stable Rust toolchain.

### 1. Generate dev certificates

```sh
cargo run -p server -- gen-certs
```

This writes a throwaway CA, a server certificate (valid for `localhost`,
`127.0.0.1` and `::1`), and one agent client certificate (CN `dev-agent-1`) to
`dev-certs/`, which is gitignored:

```
dev-certs/ca.crt       CA both sides trust
dev-certs/server.crt   server certificate
dev-certs/server.key
dev-certs/agent.crt    agent client certificate
dev-certs/agent.key
```

Options: `--out <dir>`, `--agent-id <cn>`, and `--force` to overwrite existing
certificates. Regenerating creates a new CA, so restart both sides afterwards.

### 2. Run the server

```sh
cargo run -p server -- serve                        # listens on 0.0.0.0:4433/udp
cargo run -p server -- serve --listen 127.0.0.1:4433 --certs-dir dev-certs
```

The server requires every agent to present a client certificate signed by
`ca.crt`. Heartbeats are logged at debug level, so use
`RUST_LOG=info,server=debug` to see them.

### 3. Run the agent

```sh
cargo run -p agent
cargo run -p agent -- --server 127.0.0.1:4433 --server-name localhost \
    --agent-id dev-agent-1 --certs-dir dev-certs --heartbeat-secs 5
```

The agent verifies the server certificate against `ca.crt`, sends `Hello`, then
sends a `Heartbeat` every 5 seconds and logs each `HeartbeatAck`. If the
connection drops, it reconnects after 5 seconds.

Most flags can also be set through environment variables (`RMM_LISTEN`,
`RMM_SERVER`, `RMM_SERVER_NAME`, `RMM_AGENT_ID`, `RMM_CERTS_DIR`). The log
level is controlled by `RUST_LOG` (default `info`).

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`crates/server/tests/heartbeat.rs` runs the server and agent in-process over
loopback. It covers the handshake and heartbeats, connection migration after
the agent moves to a new UDP socket, and rejection of bad certificates and of
connections that skip `Hello`.
