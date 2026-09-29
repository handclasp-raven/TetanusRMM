# RMMTool

Remote monitoring and management tool. See [rmm-build-plan.md](rmm-build-plan.md)
for the architecture and phase plan.

## Workspace layout

| Crate | Kind | Purpose |
|---|---|---|
| `crates/protocol` | lib | Wire `Message` enum, framing, close codes, update manifest and signed-message format |
| `crates/common` | lib | Logging init, PEM loading, quinn/rustls TLS configs, dev cert generation |
| `crates/server` | bin + lib | QUIC listener for agents, HTTPS API, Postgres, auth, audit log, enrollment CA, update publishing |
| `crates/agent` | bin + lib | Enrollment, heartbeats, protected credential storage, signed self-update |
| `crates/viewer` | bin | Placeholder until Phase 5 |

Server modules: `quic` (agent listener), `api` (HTTPS routes), `auth` (Argon2id,
TOTP, sessions), `users`, `audit` (hash chain), `registry` (agents, policies),
`enroll` (tokens, internal CA), `updates` (signing, publishing), `db` (pool,
migrations), `config`.

Agent modules: `enroll`, `credstore` (DPAPI on Windows, dev-only encrypted file
elsewhere), `update` (download and verify), `updater` (rename-and-replace). Migrations live in
`crates/server/migrations/` and are embedded in the binary. They run
automatically when the server starts.

## Quick start with Docker Compose

Requires Docker and a Rust toolchain (for generating certs).

```sh
cargo run -p server -- gen-certs          # 1. dev CA + server cert into ./dev-certs
mkdir -p updates                          # 2. published agent builds live here
docker compose up --build -d              # 3. Postgres + server
curl --cacert dev-certs/ca.crt https://localhost:8443/api/health
# {"status":"ok"}
```

`gen-certs` writes `ca.crt`, `ca.key`, `server.crt` and `server.key`. The
server uses `ca.key` to sign agent certificates at enrollment. If your
`dev-certs/` predates Phase 3 it has no `ca.key`: run `gen-certs --force`,
which replaces the CA, so re-enroll any existing agents.

Create the first user. The password is read from the first line of stdin. The
command prints a TOTP secret and an `otpauth://` URL; add either to an
authenticator app.

```sh
echo 'a long password here' | \
  docker compose run --rm -T server create-user --username admin --role admin
```

Roles: `admin` (can change device policies and create download links),
`support_engineer` (can create download links), and `auditor` (read-only).

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

## Enrolling an agent

Agents do not share a certificate. Each one gets its own at enrollment:

