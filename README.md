# RMMTool

Remote monitoring and management tool. See [rmm-build-plan.md](rmm-build-plan.md)
for the architecture and phase plan.

## Workspace layout

| Crate | Kind | Purpose |
|---|---|---|
| `crates/protocol` | lib | Wire `Message` enum, framing, close codes, update manifest and signed-message format |
| `crates/common` | lib | Logging init, PEM loading, quinn/rustls TLS configs, dev cert generation |
| `crates/transport` | lib | Connections to the server: QUIC, or the WebSocket-over-TLS fallback (a small stream multiplexer), and dialing with fallback |
| `crates/peer` | lib | End-to-end encrypted sessions between agent and viewer (Noise), and direct peer-to-peer paths: candidate gathering (host + STUN), hole punching, pinned QUIC, merging video from two paths |
| `crates/server` | bin + lib | QUIC listener for agents, HTTPS API, Postgres, auth, audit log, enrollment CA, update publishing |
| `crates/agent` | bin + lib | Enrollment, heartbeats with telemetry, protected credential storage, signed self-update, Windows service + session helper + tray, remote shell / scripts / file transfer |
| `crates/viewer` | bin + lib | Cross-platform remote desktop viewer (Linux, macOS, Windows): QUIC (or the WebSocket fallback) to the server, straight to the agent when NAT allows, end-to-end encrypted, OpenH264 decode, winit + softbuffer window with a toolbar (display mode, monitors) and a side panel (agent status, command buttons, file transfer) |
| `tui/` | Python project | Support TUI (Textual): sign in, live agent table, launch the viewer, shell console and script runner through the server ([tui/README.md](tui/README.md)) |

Server modules: `quic` (agent and viewer listeners: QUIC and the WebSocket
fallback), `api` (HTTPS routes; `api::rbac` for users, grants and groups),
`auth` (Argon2id, TOTP, sessions), `users`, `access` (RBAC: roles and
per-agent grants), `groups` (agent groups), `audit` (hash chain), `registry`
(agents, policies), `enroll` (tokens, internal CA), `updates` (signing,
publishing), `relay` (video fan-out to viewers, adaptive-bitrate feedback,
routing of sealed session records), `stun` (STUN responder for direct
paths), `viewers` (viewer-session tokens), `remote` (shell, script and
file-transfer relay to agents), `metrics` (Prometheus), `db` (pool,
migrations), `config`.

Agent modules:
- `core`: the connect/heartbeat/update loop, shared by console and service mode.
- `enroll`
- `credstore`: DPAPI on Windows, a dev-only encrypted file elsewhere.
- `telemetry`: sysinfo.
- `session`: the helper supervisor state machine.
- `update` (download and verify) and `updater` (rename-and-replace).
- `media`: NV12 conversion, H.264 fix-ups, the source link used for
  streaming, and the adaptive-bitrate controller (`rate`).
- `remote`: the agent's end of the remote shell (`pty`, `shell`), the script
  runner (`script`) and file transfer (`transfer`).
- `peers`: end-to-end encrypted sessions with viewers (handshake, sealed
  records, the media key) and their direct paths.
- `paths`
- `win` (Windows only): `service`, `process` (spawn into a session), `pipe`,
  `helper` (tray), `indicator` (on-screen session banner), `input_helper`
  (SYSTEM input injector), `acl`, `capture` (DXGI), `encoder` (Media Foundation),
  `stream` (capture worker), `bridge` (service ↔ helper media), `conpty`
  (PowerShell on a pseudoconsole).

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
server uses `ca.key` to sign agent certificates at enrollment. Issued
certificates carry subject and authority key identifiers, which strict
verifiers such as Python 3.13+'s default TLS context require. Certificates
generated before Phase 7 lack them: regenerate with `gen-certs --force` (and
re-enroll agents) if a Python client reports "Missing Authority Key
Identifier". If your
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
Admins and support engineers can also view desktops and use the remote shell,
script runner and file transfer; auditors cannot.

Stop with `docker compose down`, or `docker compose down -v` to also delete the
database volume.

### Compose settings

Put overrides in a `.env` file next to `docker-compose.yml`:

