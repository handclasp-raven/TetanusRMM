# rmm-tui: support TUI

A terminal app for support engineers. It runs on Linux, macOS and Windows,
and talks only to the RMM server's HTTPS API. It never connects to agents
directly. What it does:

- **Sign in** to any server with username, password and TOTP. The session
  token is stored in the OS keyring, so you stay signed in across launches.
- **Agents:** a live table with a green/red online/offline dot, and columns
  you choose: hostname, status, logged-in user, IP address, public IP,
  uptime, groups, last seen, CPU, RAM, disk, active sessions, type and agent
  ID.
- **Remote desktop:** starts the native viewer for the selected agent. The
  TUI doesn't render video itself. Quitting the TUI closes every viewer
  window it opened. The viewer's side panel shows the agent's status and
  has command buttons (`cmd`, `ncpa.cpl`, `mstsc`, ...) that you choose in
  the TUI (`v`), plus file upload and download.
- **Shell console:** interactive PowerShell on the agent, in a pane. Needs
  **no viewer**.
- **Script runner:** runs a command or script on one or more agents, or on
  whole agent groups, and shows each agent's stdout, stderr and exit code.
  Also needs **no viewer**.
  Saved scripts live in a small local library.
- **Audit log:** recent entries and a chain check (admins and auditors).
- **New agent:** a single-use download link for installing an agent. On
  Windows that's an MSI which installs and enrolls it unattended, optionally
  straight into agent groups. The MSI can be saved from the TUI.
- **Groups:** view groups and their members. Admins can also create, rename,
  delete and fill them. The group list beside the agent table filters it,
  and the search box above it filters by hostname.
- **Users** (admins only): add and remove users, change their role, choose
  which agents a support engineer may work on, and set their password or
  reset their TOTP secret.

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
viewer_font_size = 9                          # optional; the viewer's text size
```

| Setting | Flag | Default | |
|---|---|---|---|
| `server_url` | `--server-url` | `https://localhost:8443` | Must be `https://`. |
| `ca_path` | `--ca` | *(system trust store)* | PEM CA certificate. Needed for the dev CA (`dev-certs/ca.crt`), and always needed to launch the viewer. |
| `viewer_path` | `--viewer` | `viewer` (on `PATH`) | E.g. `target/release/viewer`. |
| `quic_addr` | `--quic-addr` | API host, port `4433` | The server's QUIC listener, for the viewer. The name is resolved to an address (IPv4 preferred), and the viewer checks the certificate against the name. |
| `poll_interval` | | `5` | At least 1. |
| `scripts_path` | | data dir `/scripts.json` | The saved-script library. |
| `viewer_font_size` | | *(viewer default, 10)* | Text size of the viewer's toolbar and side panel, in pixels (6–32), until you pick one in a viewer (**Text** menu, or `Ctrl+Alt+Shift+-`/`=`). From then on viewers start at the size you last picked, remembered in `viewer.json` in the data directory; delete that file to go back to this setting. |

Relative paths in the file are relative to the file. Unknown keys are an
error, so typos get caught.

TLS is always verified: against `ca_path` if set, otherwise against the
system trust store. There is no option to turn verification off.

The TUI also remembers the server you last signed in to and your agent-table
columns, in `state.json` in the data directory.

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

The server is `--server-url` if given, otherwise the one you last signed in
to, otherwise `server_url` from the config file. The login screen shows it in
the **Server URL** field, and you can type another. `https://` is assumed if
you leave it out. Each server keeps its own saved session, and the viewer's
`quic_addr` is only kept when the new server has the same host.

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
| `n` | New agent: download link / MSI (admins and support engineers) |
| `/` | Search by hostname (`Enter`: back to the table, `Esc`: clear) |
| `f` | Jump to the group list |
| `k` | Classify the selected agent as server, desktop or other (admins) |
| `g` | Groups |
| `u` | Users: accounts, roles, access and passwords (admins; hidden otherwise) |
| `a` | Audit log (admins and auditors) |
| `c` | Choose columns |
| `v` | Viewer buttons: the remote viewer's command buttons |
| `F5` | Refresh now (the table also refreshes every `poll_interval`) |
| `l` | Sign out |
| `q` | Quit |

The footer shows only the actions you may take on the **selected agent**. A
**support engineer** only sees the agents an admin has granted them, and
only gets remote desktop, shell or scripts where their grant covers that
(the server says so per agent). An **auditor** sees the agent table and the
audit log, but no control actions. The server enforces the same rules: a
refused request returns 403 and is audited as `permission.denied`.

The **Status** column has a green dot for online and a red one for offline
(grey: revoked). It says `online (ws)` for an agent connected over the
WebSocket fallback (its network blocks UDP). **Groups** lists its agent
groups.

