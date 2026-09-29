# rmm-tui: support TUI

A terminal app for support engineers. It runs on Linux, macOS and Windows,
and talks only to the RMM server's HTTPS API. It never connects to agents
directly. What it does:

- **Sign in** with username, password and TOTP. The session token is stored
  in the OS keyring, so you stay signed in across launches.
- **Agents:** a live table showing hostname, status, last seen, CPU, RAM,
  disk and active sessions.
- **Remote desktop:** starts the native viewer for the selected agent. The
  TUI doesn't render video itself. Quitting the TUI closes every viewer
  window it opened.
- **Shell console:** interactive PowerShell on the agent, in a pane. Needs
  **no viewer**.
- **Script runner:** runs a command or script on one or more agents, or on
  whole agent groups, and shows each agent's stdout, stderr and exit code.
  Also needs **no viewer**.
  Saved scripts live in a small local library.
- **Audit log:** recent entries and a chain check (admins and auditors).

## Install

Python 3.11 or newer. With [uv](https://docs.astral.sh/uv/):

```sh
cd tui
uv venv && uv pip install -e .          # add '.[dev]' for the tests
.venv/bin/rmm-tui --help
```

With pip:

```sh
cd tui
python -m venv .venv
.venv/bin/pip install -e .              # Windows: .venv\Scripts\pip
.venv/bin/rmm-tui
```

`python -m rmm_tui` works too. Dependencies: `textual`, `httpx`, `keyring`,
`websockets` (the shell), `pyte` (terminal emulation for the shell pane).

The keyring uses Windows Credential Manager, the macOS Keychain, or Secret
Service on Linux (GNOME Keyring or KWallet). If no keyring is available, the
TUI still works, but you have to sign in on each launch; it tells you when
this happens.

## Configuration

Settings come from a TOML file, and command-line flags override them. The
file lives at:

| OS | Default path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/rmm-tui/config.toml` (usually `~/.config/rmm-tui/config.toml`) |
| macOS | `~/Library/Application Support/rmm-tui/config.toml` |
| Windows | `%APPDATA%\rmm-tui\config.toml` |

Pass `--config PATH` or set `RMM_TUI_CONFIG` to use a different file.

```toml
server_url = "https://rmm.example.com:8443"   # the HTTPS API
ca_path = "/etc/rmm/ca.crt"                   # CA to trust for the server
viewer_path = "/opt/rmm/viewer"               # native viewer binary
quic_addr = "rmm.example.com:4433"            # optional, see below
poll_interval = 5                             # seconds between agent refreshes
scripts_path = "~/rmm-scripts.json"           # optional; default is in the data dir
```

| Setting | Flag | Default | |
|---|---|---|---|
| `server_url` | `--server-url` | `https://localhost:8443` | Must be `https://`. |
| `ca_path` | `--ca` | *(system trust store)* | PEM CA certificate. Needed for the dev CA (`dev-certs/ca.crt`), and always needed to launch the viewer. |
| `viewer_path` | `--viewer` | `viewer` (on `PATH`) | E.g. `target/release/viewer`. |
| `quic_addr` | `--quic-addr` | API host, port `4433` | The server's QUIC listener, for the viewer. The name is resolved to an address (IPv4 preferred), and the viewer checks the certificate against the name. |
| `poll_interval` | | `5` | At least 1. |
| `scripts_path` | | data dir `/scripts.json` | The saved-script library. |

Relative paths in the file are relative to the file. Unknown keys are an
error, so typos get caught.

TLS is always verified: against `ca_path` if set, otherwise against the
system trust store. There is no option to turn verification off.

Logs go to the data directory (`~/.local/share/rmm-tui/`,
`~/Library/Application Support/rmm-tui/` or `%LOCALAPPDATA%\rmm-tui\`), never
to the terminal: `rmm-tui.log` for the TUI and `viewer.log` for viewers it
launches.

## Running

```sh
# Against a dev server started from the repo root (see the main README):
rmm-tui --server-url https://localhost:8443 --ca ../dev-certs/ca.crt \
        --viewer ../target/debug/viewer
```

On start, the TUI checks the saved session with `GET /api/me`:
- If the server accepts it, you go straight to the agent table.
- If the token has expired or the server rejects it, the token is removed and
  you get the login screen.
- If the server can't be reached, the token is kept.

The server has no refresh endpoint, so a session lasts the server's
`RMM_SESSION_TTL_SECS` (12 h by default); then you sign in again. `l` signs
out (the server revokes the session and the token is removed from the
keyring). `rmm-tui --logout` only forgets the saved token locally; the
server-side session then expires on its own.

### Keys

**Agent table**

| Key | Action |
|---|---|
| `d` or `Enter` | Remote desktop: launch the viewer for the selected agent |
| `s` | Shell console on the selected agent |
| `r` | Script runner (the selected agent is pre-selected) |
| `a` | Audit log (admins and auditors) |
| `F5` | Refresh now (the table also refreshes every `poll_interval`) |
| `l` | Sign out |
| `q` | Quit |

The footer shows only the actions you may take on the **selected agent**. A
**support engineer** only sees the agents an admin has granted them, and
only gets remote desktop, shell or scripts where their grant covers that
(the server says so per agent). An **auditor** sees the agent table and the
audit log, but no control actions. The server enforces the same rules: a
refused request returns 403 and is audited as `permission.denied`.

The **Status** column says `online (ws)` for an agent connected over the
WebSocket fallback (its network blocks UDP), and **Groups** lists its agent
groups.

**Shell console:** type a command in the input box and press Enter. The
output pane is a VT terminal emulator, so colours and cursor movement render
correctly, and scrollback is kept.
- `Ctrl+C` sends an interrupt to the remote shell.
- `Up`/`Down` recall previous commands.
- `Esc` closes the console and ends the shell.
- The PTY follows the pane's size, so resizing your terminal resizes it too.

**Script runner:**
1. Tick targets with `Space`: agents (only those you may run scripts on),
   and/or groups. A group's members are decided by the server when the run
   starts, so an agent that joined it meanwhile is included.
2. Type a script, or load a saved one with `Enter`.
3. Optionally set a timeout (1–3600 s; default 300).
4. Press `Ctrl+R`.

Each agent then shows its status, exit code and duration. The highlighted row
shows its stdout and stderr. Name a script and press **Save** to keep it, or
**Delete** to remove the highlighted one. The library is plain JSON.

A run is one request for all selected agents, so the server audits it as a
single run (`script.run` / `script.complete`, with one `run_id`). Results
appear together when the slowest agent finishes; until then every target
shows *running…*.

### Remote desktop

The TUI asks the server for a single-use viewer token (valid 60 s), then
starts:

```text
viewer --server <ip:port> --server-name <host> --ca <ca_path>
```

The token is passed in the `RMM_VIEWER_TOKEN` environment variable, not on
the command line, so other local users can't read it from the process list.
The viewer runs detached from the TUI, and you can open several at once.

## Tests

```sh
pip install -e '.[dev]'
pytest               # API client (mocked), keyring flow, script results,
                     # shell protocol + terminal, viewer command, headless UI
ruff check src tests && ruff format --check src tests
```

The tests don't need a server, keyring or display. The API is served by an
`httpx.MockTransport`, the keyring is an in-memory stand-in, and the UI runs
headless under Textual's test pilot.
