# RMMTool

Remote monitoring and management tool. See [rmm-build-plan.md](rmm-build-plan.md)
for the architecture and phase plan.

## Workspace layout

| Crate | Kind | Purpose |
|---|---|---|
| `crates/protocol` | lib | Wire `Message` enum, framing, close codes, update manifest and signed-message format |
| `crates/common` | lib | Logging init, PEM loading, quinn/rustls TLS configs, dev cert generation |
| `crates/server` | bin + lib | QUIC listener for agents, HTTPS API, Postgres, auth, audit log, enrollment CA, update publishing |
| `crates/agent` | bin + lib | Enrollment, heartbeats with telemetry, protected credential storage, signed self-update, Windows service + session helper + tray |
| `crates/viewer` | bin + lib | Cross-platform remote desktop viewer (Linux, macOS, Windows): QUIC to the server, OpenH264 decode, winit + softbuffer window |

Server modules: `quic` (agent listener), `api` (HTTPS routes), `auth` (Argon2id,
TOTP, sessions), `users`, `audit` (hash chain), `registry` (agents, policies),
`enroll` (tokens, internal CA), `updates` (signing, publishing), `relay` (video
fan-out to viewers), `viewers` (viewer-session tokens), `db` (pool, migrations),
`config`.

Agent modules:
- `core`: the connect/heartbeat/update loop, shared by console and service mode.
- `enroll`
- `credstore`: DPAPI on Windows, a dev-only encrypted file elsewhere.
- `telemetry`: sysinfo.
- `session`: the helper supervisor state machine.
- `update` (download and verify) and `updater` (rename-and-replace).
- `media`: NV12 conversion, H.264 fix-ups, and the source link used for
  streaming.
- `paths`
- `win` (Windows only): `service`, `process` (spawn into a session), `pipe`,
  `helper` (tray), `acl`, `capture` (DXGI), `encoder` (Media Foundation),
  `stream` (capture worker), `bridge` (service ↔ helper media).

Migrations live in
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

## Installing the agent on Windows (service + session helper)

On Windows the agent runs as a service. From an elevated prompt, with the token
from a download link and the server's `ca.crt`:

```powershell
rmm-agent.exe service install --server 203.0.113.10:4433 --server-name localhost `
  --server-ca C:\path\to\ca.crt --token 9f3c…
rmm-agent.exe service stop | start      # manual control
rmm-agent.exe service uninstall         # stops and removes the service; keeps files
```

`service install`:
1. Copies the binary to `C:\Program Files\RMM\rmm-agent.exe`.
2. Creates `C:\ProgramData\RMM\agent\` with an ACL allowing only SYSTEM and
   Administrators (nothing inherited from `ProgramData`, which ordinary users can
   read).
3. Saves the token there as a pending enrollment request.
4. Registers the `RmmAgent` service:
   - runs as **LocalSystem** and starts automatically at boot;
   - restarts on failure after 5 s, 5 s, then 30 s;
   - failure restarts also apply to the agent's own non-zero exits.
5. Starts the service. Pass `--no-start` to skip this.

The service enrolls itself on first start. It does this rather than the
installer because DPAPI ties the credential to the account that encrypts it:
the admin running `install` is not the account that runs the service. So
SYSTEM enrolls, encrypts the credential under SYSTEM, and deletes the request.
Reinstalling later needs no token, because the existing credential is reused.

For the same reason, don't use `agent enroll`/`agent run` in the default
state directory on a machine where the service is installed. Console mode is
for development; point it at its own `--state-dir`.

### Service and helper model

```text
 Session 0 (no desktop)                 User's session (e.g. 1)