| Variable | Default | Purpose |
|---|---|---|
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `rmm` / `rmm-dev-password` / `rmm` | Database credentials. **Change the password outside local dev.** |
| `RMM_API_PORT` | `8443` | Host TCP port for the HTTPS API |
| `RMM_QUIC_PORT` | `4433` | Host UDP port for agents and viewers (QUIC) |
| `RMM_WS_PORT` | `4433` | Host TCP port for the WebSocket fallback. Publish it as `443` where firewalls only allow HTTPS out, and give clients `--ws-server host:443`. |
| `RMM_STUN_PORT` | `3478` | Host UDP port for the STUN responder (direct paths); also the port announced to clients |
| `RMM_METRICS_PORT` | `9464` | Prometheus `/metrics`, published on `127.0.0.1` only |
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
   # {"token":"9f3c…","expires_at":"…",
   #  "download_url":"https://localhost:8443/api/download/windows-x86_64?token=9f3c…",
   #  "msi_url":"https://localhost:8443/api/download/windows-x86_64/msi?token=9f3c…",
   #  "server":"localhost:4433","server_name":"localhost"}
   ```

   Each link mints a random, single-use token that expires after `ttl_secs`
   (default 24 h, max 7 days). Only its SHA-256 is stored. An admin can add
   `"group_ids": [..]` so the agent joins those [groups](#access-control-rbac-and-agent-groups)
   when it enrolls. The link downloads
   the latest published build for that platform (see
   [Publishing a signed update](#publishing-a-signed-update)). It keeps working
   until the token is used to enroll.

   `"server"` (`host:port`) and `"server_name"` say where an agent installed
   from the link's MSI connects, and which name the server certificate must
   match. The server defaults to this server's public host (from
   `RMM_PUBLIC_URL`) on port 4433. The name defaults to that host, unless the
   server is given as an IP address, in which case it stays the public host.
   Both are stored with the token. Windows links also get an `msi_url`: see
   [the MSI installer](#msi-installer).

2. **Enroll** on the target machine with the token and the server's CA
   certificate:

   ```sh
   agent enroll --server 203.0.113.10:4433 --server-name localhost \
     --server-ca ca.crt --token 9f3c…
   ```

   The agent generates its key pair locally and sends only a certificate
   signing request, over QUIC (or the [WebSocket fallback](#connectivity-quic-and-the-websocket-fallback))
   without a client certificate. The server checks
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

## MSI installer

`GET /api/download/windows-x86_64/msi?token=…` (a link's `msi_url`) returns
an MSI built on the fly. It holds the latest published Windows build, the CA
certificate agents must trust, and the link's server, TLS name and token as
properties. Double-click it (UAC prompts), or deploy it silently:

```powershell
msiexec /i rmm-agent.msi /qn /l*v install.log
```

It installs `rmm-agent.exe` and `ca.crt` to `C:\Program Files\RMM`, then runs
`rmm-agent.exe service install …` as LocalSystem, so the service enrolls
itself on first start and joins the link's groups. Nothing else is needed on
the machine. The token is a hidden property, so it shows as `**********` in
logs. Uninstall (Apps & features, or `msiexec /x rmm-agent.msi`) runs
`service uninstall`, then removes the files. The credential in
`%ProgramData%\RMM\agent` is kept, as with `service uninstall`, so
reinstalling reuses the same agent identity.

Like the plain download, the MSI needs a usable token but doesn't use it up;
enrolling does. So only the first machine installed from one link enrolls.

The server writes the MSI itself (`crates/server/src/msi.rs`, with the `msi`
and `cab` crates), so it needs no Windows tooling. Some limits:
- **x64 only**, with a fixed product code. On a machine that already has the
  agent from an MSI, Windows Installer refuses a second one ("another version
  of this product is already installed"). Agents update themselves instead.
- **Not Authenticode-signed:** SmartScreen may warn when it's opened from a
  browser download. The agent binary inside is covered by the update
  signature.
- **Patched `msi` crate:** `vendor/msi` is `msi` 0.10.0 with one fix. Windows
  Installer only opens databases whose rows are stored in key order, so see
  `vendor/msi/RMM-PATCH.md`.

`--server` accepts `host:port` as well as an IP address (for the MSI and by
hand). A name is resolved once, at install or enrollment, preferring IPv4.

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
│ - supervises the helpers     │ pipe  │                             │
│                              │       └─────────────────────────────┘
│                              │ spawn ┌─────────────────────────────┐
│                              │──────▶│ rmm-agent.exe input-helper  │
│                              │◀─────▶│ runs as SYSTEM, no windows  │
└──────────────────────────────┘ pipe  │ - injects technician input  │
                                       └─────────────────────────────┘
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
- **The input helper:** a second, window-less process that only injects the
  technician's input. Windows drops input from a lower integrity level aimed
  at a higher one (UIPI), so input from the user-level helper can't reach
  elevated windows, e.g. anything the user accepted a UAC prompt for. The
  input helper runs **as SYSTEM** in the user's session instead: the service
  duplicates its own token, moves it to that session, and starts
  `rmm-agent.exe input-helper` on `winsta0\default`. It's supervised like the
  helper. While it's connected, all input goes to it; if it isn't, input
  falls back to the user-level helper.
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
from the exact process IDs it just spawned; each helper starts suspended
until its ID is recorded. The input helper checks the other direction too:
since interactive users may create instances of the pipe, it refuses a
server that isn't the service's process ID (passed on its command line),
so a user can't get SYSTEM to inject input for them. For now the pipe carries status for the tray; screen
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
- input helper: `C:\ProgramData\RMM\agent\input-helper.log`

All are appended to with no rotation yet.

## Connectivity: QUIC and the WebSocket fallback

Agents and viewers connect to the server over **QUIC** (UDP) by default:
TLS 1.3, multiplexed streams, and connection migration across network
changes. Some networks block outbound UDP, so the server also accepts the
same protocol over a **WebSocket on TLS/TCP**:

- **Same port number, TCP.** The server listens for it on `--ws-listen`
  (default `0.0.0.0:4433/tcp`, next to QUIC on `4433/udp`), so one
  `host:port` reaches both. It can be published elsewhere (e.g. `443`); tell
  clients with `--ws-server`.
- **Same security.** The TLS layer uses the same server certificate and the
  same client-certificate check as QUIC (mutual TLS for agents), and the
  server runs the same code on the connection once it is up.
- **Same streams.** `transport::mux` carries QUIC-like bidirectional and
  unidirectional streams over one WebSocket, with per-stream flow control
  (256 KiB windows, so a stalled video or file stream never holds up the
  others), and the control stream's frames jump the queue ahead of bulk
  data. Heartbeats, video, input, shells, scripts and file transfers all
  work unchanged.
- **Automatic.** In `auto` mode (the default) a client tries QUIC for 5 s,
  then the WebSocket. An agent that had to fall back tries the WebSocket
  first for the next hour, so reconnects are quick, then gives QUIC another
  chance. A server that *refuses* a client (bad certificate or token) is
  not retried on the other transport.

Force one with `--transport quic|websocket|auto` (`RMM_TRANSPORT`); agents
store their transport settings with the credential at enrollment.
`GET /api/agents` shows each agent's `transport`, and the TUI marks
fallback agents `online (ws)`.

Trade-offs on the fallback: it is TCP, so a lost packet stalls every stream
behind it (head-of-line blocking), and it cannot migrate: a network change
drops the connection and the agent reconnects. Use QUIC wherever UDP is
allowed.

## Remote desktop viewing

Agents sit behind NAT and only connect *out*, so a session starts on the
**server's relay**: viewers connect to the server, which relays. Everything
in the session is **end-to-end encrypted** between agent and viewer, so the
relay forwards only ciphertext. When the network allows, the session then
moves to a **direct** connection between the two (see
[Direct connections](#direct-connections-nat-traversal)).

```text
 helper (user session)          service              server                viewers
 DXGI capture ─▶ MF H.264 ─pipe─▶ seal once ─ QUIC ──▶ relay ──┬─▶ viewer A (Linux)
 (dirty rects)   encode once      (media key)                   └─▶ viewer B (Windows)
                                     └──────── direct QUIC (hole-punched) ──▶ viewer C