**Columns** (`c`): tick columns with `Space`, reorder with `Shift+↑`/`Shift+↓`
(or the ▲/▼ buttons), then **Apply**. **Defaults** restores the standard
set. Hidden by default: Public IP and Agent ID.
- **Classification:** server, desktop or other. It follows what the agent
  detects (a Windows Server edition, or a headless machine, is a server;
  anything else a desktop; `Other` until the agent has reported) until an
  admin sets it with `k`; then it is marked `*` (e.g. `Server*`). Choosing
  **Automatic** hands it back to the agent. Changes are audited as
  `agent.classify`.
- **OS:** the operating system and version the agent reports, e.g.
  `Windows 11 Pro (build 26100)`.
- **Logged-in user:** everyone signed in to the machine (console or remote
  desktop, including disconnected sessions), or `none`.
- **IP address:** the agent's own address on its route to the server (its
  LAN address behind NAT).
- **Public IP:** the address the server sees it connect from.

Logged-in user and uptime show `–` while an agent is offline, since the last
values are stale. Logged-in user, IP address and OS need agents at protocol 9;
older agents show `–` until they update.

**Search:** type in the box above the table (`/` jumps to it) to show only
agents whose hostname contains the text, ignoring case.

**Group list:** the list beside the table (`f` jumps to it) shows all
agents, those in no group, or one group's members, each with its agent
count; moving through it filters the table at once, and `Enter` goes back to
the table. It works together with the search, holds across refreshes, and
goes back to all if the group is deleted. The status line then says e.g.
`3 of 12 agents`.

**New agent** (`n`):
1. Pick the platform, and check **Server address** (`host:port` the agent
   connects to) and **TLS name** (a name on the server's certificate). They
   default to the server this TUI talks to: `quic_addr` if set, else its host
   on port 4433, and the host name you signed in with. Change the address
   when agents reach the server differently from you, e.g. an IP on their
   network.
2. Choose how long the link stays valid (1 hour, 24 hours or 7 days). As an
   admin, you can also tick groups the agent joins when it enrolls.
3. Press **Create link** (`Ctrl+G`).

You then get:
- **MSI link** (Windows): anyone with it can download an installer that
  installs the agent service and enrolls it. Run it by double-clicking, or
  `msiexec /i rmm-agent.msi /qn`.
- **Save MSI:** downloads it to the path shown (default
  `~/Downloads/rmm-agent.msi`, never overwriting).
- **Agent binary** link and the matching manual install command.

**Copy** puts a value on the clipboard (via your terminal, OSC 52). A link is
single use: once an agent enrolls with it, it's dead.

**Groups** (`g`): the groups, their descriptions and counts, and the
highlighted group's members with online dots. Admins get:
- `n` new
- `e` rename / change description
- `m` members: tick agents with `Space`
- `Del` delete: its agents stay but lose access granted through it

**Users** (`u`, admins only): every user with their role and what they can
reach.
- `n` new user: username, password (at least 12 characters) and role. The
  new **TOTP secret** and its `otpauth://` URL are then shown **once**, to
  pass to the user for their authenticator app.
- `r` role: admin, support engineer or auditor. It applies to the user's
  next request. The server keeps at least one admin.
- `a` access (support engineers): their grants. `n` adds one, on every
  agent, a group or a single agent, for any of remote desktop, shell,
  scripts and file transfer; `Del` removes the highlighted one. Without a
  grant an engineer sees no agents.
- `p` password: set a new one. The user is signed out everywhere (changing
  your own keeps the session you are in).
- `t` reset TOTP: a new secret, shown once like a new user's. The old
  authenticator entry stops working and the user is signed out.
- `Del` delete: not yourself, and not the last admin.

Others see the same view read-only; the server enforces this too.

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
2. Type a script, load a saved one with `Enter`, or press **New**
   (`Ctrl+N`) to create a library entry (name, optional timeout, body). It
   is saved and loaded into the editor.
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

For its side panel the viewer also gets `--api-url <server_url>`, your
session token in `RMM_API_TOKEN` (the environment again), and one
`--command "LABEL=COMMAND"` per button. It acts through the API as you, so
the server applies your access and audits what it does. It also gets
`--remember-font-size <data dir>/viewer.json`: a text size you pick in a
viewer is saved there, and the next viewer starts at it (see
`viewer_font_size`).

**Viewer buttons** (`v`): the command buttons shown in the viewer's side
panel. Each starts its command on the agent's desktop as the signed-in
user, like the Run dialog: `cmd`, `ncpa.cpl`, `mstsc /v:server01`,
`services.msc`... Add one with a label and a command (**Enter** in the
command field adds it too), remove the highlighted one with **Remove** or
`Delete`, reorder with ▲/▼ or `Shift+↑`/`Shift+↓`, **Defaults** restores
the standard set (Command prompt, Network connections, Remote desktop, Task
manager, Services, Event viewer), and **Save** keeps the list in
`state.json`. Viewers started afterwards show it; with an empty list the
panel has no buttons.

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