1. **Create a download link** (admin or support engineer). Log in first (see
   [HTTPS API](#https-api)) and use the session token:

   ```sh
   curl --cacert dev-certs/ca.crt -H "Authorization: Bearer $SESSION" \
     -H 'content-type: application/json' \
     -d '{"platform": "windows-x86_64", "ttl_secs": 86400}' \
     https://localhost:8443/api/enrollment-links
   # {"token":"9f3c…","expires_at":"…","download_url":"https://localhost:8443/api/download/windows-x86_64?token=9f3c…"}
   ```

   Each link mints a random, single-use token that expires after `ttl_secs`
   (default 24 h, max 7 days). Only its SHA-256 is stored. The link downloads
   the latest published build for that platform (see
   [Publishing a signed update](#publishing-a-signed-update)). It keeps working
   until the token is used to enroll.

2. **Enroll** on the target machine with the token and the server's CA
   certificate:

   ```sh
   agent enroll --server 203.0.113.10:4433 --server-name localhost \
     --server-ca ca.crt --token 9f3c…
   ```

   The agent generates its key pair locally and sends only a certificate
   signing request, over QUIC without a client certificate. The server checks
   and consumes the token, assigns an id (`agt-…`), signs a client
   certificate, and pins its fingerprint to the agent. Reusing the token, or
   using an expired one, is refused.

   The credential (certificate, key, CA, server address) is stored in the
   state directory (`--state-dir`, default `%ProgramData%\RMM\agent` on
   Windows, `./agent-state` elsewhere):
   - **Windows:** encrypted with DPAPI under the agent's account.
   - **Linux/macOS:** a dev-only encrypted file whose key sits next to it.
     Not a real protection; see the TODO in
     `crates/agent/src/credstore/devfile.rs`.

3. **Run:** `agent run --state-dir …`. The agent connects with its issued
   certificate. The server accepts `Hello` only if the certificate is the one
   pinned to that agent id and the agent is not revoked. Setting
   `agents.enrollment_state = 'revoked'` locks it out at its next connection.

## Publishing a signed update

Agents only install updates signed with the ed25519 key whose public half was
baked in at build time.

```sh
# Once: create the signing key. Keep update-keys/update.key secret.
cargo run -p server -- gen-update-key            # prints RMM_UPDATE_PUBKEY=<hex>

# Build agents with the public key baked in (build fails if it is malformed).
RMM_UPDATE_PUBKEY=$(cat update-keys/update.pub) cargo build --release -p agent

# For each release: bump the agent's version in crates/agent/Cargo.toml, build, then
cargo run -p server -- sign-update target/release/agent \
  --platform linux-x86_64 --version 0.2.0        # writes target/release/agent.sig
cargo run -p server -- publish-update target/release/agent \
  --platform linux-x86_64 --version 0.2.0        # copies into ./updates/linux-x86_64/
```

Platforms are named `<os>-<arch>` as Rust reports them: `windows-x86_64`,
`linux-x86_64`, `macos-aarch64`.

The signature covers the platform, the version and the binary's SHA-256
together. A genuinely signed old build therefore can't be passed off as a newer
version, and a build for one platform can't be served to another.

The running agent checks `GET /api/updates/<platform>/manifest` on start and
then every `--update-interval-secs` (default 3600; 0 disables). When the
manifest shows a newer version it downloads the binary and signature and
verifies them in memory. Only after verification passes does it write
anything. It then:
1. writes the new binary next to itself as `agent.new`,
2. renames itself to `agent.old` (Windows allows renaming a running exe,
   though not overwriting it),
3. renames `agent.new` into place,
4. relaunches.

The new process deletes `agent.old` when it starts. A failed signature check
is logged and nothing changes. An agent built without `RMM_UPDATE_PUBKEY`
never updates itself.

## Server configuration

`server serve` reads each setting from a flag or an environment variable:

| Env var | Flag | Default | Purpose |
|---|---|---|---|
| `DATABASE_URL` | `--database-url` | *(required)* | Postgres URL, e.g. `postgres://rmm:pw@localhost:5432/rmm` |
| `RMM_DB_MAX_CONNECTIONS` | `--db-max-connections` | `10` | Pool size upper bound |
| `RMM_QUIC_LISTEN` | `--quic-listen` | `0.0.0.0:4433` | UDP address for agents |
| `RMM_API_LISTEN` | `--api-listen` | `0.0.0.0:8443` | TCP address for the HTTPS API |
| `RMM_CERTS_DIR` | `--certs-dir` | `dev-certs` | Holds `ca.crt` + `ca.key` (CA that signs agent certs), `server.crt`, `server.key` |
| `RMM_API_TLS_CERT` | `--api-tls-cert` | *(unset)* | PEM chain for HTTPS, e.g. from a public CA. Set together with the key. If unset, HTTPS uses `server.crt` from the certs dir. |
| `RMM_API_TLS_KEY` | `--api-tls-key` | *(unset)* | PEM private key for HTTPS |
| `RMM_SESSION_TTL_SECS` | `--session-ttl-secs` | `43200` (12 h) | Login session lifetime |
| `RMM_PUBLIC_URL` | `--public-url` | `https://localhost:8443` | Base URL of the API as agents and users reach it. Used in download links and given to agents for updates. |
| `RMM_UPDATES_DIR` | `--updates-dir` | `updates` | Published agent builds (`publish-update` writes here) |
| `RUST_LOG` | | `info` | Log filter. Logs go to stderr. |

`server create-user` needs `DATABASE_URL` too.

### Agent settings

| Env var | Flag | Default | Purpose |
|---|---|---|---|
| `RMM_STATE_DIR` | `--state-dir` | `%ProgramData%\RMM\agent` / `agent-state` | Where the protected credential lives |
| `RMM_SERVER` | `enroll --server` | `127.0.0.1:4433` | Server UDP address (stored at enrollment) |
| `RMM_SERVER_NAME` | `enroll --server-name` | `localhost` | Name the server certificate must match |
| `RMM_SERVER_CA` | `enroll --server-ca` | *(required)* | CA certificate that signed the server certificate |
| `RMM_ENROLL_TOKEN` | `enroll --token` | *(required)* | Token from the download link |
| `RMM_UPDATE_INTERVAL_SECS` | `run --update-interval-secs` | `3600` | Update check interval; `0` disables |
| `RMM_UPDATE_PUBKEY` | | *(unset)* | **Build time.** Hex ed25519 public key baked into the binary |

## HTTPS API

Sessions are sent as `Authorization: Bearer <token>`. Errors come back as
`{"error": "..."}`.

| Method | Path | Auth | |
|---|---|---|---|
| GET | `/api/health` | none | 200 if the database is reachable, else 503 |
| POST | `/api/auth/login` | none | `{username, password}` → `{challenge_token, expires_in_secs}` |
| POST | `/api/auth/totp` | none | `{challenge_token, code}` → `{session_token, expires_at, user}` |
| POST | `/api/auth/logout` | session | Ends the session |
| POST | `/api/enrollment-links` | admin, support_engineer | `{ttl_secs?, platform?}` → `{token, expires_at, download_url}` |
| GET | `/api/download/{platform}?token=` | enrollment token | Latest published agent build. Does not use up the token. |
| GET | `/api/updates/{platform}/manifest` | none | `{platform, version, sha256, size}` |
| GET | `/api/updates/{platform}/binary` | none | Agent build (signed, so public) |
| GET | `/api/updates/{platform}/signature` | none | 64-byte detached ed25519 signature |
| GET | `/api/me` | session | Current user |
| GET | `/api/agents` | session | Agent registry |
| GET | `/api/agents/{id}/policy` | session | Consent policy |
| PUT | `/api/agents/{id}/policy` | admin | `{consent_mode, on_no_user, consent_timeout_secs}` |

Login is two steps:
1. A correct password returns a 5-minute challenge token.
2. A correct TOTP code swaps the challenge for a session token.

Each TOTP code works only once. After 5 wrong codes the challenge is discarded
and the user has to enter their password again.

Every login attempt (successful or not), every logout, user creation, policy
change, download-link creation and agent enrollment is written to the
hash-chained `audit_log` table. Chain verification is
`server::audit::verify`.

## Running without Docker

```sh
cargo run -p server -- gen-certs
DATABASE_URL=postgres://... cargo run -p server -- serve
# create a user and a download link as above, then:
cargo run -p agent -- enroll --server-ca dev-certs/ca.crt --token <token>
cargo run -p agent -- run
```

Server heartbeat logs are at debug level (`RUST_LOG=info,server=debug`). If its
connection drops, the agent reconnects after 5 seconds.

`gen-certs --force` replaces an existing CA. Every enrolled agent then has to
enroll again.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace        # needs a running Docker daemon
```

The database tests (`crates/server/tests/db.rs`, `api.rs` and `enrollment.rs`)
each start a throwaway `postgres:17-alpine` container with testcontainers. The
container is removed when the test ends. `heartbeat.rs` (QUIC over loopback) and
`updates.rs` (signed updates over HTTPS) need no database.

The Windows-only code (DPAPI credential storage, detached relaunch) is behind
`#[cfg(windows)]`. It must be built and tested on Windows; Linux CI does not
compile it.