┌──────────────────────────────┐       ┌─────────────────────────────┐
│ rmm-agent.exe service run    │ spawn │ rmm-agent.exe helper        │
│ LocalSystem, auto-start      │──────▶│ runs as the logged-on user  │
│ - QUIC + heartbeat/telemetry │       │ - tray icon                 │
│ - signed updates             │◀─────▶│ - (later) capture + input   │
│ - supervises the helper      │ pipe  │                             │
└──────────────────────────────┘       └─────────────────────────────┘
```

Why a helper is needed: services run in **session 0**, which Windows isolates
from users (its own window station and desktop, visible to no one). A SYSTEM
service there can't capture the user's screen, inject input, or show a tray
icon, and it would only ever see session 0's empty desktop. So the service
starts a helper process *inside* the user's session:

- **Finding the session:** `WTSGetActiveConsoleSessionId` gives the console
  session. `WTSQueryUserToken` succeeds only if a user is logged on there; at
  the logon screen, nobody is.
- **Starting the helper:** it's launched with that user's token and
  environment block, via `CreateProcessAsUser` on `winsta0\default`. It runs
  **as the user, not SYSTEM**. (The secure desktop for UAC and the logon
  screen needs a SYSTEM token and is Phase 9.)
- **When nobody is logged on,** the service keeps running and heartbeating.
  It re-checks every 5 s, and immediately on session-change notifications
  (logon, logoff, user switch), then spawns the helper when a user appears.
- **If spawning fails or the helper crashes quickly,** retries back off
  exponentially: 1 s, 2 s, 4 s … up to 60 s. A helper that has run for 60 s
  resets the backoff.
- **If the console user changes,** the old helper is terminated and a new one
  is started for the new user.

This logic lives in `crates/agent/src/session.rs` as a pure state machine
with unit tests; the Windows code only feeds it observations.

The service and helper talk over the named pipe `\\.\pipe\rmm-agent-helper`.
Only SYSTEM, Administrators and interactive users may open it, and remote
clients are rejected. The service creates it with `first_pipe_instance`, so
another process can't claim the name first. It also accepts a connection only
from the exact process ID it just spawned; the helper starts suspended until
that ID is recorded. For now the pipe carries status for the tray; screen
capture and input come in later phases.

The **tray icon** is a green, amber or grey dot for connected, disconnected
and not enrolled. Its tooltip is e.g. "RMM Agent 0.1.1 - connected". Its menu
has the status line, *About*, and a disabled *Quit*: users can't stop the
agent. Windows 11 puts new tray icons in the overflow (^) area until the user
pins them.

The helper exits when the pipe closes, and the service restarts it.

**Updates in service mode:** after a verified update is swapped in, the
service exits with service-specific code 1, and the SCM's failure actions
restart it on the new binary.

**Logs:**
- service: `C:\ProgramData\RMM\agent\agent.log`
- helper: `%LOCALAPPDATA%\RMM\helper.log` for the logged-on user

Both are appended to with no rotation yet.

## Remote desktop viewing

Agents sit behind NAT and only connect *out*, so viewers never connect to an
agent. They connect to the **server**, which relays:

```text
 helper (user session)          service              server                viewers
 DXGI capture ─▶ MF H.264 ─pipe─▶ QUIC video ─────────▶ relay ──┬─▶ viewer A (Linux)
 (dirty rects)   encode once      stream (agent → server)        └─▶ viewer B (Windows)