```

The agent **encodes once**, whatever the number of viewers, and the server
fans the one stream out. Other points:
- **Start and stop:** capture starts when the first viewer arrives and stops
  when the last one leaves.
- **Joining mid-stream:** a viewer that joins late starts at a keyframe the
  server asks the agent for.
- **Slow viewers:** a viewer that falls behind skips to the next keyframe
  rather than slowing anyone else (on the relay, and on a direct path).
- **Monitor choice is shared:** picking a monitor changes it for everyone
  watching that agent, because there is only one stream.

### End-to-end encryption

The server authorises sessions but cannot read or alter them
(`peer::noise`, `protocol::e2e`):

- **Handshake.** `Noise_XX_25519_ChaChaPoly_BLAKE2s` (the `snow` crate),
  relayed by the server right after consent. The **viewer** makes a new
  X25519 key per connection and sends it with its viewer token; the server
  tells the agent which key belongs to the session it asked consent for
  (`SessionViewerKey`), and the agent completes the handshake with no
  other. The **agent** sends its enrolled certificate and a signature by the
  certificate's key over its agent id and Noise key; the viewer checks the
  chain against the CA it trusts, that the certificate names the agent it
  asked for, and the signature. `viewer::client::connect` only returns once
  this has succeeded.
- **Records.** Input, clipboard and direct-path signaling are sealed
  records (ChaCha20-Poly1305 under the handshake's per-direction keys, with
  explicit 64-bit counter nonces), relayed as opaque `Sealed` messages. The
  agent no longer accepts plaintext input or clipboard at all, so not even
  the server can inject keystrokes. Consent, the kill switch and "only
  granted sessions get input" are still enforced on the agent.
- **Video** is sealed **once** under a media key that the agent gives each
  viewer over its own session, so the relay can still fan one encode out.
  The relay reads only each frame's sequence number and keyframe flag (to
  start viewers on a keyframe), which the seal authenticates. The key
  changes whenever a viewer leaves.
- **Same keys on every path.** Nothing re-keys when the session moves
  between the relay and a direct path; records carry their own nonces, and
  the receiver restores their order across the two paths.

What the server still sees: who is in a session with which agent, when,
frame sizes and sequence numbers, and whether the session is relayed or
direct. Server-side session recording is therefore impossible by design.

Trade-off: the server runs the CA that issues agent certificates and
decides who may view, so it remains trusted for *authorisation*: a
malicious operator could mint a certificate for an agent id and route a
viewer to an impostor. What E2E removes is everything short of that: a
compromised relay process, a tapped network path or a leaked traffic
capture reveals nothing. Both ends must speak protocol 8: the server refuses
older viewers ("update it") and refuses to connect viewers to older agents
(which update themselves) rather than fall back to plaintext.

### Direct connections (NAT traversal)

Once the session is up on the relay, the viewer tries to reach the agent
directly (`peer::gather`, `peer::direct`):

1. **Candidates.** Each side binds a fresh UDP socket and gathers *host*
   candidates (its interface addresses) and a *server-reflexive* one: its
   public address as the server's STUN responder (`--stun-listen`, default
   `0.0.0.0:3478/udp`) sees it. No TURN: the relay is the fallback.
2. **Signaling** goes over the sealed session: the viewer's `DirectOffer`
   (candidates, and the SHA-256 of a self-signed certificate made for this
   attempt), then the agent's `DirectAnswer`.
3. **Hole punch.** Before answering, the agent sends a few packets to each
   viewer candidate with a **TTL of 2**: enough to open its own NAT, not
   enough to reach the viewer's NAT (which, if it tracks unsolicited
   packets as Linux does, would otherwise reserve the port pair and remap
   the viewer's traffic). The viewer's QUIC handshake then opens its own
   NAT and passes through the agent's.
4. **Direct QUIC**, mutually authenticated: each side pins the other's
   certificate hash from the sealed signaling. The viewer tries every agent
   candidate at once; the first to connect wins.
5. **Seamless switch.** The agent sends video down both paths; the viewer
   merges them by sequence number (`peer::merge`), so no frame is lost or
   repeated and no keyframe is needed. It then tells the agent (which stops
   sending through the relay once every viewer is direct) and the server
   (which stops forwarding to it, and records the path).
6. **Fallback.** If the direct path drops (6 s without packets, keep-alives
   every second), both sides return to the relay, which restarts the viewer
   on a keyframe; the session is unchanged. The viewer tries again after
   10 s, up to three times. A failed attempt is not retried.

This works through ordinary home and office NATs (which keep a socket's
public port the same whoever it sends to). Behind a symmetric NAT, or with
UDP blocked, the attempt fails and the session stays on the relay, which is
a permanent fallback rather than a scaffold. Adaptive bitrate keeps
working: viewers on a direct path acknowledge frames to the agent, which
judges their delay itself.

Turn it off for everyone with `--no-direct` on the server (no STUN
responder either), or per viewer with `viewer --no-direct`. The viewer's
title shows `encrypted, relayed` or `encrypted, direct`. The server records
only the path, never content: `session.path` audit entries (`direct`,
`relayed`, `direct_failed`), the final path in `viewer.disconnect`, and the
`rmm_viewers_by_path` / `rmm_session_path_changes_total` metrics.

### Adaptive bitrate

The one stream has to fit the agent's uplink *and* every viewer's downlink,
so the agent adjusts its encoder's bitrate to both, a few times a second:

- **Measurement.** The server acknowledges the agent's frames
  (`StreamReport`, every 200 ms while frames flow), and viewers acknowledge
  them to the server (`FrameAck`, every 100 ms). A frame's round trip above
  the recent minimum is time it spent queued: on the uplink, or in the
  slowest viewer's path. A viewer that stops acknowledging altogether (jammed
  link, stalled decoder) counts as falling behind.
- **Control** (`agent::media::rate`, AIMD like TCP): queueing over 250 ms
  cuts the target by 20% (50% over 1 s, and never above what the uplink is
  shown to deliver), at most every 500 ms; under 80 ms for a second grows
  it by 8%. It stays between 300 kbit/s and the resolution's full quality
  (4 Mbit/s at 1080p).
- **Encoder.** The service passes each new target to the session helper,
  which applies it to the running Media Foundation encoder
  (`CODECAPI_AVEncCommonMeanBitRate`); it carries over to monitor switches
  and resets when streaming stops.

It works the same on QUIC and the WebSocket fallback, and only agents and
viewers of protocol 7 or later take part (older ones keep a fixed bitrate).
The agent logs every change (`bitrate adapted bps=… reason=…
uplink_delay_ms=… viewer_delay_ms=…`); the server exports the viewer delay
it reports as `rmm_stream_viewer_delay_seconds`.

### Launching the viewer

1. Get a short-lived viewer token (admin or support engineer; auditors can't
   view screens). It is valid for **60 seconds** and works **once**:

   ```sh
   curl --cacert dev-certs/ca.crt -X POST -H "Authorization: Bearer $SESSION" \
     https://localhost:8443/api/agents/agt-c77b3326d49696ef/viewer-sessions
   # {"token":"…","expires_at":"…","agent_id":"agt-…","online":true}
   ```

2. Start the viewer with it (the TUI does both steps, and downloads the
   viewer from the server: see [Setting staff up](#setting-staff-up)):

   ```sh
   viewer --server 203.0.113.10:4433 --server-name localhost \
     --ca dev-certs/ca.crt --token <token>
   # or: RMM_VIEWER_TOKEN=<token> viewer --ca dev-certs/ca.crt
   ```

   In the window, mouse, wheel and keyboard go to the remote machine (keys
   are sent by physical position, so the remote keyboard layout applies).

   **Toolbar** (top): **Display** (drop-down: **Scale** fits the picture
   keeping its shape, **Stretch** fills the area ignoring it, **Fill** fills
   the area keeping it, cropping the edges, **Original size** shows one
   remote pixel per screen pixel: a remote screen larger than the window
   pans when the pointer is held near an edge, with bars along the right
   and bottom edges showing which part is in view), **FPS** (drop-down:
   **Auto**, **Max**, 120, 60, 30 or 15 frames a second; see below),
   **Monitor** (drop-down),
   **Text** (drop-down: the size of the toolbar's and panel's text, 8-20
   px; default 10), **Refresh** (a fresh keyframe), **Full screen**, and on
   the right
   **Panel** (show/hide the side panel) and **Disconnect**. In a narrow
   window, Full screen, Refresh and Text are left out (they have
   shortcuts). `--display scale|stretch|fill|original` picks the mode at
   startup, `--fps auto|max|N` (or `RMM_VIEWER_FPS`) the frame rate,
   `--no-panel` starts
   with the panel hidden, and `--font-size PX` (6-32, or
   `RMM_VIEWER_FONT_SIZE`) the text size. With `--remember-font-size
   FILE` a size picked in the viewer is saved there as `{"font_size": N}`;
   the TUI uses this to start each viewer at the size last picked.

   **Frame rate.** The agent captures when the screen changes, at most at
   the frame rate asked for: a still screen sends nothing (so the fps shown
   drops, without any delay to the next change), a video as many frames as
   the cap allows. **Auto** lets the agent choose 15, 30 or 60 by how well
   the network carries the video (the button shows its choice, e.g. "FPS:
   Auto (60)"); it starts at 30, moves up after a few seconds of full
   quality on clear links, and down when the links congest. **Max** is
   every change the screen shows. With several technicians watching one
   machine, the stream runs at the fastest rate any of them asked for. The
   bitrate ceiling rises with the frame rate, and each frame gets its share
   of it whatever the screen's real pace. Agents older than this ignore the
   setting (and stream at 30).

   On machines whose desktop is drawn in software (virtual machines,
   servers without a GPU), the agent reads screen pixels through GDI rather
   than from Desktop Duplication, whose image there can be read while
   Windows is still writing it (video arrives torn). That is tear-free but
   slower: expect fewer frames a second than on a machine with a GPU. The
   helper's log names the adapter and the source it chose ("screen capture
   source").

   **Side panel** (right): the agent's **status**: hostname, signed-in
   user, internal IP (the agent's own address on its route to the server),
   external IP (the address the server sees), DNS servers, OS, and every
   fixed disk with a usage bar (amber from 80%, red from 90%), refreshed
   every 5 s. Under it, **command buttons**, each starting its command on
   the agent's desktop as the signed-in user (`cmd`, `ncpa.cpl`, `mstsc`,
   ...: anything the Run dialog takes), and **Upload file** / **Download
   file**, which ask for the path on the agent and use the local file
   dialog for the other end (an upload onto an existing file asks before
   replacing it). The panel scrolls with the mouse wheel.

   The panel works through the HTTPS API as the technician, so it needs
   `--api-url https://host:8443` and the technician's session token in
   `RMM_API_TOKEN` (never on the command line). The TUI passes both, plus
   its command buttons as `--command "LABEL=COMMAND"` (repeatable;
   `--no-default-commands` for none). Without them the viewer works as
   before and the panel says where to get it. Command buttons need
   `desktop` on the agent and file transfer `file_transfer`; the server
   checks and audits both (`command.launch`, `file.upload`,
   `file.download`).

   The viewer's own shortcuts all use **Ctrl+Alt+Shift**:
   - **Ctrl+Alt+Shift+M** opens the monitor menu (then **1–9** picks,
     **Esc** closes it); **Ctrl+Alt+Shift+1–9** switches monitor directly.
   - **Ctrl+Alt+Shift+P** shows or hides the panel, **Ctrl+Alt+Shift+F**
     toggles full screen.
   - **Ctrl+Alt+Shift+-** / **Ctrl+Alt+Shift+=** make the text a pixel
     smaller or larger.
   - **Ctrl+Alt+Shift+F5** requests a fresh keyframe.
   - **Ctrl+Alt+Shift+Q** (or closing the window, or **Disconnect**) quits.

   While a menu or dialog is open, clicks and keys go to it rather than the
   remote machine; in a path prompt **Ctrl+V** pastes from the local
   clipboard.

   Under the `require` consent mode the viewer shows "Waiting for the remote
   user to accept" until the user answers (see
   [Consent and notifications](#consent-and-notifications)).

   `--monitor N` picks a monitor at startup. `--snapshot out.ppm --frames N`
   is headless: it decodes N frames, writes the last one, and exits. Use it
   for checks and on machines without a display. `--no-direct` keeps the
   session on the relay.

The server audits `viewer.session_create`, `viewer.connect` and
`viewer.disconnect` (the last with duration, frames the relay sent, and the
final path), and `session.path` whenever the session changes path. A token that
has expired or already been used is refused, as is a token for an agent that
isn't connected ("agent is not connected").

### Consent and notifications

Every session is gated by the device's **consent policy**, read from the
`device_policies` row for the agent (`GET`/`PUT /api/agents/{id}/policy`;
changes are audited as `policy.update`). The server sends the agent a
session request carrying the policy; the agent, which alone knows whether a
user is logged on, decides, and the session (video, input, clipboard)
starts only if the outcome allows it.

| Mode | User logged on | Nobody logged on |
|---|---|---|
| `require` | Blocking Yes/No prompt naming the technician. **Yes** → `granted`; **No** → `denied`; no answer within `consent_timeout_secs` → `timeout`. | `on_no_user`: `deny` → `consent_unavailable` (refused), `allow` → `bypassed_no_user` (starts) |
| `notify` | Starts at once; a toast names the technician → `notify` | Starts → `notify_no_user` |
| `unattended` | Starts silently, no prompt or toast → `unattended` | `unattended` |

**Defaults.** A new device starts as `notify`. When the agent first connects
it reports whether it is a workstation or a server (Windows product type):
workstations stay `notify`, servers become `unattended` (audited as
`policy.default` by `system`). Once an admin sets a device's policy, the
reported kind no longer changes it. `on_no_user` defaults to `deny`,
`consent_timeout_secs` to 30.

**What the user sees** (Windows, in the session helper):
- A toast "*technician* connected to this computer" for each `notify`
  session, including concurrent ones.
- The tray tooltip and menu list every connected technician (`notify` and
  `require` sessions; `unattended` ones are silent). The tray icon turns
  blue while anyone is connected.
- An **on-screen session indicator**: a small always-on-top banner at the
  top of the primary screen, "● Remote support session: *names* · Ctrl+F12
  to end", for as long as anyone is connected (same technicians as the
  tray). It is click-through and never takes focus, so it can't get in the
  user's way or be clicked by injected input, and it is excluded from screen
  capture (Windows 10 2004+) so it does not cover the technician's view.
