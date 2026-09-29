# RMMTool

Remote monitoring and management tool. See [rmm-build-plan.md](rmm-build-plan.md)
for the architecture and phase plan.

## Workspace layout

| Crate | Kind | Purpose |
|---|---|---|
| `crates/protocol` | lib | Wire `Message` enum, length-delimited postcard framing, ALPN, close codes |
| `crates/common` | lib | Logging init, PEM loading, quinn/rustls TLS configs, dev cert generation |
| `crates/server` | bin + lib | QUIC listener for agents, HTTPS API, Postgres, auth, audit log |
| `crates/agent` | bin + lib | QUIC client that sends heartbeats |
| `crates/viewer` | bin | Placeholder until Phase 5 |

Server modules: `quic` (agent listener), `api` (HTTPS routes), `auth` (Argon2id,
TOTP, sessions), `users`, `audit` (hash chain), `registry` (agents, policies),
`db` (pool, migrations), `config`. Migrations live in
`crates/server/migrations/` and are embedded in the binary. They run
automatically when the server starts.

## Quick start with Docker Compose

Requires Docker and a Rust toolchain (for generating certs).

```sh
cargo run -p server -- gen-certs          # 1. dev CA + certs into ./dev-certs
docker compose up --build -d              # 2. Postgres + server
curl --cacert dev-certs/ca.crt https://localhost:8443/api/health
# {"status":"ok"}
```

Create the first user. The password is read from the first line of stdin. The
command prints a TOTP secret and an `otpauth://` URL; add either to an
authenticator app.

```sh
echo 'a long password here' | \
  docker compose run --rm -T server create-user --username admin --role admin
```

Roles: `admin` (can change device policies), `support_engineer`, and
`auditor` (read-only).

Point an agent at the container: `cargo run -p agent`. It connects to
`127.0.0.1:4433` over UDP and shows up in `GET /api/agents`.

Stop with `docker compose down`, or `docker compose down -v` to also delete the
database volume.

### Compose settings

Put overrides in a `.env` file next to `docker-compose.yml`:

| Variable | Default | Purpose |
|---|---|---|
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `rmm` / `rmm-dev-password` / `rmm` | Database credentials. **Change the password outside local dev.** |
| `RMM_API_PORT` | `8443` | Host TCP port for the HTTPS API |
| `RMM_QUIC_PORT` | `4433` | Host UDP port for agents |
| `RMM_UID` / `RMM_GID` | `1000` / `1000` | User the server runs as. Must be able to read the `0600` keys in `./dev-certs` (use `id -u` / `id -g`). |
| `RMM_DB_MAX_CONNECTIONS`, `RMM_SESSION_TTL_SECS`, `RUST_LOG` | see below | Passed through to the server |

The database lives in the `pgdata` named volume. `./dev-certs` is mounted
read-only at `/certs`.

## Server configuration

`server serve` reads each setting from a flag or an environment variable:

| Env var | Flag | Default | Purpose |
|---|---|---|---|
| `DATABASE_URL` | `--database-url` | *(required)* | Postgres URL, e.g. `postgres://rmm:pw@localhost:5432/rmm` |
| `RMM_DB_MAX_CONNECTIONS` | `--db-max-connections` | `10` | Pool size upper bound |
| `RMM_QUIC_LISTEN` | `--quic-listen` | `0.0.0.0:4433` | UDP address for agents |
| `RMM_API_LISTEN` | `--api-listen` | `0.0.0.0:8443` | TCP address for the HTTPS API |
| `RMM_CERTS_DIR` | `--certs-dir` | `dev-certs` | Holds `ca.crt` (agent CA), `server.crt`, `server.key` |
| `RMM_API_TLS_CERT` | `--api-tls-cert` | *(unset)* | PEM chain for HTTPS, e.g. from a public CA. Set together with the key. If unset, HTTPS uses `server.crt` from the certs dir. |
| `RMM_API_TLS_KEY` | `--api-tls-key` | *(unset)* | PEM private key for HTTPS |
| `RMM_SESSION_TTL_SECS` | `--session-ttl-secs` | `43200` (12 h) | Login session lifetime |
| `RUST_LOG` | | `info` | Log filter. Logs go to stderr. |

`server create-user` needs `DATABASE_URL` too.

## HTTPS API

Sessions are sent as `Authorization: Bearer <token>`. Errors come back as
`{"error": "..."}`.

| Method | Path | Auth | |
|---|---|---|---|
| GET | `/api/health` | none | 200 if the database is reachable, else 503 |
| POST | `/api/auth/login` | none | `{username, password}` → `{challenge_token, expires_in_secs}` |
| POST | `/api/auth/totp` | none | `{challenge_token, code}` → `{session_token, expires_at, user}` |
| POST | `/api/auth/logout` | session | Ends the session |
| GET | `/api/me` | session | Current user |
| GET | `/api/agents` | session | Agent registry |
| GET | `/api/agents/{id}/policy` | session | Consent policy |
| PUT | `/api/agents/{id}/policy` | admin | `{consent_mode, on_no_user, consent_timeout_secs}` |

Login is two steps:
1. A correct password returns a 5-minute challenge token.
2. A correct TOTP code swaps the challenge for a session token.

Each TOTP code works only once. After 5 wrong codes the challenge is discarded
and the user has to enter their password again.

Every login attempt (successful or not), every logout, user creation and policy
change is written to the hash-chained `audit_log` table. Chain verification is
`server::audit::verify`.

## Running without Docker

```sh
cargo run -p server -- gen-certs
DATABASE_URL=postgres://... cargo run -p server -- serve
cargo run -p agent
```

The agent takes `--server 127.0.0.1:4433 --server-name localhost --agent-id
dev-agent-1 --certs-dir dev-certs --heartbeat-secs 5`, or the environment
variables `RMM_SERVER`, `RMM_SERVER_NAME`, `RMM_AGENT_ID` and `RMM_CERTS_DIR`.
It verifies the server certificate against `ca.crt`, sends `Hello`, then sends
a heartbeat every 5 seconds and logs each ack. If the connection drops, it
reconnects after 5 seconds. Server heartbeat logs are at debug level
(`RUST_LOG=info,server=debug`).

`gen-certs` options: `--out <dir>`, `--agent-id <cn>`, and `--force` to replace
existing certificates. Regenerating creates a new CA, so restart everything
afterwards.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace        # needs a running Docker daemon
```

The database tests (`crates/server/tests/db.rs` and `api.rs`) each start a
throwaway `postgres:17-alpine` container with testcontainers. The container is
removed when the test ends. `heartbeat.rs` runs server and agent in-process over
loopback QUIC and needs no database.