```

The agent **encodes once**, whatever the number of viewers, and the server
fans the one stream out. Other points:
- **Start and stop:** capture starts when the first viewer arrives and stops
  when the last one leaves.
- **Joining mid-stream:** a viewer that joins late starts at a keyframe the
  server asks the agent for.
- **Slow viewers:** a viewer that falls behind skips to the next keyframe
  rather than slowing anyone else.
- **Monitor choice is shared:** picking a monitor changes it for everyone
  watching that agent, because there is only one stream.

### Launching the viewer

1. Get a short-lived viewer token (admin or support engineer; auditors can't
   view screens). It is valid for **60 seconds** and works **once**:

   ```sh
   curl --cacert dev-certs/ca.crt -X POST -H "Authorization: Bearer $SESSION" \
     https://localhost:8443/api/agents/agt-c77b3326d49696ef/viewer-sessions
   # {"token":"…","expires_at":"…","agent_id":"agt-…","online":true}
   ```

2. Start the viewer with it (the TUI will do both steps in Phase 8):

   ```sh
   viewer --server 203.0.113.10:4433 --server-name localhost \
     --ca dev-certs/ca.crt --token <token>
   # or: RMM_VIEWER_TOKEN=<token> viewer --ca dev-certs/ca.crt
   ```

   In the window:
   - **Tab** or **M** shows the monitor picker; **1–9** switches monitor.
   - **F5** requests a fresh keyframe.
   - **Esc** closes the picker, or the viewer.

   `--monitor N` picks a monitor at startup. `--snapshot out.ppm --frames N`
   is headless: it decodes N frames, writes the last one, and exits. Use it
   for checks and on machines without a display.

The server audits `viewer.session_create`, `viewer.connect` and
`viewer.disconnect` (the last with duration and frames sent). A token that
has expired or already been used is refused, as is a token for an agent that
isn't connected ("agent is not connected").

### Capture and encoding on the agent

- **Capture:** DXGI Desktop Duplication in the helper (the user's session),
  on any selected monitor. Only the regions DXGI reports as changed (dirty
  and move rectangles) are copied off the GPU and converted to NV12. Frames
  are encoded only when something changed, at up to 30 fps, so a static
  screen costs almost nothing.
- **Encoding:** H.264 through Media Foundation. **Hardware encoders are
  preferred** (NVENC, Quick Sync and AMF all register as MFTs). If none
  exists or none accepts the configuration, it **falls back to Microsoft's
  software "H264 Encoder MFT"**, and the helper log says which it used:
  `H.264 encoder ready encoder=… hardware=true|false`.
- **Stream format:** Constrained Baseline profile, low-latency mode, Annex B,
  with SPS/PPS repeated on every keyframe. Baseline keeps it decodable by the
  portable OpenH264 decoder the viewer uses.
- **Bitrate:** scales with resolution (4 Mbit/s at 1080p) and can be changed
  at runtime (`H264Encoder::set_bitrate`, for Phase 9's adaptive bitrate).
- **Display changes:** display switches (UAC, lock screen, resolution change)
  invalidate the duplication; capture restarts and resends the monitor list.
- **Backpressure:** if the network can't keep up, frames are dropped rather
  than queued, and the next one is a keyframe.
- **Not yet:** the mouse cursor isn't drawn into the stream.

Measured on the development VM (Windows 11, 4 vCPUs, **no GPU, so the
software encoder**), at 1024×768:

| Screen | Helper CPU | Service CPU |
|---|---|---|
| Static | ~0.2% of one core | ~0% |
| Console scrolling 10 lines/s (~18 fps) | 16% of one core (4% of the VM) | 0.7% |

The hardware encoder path is implemented, including the asynchronous model
hardware MFTs use, but it hasn't been exercised: the VM has no GPU encoder.
Test it on a machine with an NVIDIA, Intel or AMD GPU.

## Telemetry

Every heartbeat carries a health sample, collected with `sysinfo`:
- CPU %: whole machine, since the previous sample
- used/total RAM
- used/total bytes of the system disk (`%SystemDrive%`, or `/`)
- uptime

The server stores the latest values on the agent's row (`cpu_percent`,
`mem_used_bytes`, `mem_total_bytes`, `disk_used_bytes`, `disk_total_bytes`,
`uptime_secs`, `telemetry_at`). `GET /api/agents` returns them.

This changed the wire format, so the protocol version is now 2. Phase 1–3
agents can't talk to this server until they update, which they still can,
because updates go over HTTPS.

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
| POST | `/api/agents/{id}/viewer-sessions` | admin, support_engineer | → `{token, expires_at, agent_id, online}`: single-use viewer token, valid 60 s |
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

The dev server certificate is valid for `localhost`, `127.0.0.1` and `::1`.
Agents on other machines connect to the QUIC port with `--server-name
localhost`, which works. The HTTPS API (updates, download links) is reached at
`RMM_PUBLIC_URL`, so its host must also be in the certificate. Add names or
IPs with `gen-certs --san 192.168.122.1 --san rmm.example.lan`.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace        # needs a running Docker daemon
```

The database tests (`crates/server/tests/db.rs`, `api.rs`, `enrollment.rs`,
`telemetry.rs` and `streaming.rs`)
each start a throwaway `postgres:17-alpine` container with testcontainers. The
container is removed when the test ends. `heartbeat.rs` (QUIC over loopback) and
`updates.rs` (signed updates over HTTPS) need no database. `streaming.rs` runs a
real agent session with a synthetic two-monitor source (OpenH264 standing in
for DXGI + Media Foundation), the real relay, and two real viewer clients. It
checks:
- both viewers decode frames from **one** encode (one `Start`, byte-identical
  frames);
- a monitor switch applies to both;
- the agent stops capturing when both leave;
- viewer tokens are single-use and role-checked.

### On Windows

The Windows-only code (service, session helper, tray, named pipe, DPAPI, ACLs,
detached relaunch, DXGI capture, Media Foundation encoding) is behind
`#[cfg(windows)]`, so Linux builds don't compile
it. Build, lint and test it on Windows (MSVC toolchain):

```powershell
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p agent -p protocol -p common
cargo test -p server --lib --test heartbeat --test updates   # no Docker needed
```

The DPAPI credential tests need an account whose logon credentials are
loaded: an interactive logon, or SYSTEM (as the service runs). Over
*key-based* SSH they fail with "Access is denied", because Windows never
receives the password that user-scope DPAPI keys derive from. To run them over
SSH, run the test binary as SYSTEM, e.g. from a one-off scheduled task.

`agent capture-test --monitor N --seconds S --out file.h264` (hidden
subcommand) captures and encodes without a server. Run it in an interactive
session, then check the output with `ffprobe`/`ffplay`.

Service install, reboot survival, the helper in the user's session, the tray
icon, service-mode self-update and live streaming (two viewers, monitor
switching) are verified by hand on a Windows 11 24H2
VM (see the Phase 4 notes in the commit history).