- **Ctrl+F12** (or the tray's *End all remote sessions*) immediately ends
  every session. Viewers are closed with "the user ended the session", and
  each session is audited as `session.user_terminated` (outcome
  `user_terminated_session`). A technician cannot send Ctrl+F12 for the user:
  the agent drops that chord from remote input.

Every session's consent is audited as `session.start` with `mode`,
`outcome` and `started`. Refused viewers see the reason ("the user declined
the session", "the user did not respond to the consent prompt", "nobody is
available to approve the session"). An agent older than protocol 8 cannot
be viewed (it could not encrypt the session; before protocol 4 it could not
enforce consent either).

The agent enforces this itself too: it only injects input, applies
clipboard or streams for sessions it granted, and it releases any keys or
buttons a technician was holding when their session ends.

**Clipboard** syncs both ways while a session is active, sealed end to end
like input: text, and a file
list copied on the remote machine (sent as its paths, pasted as text on the
viewer; the files themselves are not transferred). Content over 1 MiB is not
synced, what was on either clipboard before connecting is never sent, and a
loop guard stops the two sides echoing a copy back and forth.

Input reaches elevated windows too: it's injected by the SYSTEM input helper
(see [Service and helper model](#service-and-helper-model)).

Known limits (planned for Phase 9, secure desktop): neither capture nor input
reaches UAC prompts or the lock/login screen, so the user has to accept a UAC
prompt locally. A locked workstation cannot answer a `require` prompt, so the
request times out.

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
- **Bitrate:** starts at full quality for the resolution (4 Mbit/s at
  1080p) and follows the network from there ([adaptive bitrate](#adaptive-bitrate)).
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

## Remote shell, scripts and file transfer

These three are **server-mediated and viewer-independent**: any authorized
client drives them through the HTTPS API, with no viewer and no remote
desktop session. The [support TUI](#support-tui) uses them the same way.

```text
 client (TUI, curl, …)            server                          agent
 WebSocket / HTTPS  ───────────▶  role check + audit  ──QUIC──▶  one new stream per operation
                                  (no files stored)              ConPTY, PowerShell, file IO
```

For each operation the server opens a new bidirectional QUIC stream on the
agent's existing connection (agents are behind NAT and never accept
connections). A large file transfer therefore never delays a shell, video or
heartbeats. The wire formats are in `crates/protocol/src/{shell,script,transfer}.rs`.

On a Windows agent all three run in the **service, as LocalSystem in
session 0**. They work with nobody logged on, need no session helper, and
have full control of the machine. Everything typed, run or written happens as
SYSTEM, and the logged-on user's desktop, mapped drives and profile are not
visible. They are **not gated by the consent policy**, which governs remote
desktop sessions only. They are gated by role and audited.

**Roles:** `admin` and `support_engineer` may use all three; `auditor` may
not. A refused request gets `403` and is audited as `permission.denied` with
the capability (`shell`, `script` or `file_transfer`), the user's role and the
agent. An agent that isn't connected gives `409 agent is not connected`. An
agent older than protocol 5 gives `409` and must update first.

### Interactive shell

`GET /api/agents/{id}/shell?cols=120&rows=30` with `Authorization: Bearer
<session>`, upgraded to a **WebSocket**. The server starts the shell on the
agent before completing the upgrade, so errors (offline, forbidden, no
ConPTY) arrive as ordinary HTTP responses.

- **Binary messages** are raw terminal bytes both ways: keystrokes in, and
  VT-encoded output out (render it with a terminal emulator widget).
- **Text messages** are JSON control:
  - client → server: `{"type":"resize","cols":132,"rows":43}`
  - server → client: `{"type":"started"}` first, then at the end
    `{"type":"exit","code":3}` or `{"type":"error","message":"…"}`, after
    which the server closes the socket. A malformed control message closes
    it with code 1008.

The agent runs Windows PowerShell (`powershell.exe -NoLogo`, by full path) on
a ConPTY, so colours, line editing and full-screen programs work. Closing the
WebSocket, or losing the agent connection, kills the shell. When the shell
ends, `ClosePseudoConsole` also ends any console programs it left running.
Dimensions are 1–1000.

On Linux and macOS agents (development only), the shell is `/bin/sh` on a
Unix pseudoterminal. That lets the whole path, resize included, be tested
without Windows.

### Script runner

`POST /api/script-runs`:

```sh
curl --cacert dev-certs/ca.crt -H "Authorization: Bearer $SESSION" \
  -H 'content-type: application/json' https://localhost:8443/api/script-runs \
  -d '{"agent_ids": ["agt-1", "agt-2"], "script": "Get-Service RmmAgent", "timeout_secs": 60}'
```

```json
{
  "run_id": 152,
  "summary": {"total": 2, "succeeded": 2, "failed": 0, "not_run": 0},
  "results": [
    {"agent_id": "agt-1", "status": "completed", "exit_code": 0,
     "stdout": "…", "stderr": "", "stdout_truncated": false,
     "stderr_truncated": false, "duration_ms": 431, "error": null},
    …
  ]
}
```

- `script` is a PowerShell script or a single command, up to 256 KiB.
  `timeout_secs` defaults to 300; the maximum is 3600. Up to 500 agents per
  run. Duplicate agent ids are dropped.
- The script runs on every agent at once, with no terminal. The call returns
  when every agent has finished, timed out or been found unreachable.
  Results come back in request order.
- `status`:
  - `completed`: see `exit_code`
  - `timed_out`: the process tree was killed; `exit_code` is null
  - `offline`
  - `unsupported`: the agent is too old
  - `failed`: it could not run; see `error`
- Summary: `succeeded` means exit 0, `failed` means it ran but exited
  non-zero or timed out, and `not_run` is everything else.
- stdout and stderr are kept up to 1 MiB each (marked `*_truncated`) and
  decoded as UTF-8.
- On Windows the script is written to a randomly named `.ps1` in the
  service's temp directory and run with `-NoProfile -NonInteractive
  -ExecutionPolicy Bypass`, with UTF-8 output. `exit N` sets the exit code,
  and an uncaught `throw` gives 1. The file is deleted afterwards.
- Timeouts kill the whole process tree (`taskkill /T` on Windows).
- On development agents the script runs with `/bin/sh -c`.

The run is audited twice. `script.run` is written before anything is sent:
who, the agents, the script's first line, its size and its SHA-256. Its row
id is the `run_id`. `script.complete` follows with every agent's status and
exit code.

### File transfer

**Upload:** `PUT /api/agents/{id}/files?path=<absolute path>&overwrite=false`,
with the file as the body. `Content-Length` and `x-content-sha256` (hex) are
required:

```sh
curl --cacert dev-certs/ca.crt -H "Authorization: Bearer $SESSION" \
  -H "x-content-sha256: $(sha256sum big.iso | cut -d' ' -f1)" -T big.iso \
  'https://localhost:8443/api/agents/agt-1/files?path=C:%5CTemp%5Cbig.iso'
# {"path":"C:\\Temp\\big.iso","size":…,"sha256":"…"}
```

The agent writes to a temporary file next to the target and checks the size
and SHA-256 against the manifest. Only then does it move the file into
place, so a failed or interrupted upload never leaves a partial file. It
refuses to replace an existing file unless `overwrite=true`, and needs the
parent directory to exist.

**Download:** `GET /api/agents/{id}/files?path=<absolute path>`. The agent
hashes the file first. The response carries `Content-Length`,
`x-content-sha256` and `Content-Disposition`. The server checks every byte
against that hash as it passes, and releases the last chunk only if the
whole file matches. A body that fails verification is cut short, so a client
never gets a complete-looking bad copy.

Neither side holds a file in memory: the data moves in 256 KiB chunks with
flow control end to end. On the test VM, a 512 MiB file went each way at
about 125–140 MB/s. The server peaked at 27 MiB of RAM and the agent service
at 22 MB. Resume after a dropped connection is not implemented; retry the
transfer.

| Status | Meaning |
|---|---|
| 400 | Bad path (relative, a directory), missing hash header |
| 403 | Role not allowed (audited), or the agent was denied access to the path |
| 404 | File or parent directory not found on the agent |
| 409 | File exists (upload without `overwrite`), agent offline or too old |
| 411 | No `Content-Length` |
| 422 | Size or SHA-256 does not match |
| 502 | The agent connection failed mid-transfer |

Each transfer that reaches an agent is audited as `file.upload` or
`file.download`: who, agent, path, bytes moved, size and SHA-256, and
`status` (`completed` or `failed` with the reason). A client that
disconnects mid-download is recorded as failed.

## Telemetry

Every heartbeat carries a health sample, collected with `sysinfo`:
- CPU %: whole machine, since the previous sample
- used/total RAM
- used/total bytes of the system disk (`%SystemDrive%`, or `/`)
- uptime

The server stores the latest values on the agent's row (`cpu_percent`,
`mem_used_bytes`, `mem_total_bytes`, `disk_used_bytes`, `disk_total_bytes`,
`uptime_secs`, `telemetry_at`). `GET /api/agents` returns them.

This changed the wire format, so the protocol version became 2. (It is 6
now: remote shell, scripts and file transfer need agents at version 5.) Phase 1–3
agents can't talk to this server until they update, which they still can,
because updates go over HTTPS.

Agents at protocol 6 also report their **hostname** once per connection
(`Message::AgentInfo`, sent after `DeviceInfo`), stored as `agents.hostname`.
Older agents keep working and show no hostname until they update.

Agents at protocol 9 also report their **status**: who is signed in (every
Windows session with a user, console or remote desktop, as `DOMAIN\user`;
utmpx logins elsewhere) and their own address on the route to the server
(the LAN address behind NAT). The server asks for it with
`EnableStatusReports` right after `Hello`, and only asks agents at version 9
or later; the agent then sends `AgentStatus` at once and again whenever it
changes (checked every heartbeat interval). An agent never sends it unasked,
so a new agent still works with an older server. The server cleans the names
(control characters, at most 32 names of 256 bytes) and stores them as
`agents.logged_in_users` and `agents.local_ip`. It also records the address it
sees the agent connect from as `agents.remote_ip` (the public address behind
NAT; behind Docker's userland proxy it is the proxy's address). All three
keep their last value while the agent is offline, and are `null` until known.

`GET /api/agents` adds live state from the relay to each row: `online`
(connected right now), `viewer_sessions` (remote-desktop viewers watching)
and `shell_sessions` (interactive shells open).

## Publishing a signed update

Agents only install updates signed with the ed25519 key whose public half was
baked in at build time.

### Windows builds without Windows

`scripts/build-windows-agent.sh` cross-compiles the Windows agent on Linux,
signs it, and publishes it:

```sh
scripts/build-windows-agent.sh            # version from crates/agent/Cargo.toml
scripts/build-windows-agent.sh 0.2.0      # or give one
scripts/build-windows-agent.sh --no-publish   # build and sign only
```

The first run builds `rmm-agent-windows-builder` from
`docker/agent-windows.Dockerfile`: Rust's `x86_64-pc-windows-msvc` target
with clang/lld via `cargo-xwin`, plus Microsoft's CRT and Windows SDK. Those
are downloaded into the image, and building it accepts Microsoft's license
for them. The image is about 3.6 GB. Each build mounts the source read-only,
bakes in `update-keys/update.pub`, and keeps its cache in the
`rmm-xwin-target` and `rmm-xwin-cargo` Docker volumes (a full release build
takes about a minute). The signed exe lands in
`target/windows-x86_64/rmm-agent.exe`. The signing key never enters the
container; signing and publishing run on the host. A cross-compiled agent
installs and runs like one built on Windows, as tested on Windows 11 via the
MSI.

### By hand

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
| `RMM_QUIC_LISTEN` | `--quic-listen` | `0.0.0.0:4433` | UDP address for agents and viewers (QUIC) |
| `RMM_WS_LISTEN` | `--ws-listen` | `0.0.0.0:4433` | TCP address for the [WebSocket fallback](#connectivity-quic-and-the-websocket-fallback) |
| `RMM_NO_WEBSOCKET` | `--no-websocket` | off | Disable the WebSocket fallback |
| `RMM_STUN_LISTEN` | `--stun-listen` | `0.0.0.0:3478` | UDP address of the STUN responder for [direct paths](#direct-connections-nat-traversal). Clients use the server's address at this port. |
| `RMM_STUN_ANNOUNCE_PORT` | `--stun-announce-port` | *(the listen port)* | STUN port to tell clients, if it is published on another port |
| `RMM_NO_DIRECT` | `--no-direct` | off | Keep every session on the relay; no STUN responder |
| `RMM_METRICS_LISTEN` | `--metrics-listen` | *(unset: off)* | Plain-HTTP address for Prometheus [metrics](#metrics) at `/metrics` |
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
| `RMM_SERVER` | `enroll --server` | `127.0.0.1:4433` | Server QUIC (UDP) address (stored at enrollment) |
| `RMM_TRANSPORT` | `enroll --transport` | `auto` | `auto`, `quic` or `websocket` (stored at enrollment) |
| `RMM_WS_SERVER` | `enroll --ws-server` | *(the `--server` address)* | TCP address of the WebSocket fallback, if published elsewhere (stored at enrollment) |
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
| POST | `/api/enrollment-links` | admin, support_engineer | `{ttl_secs?, platform?, group_ids?, server?, server_name?}` → `{token, expires_at, download_url, msi_url, server, server_name}`. `group_ids`: admins only. `msi_url`: Windows only. |
| POST | `/api/agents/{id}/viewer-sessions` | `desktop` on the agent | → `{token, expires_at, agent_id, online}`: single-use viewer token, valid 60 s |
| GET | `/api/download/{platform}?token=` | enrollment token | Latest published agent build. Does not use up the token. |
| GET | `/api/download/windows-x86_64/msi?token=` | enrollment token | [MSI](#msi-installer) that installs and enrolls the latest build. Does not use up the token. |
| GET | `/api/updates/{platform}/manifest` | none | `{platform, version, sha256, size}` |
| GET | `/api/updates/{platform}/binary` | none | Agent build (signed, so public) |
| GET | `/api/updates/{platform}/signature` | none | 64-byte detached ed25519 signature |
| GET | `/api/ca` | none | The CA certificate (PEM) that signed the server's and agents' certificates. The TUI offers it for trust at first sign-in and gives it to the viewer. |
| GET | `/api/viewer/{platform}/manifest` | none | `{platform, version, sha256, size}` of the published viewer build |
| GET | `/api/viewer/{platform}/binary` | none | The viewer build the TUI downloads |
| GET | `/install` | none | The [install page](#setting-staff-up) for staff (`/` redirects to it) |
| GET | `/install/{wheel}` | none | The published TUI wheel, under its file name |
| GET | `/install/viewer/{platform}` | none | The published viewer, as a download |
| GET | `/api/me` | session | Current user |
| GET | `/api/agents` | session | The agents the user can see (engineers: granted ones), with hostname, telemetry, `logged_in_users` / `local_ip` / `remote_ip` / `os` / `dns_servers` / `disks`, `classification` (and `classification_override`), `groups`, the user's `capabilities` on each, and live `online` / `transport` / `viewer_sessions` / `shell_sessions` |
| GET | `/api/agents/{id}` | session; agent visible to the user | One agent, as listed above (the viewer's side panel) |
| PUT | `/api/agents/{id}/classification` | admin | `{classification: "server" \| "desktop" \| "other" \| null}` (`null`: back to the device kind's) → `{classification, classification_override}`; audited as `agent.classify` |
| POST | `/api/agents/{id}/launch` | `desktop` on the agent | `{command}` (as typed at a Run prompt) → `{command, user}`: started on the agent's desktop as the signed-in user; 409 if nobody is signed in. Audited as `command.launch` |
| GET | `/api/audit?limit=` | admin, auditor | Newest audit entries first (default 100, at most 1000) |
| GET | `/api/audit/verify` | admin, auditor | Walks the whole chain: `{"status":"valid","entries":n}` or `{"status":"broken","id":…,"reason":…}` |
| GET | `/api/agents/{id}/policy` | session; agent visible to the user | Consent policy |
| PUT | `/api/agents/{id}/policy` | admin | `{consent_mode, on_no_user, consent_timeout_secs}` |
| GET | `/api/agents/{id}/shell?cols=&rows=` | `shell` on the agent | WebSocket: interactive PowerShell ([details](#interactive-shell)) |
| POST | `/api/script-runs` | `script` on every target | `{agent_ids?, group_ids?, script, timeout_secs?}` → per-agent results ([details](#script-runner)). Groups are expanded when the run starts. |
| PUT | `/api/agents/{id}/files?path=&overwrite=` | `file_transfer` on the agent | Upload the body; needs `x-content-sha256` ([details](#file-transfer)) |
| GET | `/api/agents/{id}/files?path=` | `file_transfer` on the agent | Download, with `x-content-sha256` |
| GET | `/api/users` | admin | Users and roles |
| POST | `/api/users` | admin | `{username, password, role}` → `{user, totp_secret, otpauth_url}` (201) |
| PUT | `/api/users/{id}/role` | admin | `{role}`; refused (409) if it would leave no admin |
| PUT | `/api/users/{id}/password` | admin | `{password}`; ends the user's sessions (an admin changing their own keeps the one in use) |
| POST | `/api/users/{id}/totp` | admin | New TOTP secret → `{user, totp_secret, otpauth_url}`; ends the user's sessions likewise |
| DELETE | `/api/users/{id}` | admin | Deletes the user, their sessions and grants; not yourself, not the last admin |
| GET | `/api/grants?user_id=` | admin; anyone for their own `user_id` | Access grants |
| POST | `/api/grants` | admin | `{user_id, agent_id \| group_id \| all_agents: true, capabilities?}` (201). Capabilities default to all four. |
| DELETE | `/api/grants/{id}` | admin | Revoke a grant |
| GET | `/api/groups` | session | Groups, with the members the user can see |
| POST | `/api/groups` | admin | `{name, description?, agent_ids?}` (201) |
| GET / PATCH / DELETE | `/api/groups/{id}` | GET: session; else admin | PATCH `{name?, description?}` |
| PUT / POST | `/api/groups/{id}/agents` | admin | `{agent_ids}`: PUT sets the members, POST adds |
| DELETE | `/api/groups/{id}/agents/{agent_id}` | admin | Remove one member |

Login is two steps:
1. A correct password returns a 5-minute challenge token.
2. A correct TOTP code swaps the challenge for a session token.

Each TOTP code works only once. After 5 wrong codes the challenge is discarded
and the user has to enter their password again.

Every login attempt (successful or not), every logout, user creation, policy
change, download-link creation and agent enrollment is written to the
hash-chained `audit_log` table. So is every remote shell (`shell.open`,
`shell.close`), script run (`script.run`, `script.complete`), file transfer
(`file.upload`, `file.download`), refused operation (`permission.denied`),
and every RBAC change (`user.role_change`, `user.password_change`,
`user.totp_reset`, `user.delete`, `grant.create`,
`grant.delete`, `group.create`, `group.update`, `group.delete`,
`group.members`). Chain verification is `server::audit::verify`, exposed as
`GET /api/audit/verify`.

## Access control (RBAC) and agent groups

The **role** says what kind of thing a user may do; for support engineers,
**grants** say where:

| Role | Sees | May do |
|---|---|---|
| `admin` | every agent | everything, including managing users, grants and groups |
| `support_engineer` | only agents a grant covers | only what those grants allow |
| `auditor` | every agent, and the audit log | nothing: read-only |

A **grant** gives one support engineer a set of **capabilities** on one
agent, on every agent in one group, or on all agents:

| Capability | Allows |
|---|---|
| `desktop` | Remote desktop: viewer tokens, and so input and clipboard |
| `shell` | Interactive shell |
| `script` | Script runs |
| `file_transfer` | Upload and download |

Grants add up and there are no deny rules. A group grant follows the
group's membership as it changes, and an all-agents grant covers agents
enrolled later. Access is checked when a viewer token is minted *and* when
it is used, so revoking a grant stops a session that hasn't started yet. A
multi-agent script run needs `script` on every target; otherwise nothing
runs. Agents a user cannot see don't exist for them (404, and absent from
the agent list), and every refusal is audited as `permission.denied`.

**Groups** are named sets of agents; an agent can be in any number of them.
Use them to grant access to many agents at once, to run a script on a
whole group (`"group_ids"` in `/api/script-runs`), and to have new agents
join groups as they enroll (`"group_ids"` on an enrollment link, admins
only, since membership extends grants).

**Upgrading:** the migration gives every existing support engineer an
all-agents grant with every capability, so nobody is locked out; narrow
them from there. Engineers created afterwards start with no access.

```sh
# A group, and a support engineer who may view and script its machines:
curl … -d '{"name":"Branch office","agent_ids":["agt-1","agt-2"]}' https://…/api/groups
curl … -d '{"user_id":7,"group_id":1,"capabilities":["desktop","script"]}' https://…/api/grants
```

## Metrics

With `--metrics-listen` set (Compose: `127.0.0.1:9464`), the server serves
Prometheus metrics in the OpenMetrics text format at `/metrics`, over plain
HTTP and without credentials. Keep it on a private address. It shows
activity levels, never agent ids, user names or content, and label values
come from small fixed sets so the number of series stays bounded.

| Metric | Type | Labels |
|---|---|---|
| `rmm_agents_connected` | gauge | `transport` (`quic`, `websocket`) |
| `rmm_viewers_connected` | gauge | `transport` |
| `rmm_agents_registered` | gauge | |
| `rmm_connections_rejected_total` | counter | `reason` |
| `rmm_agent_heartbeats_total` | counter | |
| `rmm_enrollments_total` | counter | `result` |
| `rmm_relay_frames_received_total`, `rmm_relay_received_bytes_total` | counter | |
| `rmm_relay_frames_sent_total`, `rmm_relay_sent_bytes_total` | counter | |
| `rmm_relay_frames_dropped_total` | counter | (frames skipped for a lagging viewer) |
| `rmm_stream_viewer_delay_seconds` | histogram | (worst viewer delay reported to agents) |
| `rmm_viewers_by_path` | gauge | `path` (`relayed`, `direct`) |
| `rmm_session_path_changes_total` | counter | `path` (`relayed`, `direct`, `direct_failed`), as viewers report |
| `rmm_relay_sealed_records_total` | counter | (end-to-end sealed records relayed; never readable by the server) |
| `rmm_stun_requests_total` | counter | |
| `rmm_sessions_total` | counter | `mode`, `outcome` (consent) |
| `rmm_user_terminations_total` | counter | (Ctrl+F12) |
| `rmm_shells_open` | gauge | |
| `rmm_script_runs_total`, `rmm_script_agent_results_total` | counter | `status` |
| `rmm_file_transfer_bytes_total` | counter | `direction` |
| `rmm_logins_total` | counter | `result` |
| `rmm_permission_denied_total` | counter | `capability` (or `admin`) |
| `rmm_audit_entries_total` | counter | |
| `rmm_http_requests_total` | counter | `method`, `route` (template), `status` |
| `rmm_http_request_duration_seconds` | histogram | `method`, `route` |
| `rmm_db_connections`, `rmm_db_connections_idle` | gauge | |
| `rmm_build_info` | gauge | `version` |

## Support TUI

`tui/` is the support engineers' terminal app (Python + Textual). It talks
only to the server's HTTPS API:

- sign in with username, password and TOTP; the session token is kept in the
  OS keyring, so it stays signed in across launches;
- a live table of agents: hostname, status (`online (ws)` on the WebSocket
  fallback), groups, last seen, CPU, RAM, disk and active sessions;
- **remote desktop:** mints a viewer token and starts the native viewer
  (`crates/viewer`) as a separate process. The TUI doesn't render video;
- **shell console** and **script runner** (agents and/or whole groups),
  which use the server's shell WebSocket and `/api/script-runs` directly.
  **Neither needs the viewer**;
- actions follow the user's per-agent capabilities: an engineer only gets
  remote desktop, shell or scripts where a grant allows them;
- an audit-log view with chain verification for admins and auditors.
  Auditors see agents and the audit log but get none of the control actions;
- **new agent:** makes a download link (MSI for Windows) for a chosen server
  address, optionally into groups (admins), and can save the MSI locally;
- **groups:** everyone can view them and their members; admins create,
  rename, delete and set members. The agent table filters by group.

Install, configuration and usage: [tui/README.md](tui/README.md).

### Setting staff up

Staff need no config file, no certificate and no viewer build. Send a new
member of staff to the server's install page, `https://<server>:8443/install`
(the site root redirects there): it has the TUI download, the install
commands for Linux and Windows, the server address to type, and the CA
fingerprint the TUI will show. To fill it:

```sh
scripts/build-clients.sh            # viewers and the TUI wheel → ./updates
```

That builds the viewer for `linux-x86_64` and `windows-x86_64` in Docker
(Linux against Debian 12's glibc, so it runs on current desktops; Windows
with the agent's cross toolchain) and publishes both with `server
publish-viewer`, next to the agent builds: `updates/<platform>/{viewer,
viewer.json}`. It also builds the TUI as a wheel and publishes it with
`server publish-tui` as `updates/tui/<wheel>`. The wheel holds nothing about
your server. The running server picks all of it up without a restart.

The page needs no sign-in (it holds nothing secret). With a private CA the
browser warns about the certificate before showing it, so give staff the
fingerprint yourself as well: `gen-certs` prints it, and the server logs it
at start (`CA certificate fingerprint`).

On first sign-in the TUI:
- fetches the server's CA certificate (`GET /api/ca`) and asks the user to
  accept its SHA-256 fingerprint, then pins it for that server, as SSH does
  with host keys;
- downloads the viewer for its platform (`GET /api/viewer/<platform>/…`),
  checks it against the manifest's SHA-256, and re-downloads whenever a new
  build is published, so viewers follow the server.

Viewer builds are not signed with the update key (a signature valid for a
viewer would also pass an agent's update check). They are trusted as the
server is: over TLS verified against the accepted CA.

The server certificate must cover the name staff type (`gen-certs --san`).

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
`telemetry.rs`, `streaming.rs`, `consent.rs`, `remote.rs`, `rbac.rs`,
`metrics.rs`, `direct.rs` and `nat.rs`)
each start a throwaway `postgres:17-alpine` container with testcontainers. The
container is removed when the test ends. `heartbeat.rs` (QUIC and the
WebSocket fallback over loopback, including falling back from a UDP port
that drops everything) and
`updates.rs` (signed updates over HTTPS) need no database. `streaming.rs` runs a
real agent session with a synthetic two-monitor source (OpenH264 standing in
for DXGI + Media Foundation), the real relay, and two real viewer clients. It
checks:
- both viewers decode frames from **one** encode (one `Start`, byte-identical
  frames);
- a monitor switch applies to both;
- the agent stops capturing when both leave;
- viewer tokens are single-use and role-checked;
- the same over the WebSocket fallback;
- a viewer that stops keeping up makes the agent lower its bitrate.

`remote.rs` drives the shell, script and file APIs with plain HTTPS and
WebSocket clients against two real agents, one on QUIC and one on the
WebSocket fallback. No viewer is used. It checks:
- an interactive shell with resize and exit code;
- a script fanned out to both agents, plus an offline one;
- a 24 MiB upload and download with hash checks;
- corrupted, short and conflicting transfers leave no file;
- an auditor is refused and audited on all three.

`rbac.rs` covers roles, agent/group/all-agents grants, groups, user
administration and group-assigning enrollment links; `metrics.rs` scrapes
`/metrics` after real traffic.

`direct.rs` runs end-to-end encryption and direct paths on loopback (a real
agent, server with STUN, and viewers). It checks:
- a session starts on the relay and moves to a direct path with every frame
  delivered once and in order, no decode error and no keyframe; the same
  handshake throughout (no re-key); metrics and the audit log record
  `direct`; the relay stops receiving video; input and clipboard still arrive;
- the relay handles only ciphertext: no plaintext H.264, clipboard text or
  keystroke appears in anything it relayed;
- when punching fails (the agent advertises a black hole) the session stays
  on the relay and everything still works; `direct_failed` is recorded;
- a dropped direct path falls back to the relay without a new handshake;
- `--no-direct` on the server keeps sessions relayed.

`nat.rs` (Linux) puts the agent and the viewer behind **two real NATs**:
it re-runs itself in an unprivileged user + network namespace, builds two
LANs (both `192.168.1.0/24`) behind nftables masquerade NATs, runs the
server on the "internet" between them, and runs agent and viewer on threads
inside their LANs' namespaces. Behind ordinary NATs the session must go
direct to NAT A's public address, confirmed by the server's metrics, and
video must leave the relay; behind symmetric NATs (`masquerade
random,fully-random`) it must stay on the relay and keep streaming. It needs
unprivileged user namespaces, `ip`, `nft` and `nsenter`, and prints
`SKIPPED` without them.

It runs the agents' development fallbacks (`/bin/sh`), so it runs on Linux
and macOS only.

The TUI has its own checks (no server or Docker needed; the API is mocked):

```sh
cd tui
python -m venv .venv && .venv/bin/pip install -e '.[dev]'
.venv/bin/ruff check src tests && .venv/bin/ruff format --check src tests
.venv/bin/pytest
```

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

The Windows agent tests include real ConPTY PowerShell (resize, exit code)
and the PowerShell script runner (output, UTF-8, `throw`, timeout tree-kill).

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
