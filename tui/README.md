# tetanus-rmm: support TUI

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
- **Quick assist:** help someone whose computer has no agent, for one
  session: a six-digit code they type into a program they download from
  the server. The viewer opens when they have.
- **Groups:** view groups and their members. Admins can also create, rename,
  delete and fill them. The group list beside the agent table filters it,
  and the search box above it filters by hostname, IP address or group
  name.
- **Users** (admins only): add and remove users, change their role, choose
  which agents a support engineer may work on, and set their password or
  reset their TOTP secret.

## Install

For staff: open your server's install page, `https://<server>:8443/install`,
download the TUI there and follow its steps (an administrator fills the page
with `scripts/build-clients.sh`). Then run `tetanus-rmm` and sign in. No config
file, certificate or viewer has to be set up by hand: see
[First sign-in](#first-sign-in). The install itself is one command, with
[uv](https://docs.astral.sh/uv/) (which fetches Python itself) or pipx
(Python 3.11 or newer):

```sh
uv tool install tetanus_rmm-0.1.3-py3-none-any.whl     # or: pipx install tetanus_rmm-…whl
tetanus-rmm
```

For development, from the repository, with uv:

```sh
cd tui
uv venv && uv pip install -e .          # add '.[dev]' for the tests
.venv/bin/tetanus-rmm --help
```

With pip:

```sh
cd tui
python -m venv .venv
.venv/bin/pip install -e .              # Windows: .venv\Scripts\pip
.venv/bin/tetanus-rmm
```

`python -m tetanus_rmm` works too. Dependencies: `textual`, `httpx`, `keyring`,
`websockets` (the shell), `pyte` (terminal emulation for the shell pane).

The keyring uses Windows Credential Manager, the macOS Keychain, or Secret
Service on Linux (GNOME Keyring or KWallet). If no keyring is available, the
TUI still works, but you have to sign in on each launch; it tells you when
this happens.

## First sign-in

Type the server's address in **Server URL** (`rmm.example.com:8443`;
`https://` is assumed), then your username, password and TOTP code. The
server is remembered for next time.

**Trusting the server.** A server usually has its own certificate authority
(from `server gen-certs`), which your computer has never seen. The first
time you sign in to such a server, before anything is sent to it, the TUI
shows the SHA-256 fingerprint of that CA and asks whether to trust it.
Compare it with the fingerprint from your administrator (the server prints
it when the certificates are made and logs it at every start: `CA
certificate fingerprint`; or `openssl x509 -in ca.crt -noout -fingerprint
-sha256`). Once accepted, the CA is kept in the data directory
(`servers/<host>_<port>/ca.crt`) and that server is only ever verified
against it: you are not asked again.

If the server later presents a certificate that the accepted CA did not
sign, the TUI refuses to sign in rather than ask again. If the server's
certificates really were replaced, forget the old CA and accept the new one:

```sh
tetanus-rmm --forget-ca --server-url https://rmm.example.com:8443
```

A server whose API has a publicly trusted certificate (`RMM_API_TLS_CERT`)
needs no prompt at all. Setting `ca_path` turns the prompt off: the server
is then verified against that file only.

**The viewer.** The first time you start a remote desktop session the TUI
downloads the server's viewer build for your platform (Linux or Windows,
x86-64) into the data directory (`viewer/`), checks it against the SHA-256
the server publishes, and starts it. It is fetched again whenever the server
publishes a new build, so it always matches the server. The viewer is given
the CA accepted above. If the server publishes no viewer for your platform,
`viewer` on `PATH` is used; `viewer_path` names another.

## Configuration

Every setting is optional, and so is the file. Settings come from a TOML
file, and command-line flags override them. The file lives at:

| OS | Default path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/tetanus-rmm/config.toml` (usually `~/.config/tetanus-rmm/config.toml`) |
| macOS | `~/Library/Application Support/tetanus-rmm/config.toml` |
| Windows | `%APPDATA%\tetanus-rmm\config.toml` |

Pass `--config PATH` or set `TETANUS_RMM_CONFIG` to use a different file.

Up to 0.1.1 the TUI was called `rmm-tui`. The first run moves that name's
config and data directories and saved sign-in over to `tetanus-rmm`; remove
the old tool with `uv tool uninstall rmm-tui`.

```toml
server_url = "https://rmm.example.com:8443"   # the HTTPS API
ca_path = "/etc/rmm/ca.crt"                   # optional: CA to trust for the server
viewer_path = "/opt/rmm/viewer"               # optional: native viewer binary
quic_addr = "rmm.example.com:4433"            # optional, see below
poll_interval = 5                             # seconds between agent refreshes
scripts_path = "~/rmm-scripts.json"           # optional; default is in the data dir
viewer_font_size = 9                          # optional; the viewer's text size
```

| Setting | Flag | Default | |
|---|---|---|---|
| `server_url` | `--server-url` | `https://localhost:8443` | Must be `https://`. |
| `ca_path` | `--ca` | *(the CA accepted at first sign-in, else the system trust store)* | PEM CA certificate to verify the server against, instead of asking. Also given to the viewer. |
| `viewer_path` | `--viewer` | *(the server's build, else `viewer` on `PATH`)* | A viewer to use instead of the downloaded one, e.g. `target/release/viewer`. |
| `quic_addr` | `--quic-addr` | API host, port `4433` | The server's QUIC listener, for the viewer. The name is resolved to an address (IPv4 preferred), and the viewer checks the certificate against the name. |
| `poll_interval` | | `5` | At least 1. |
| `scripts_path` | | data dir `/scripts.json` | The saved-script library. |
| `viewer_font_size` | | *(viewer default, 10)* | Text size of the viewer's toolbar and side panel, in pixels (6–32), until you pick one in a viewer (**Text** menu, or `Ctrl+Alt+Shift+-`/`=`). From then on viewers start at the size you last picked, remembered in `viewer.json` in the data directory; delete that file to go back to this setting. |

Relative paths in the file are relative to the file. Unknown keys are an
error, so typos get caught.

TLS is always verified: against `ca_path` if set, otherwise against the CA
accepted for the server at [first sign-in](#first-sign-in), otherwise
against the system trust store. There is no option to turn verification off.

The TUI also remembers the server you last signed in to, your agent-table
columns, your colour theme and the themes you made, in `state.json` in the
data directory.

Logs go to the data directory (`~/.local/share/tetanus-rmm/`,
`~/Library/Application Support/tetanus-rmm/` or `%LOCALAPPDATA%\tetanus-rmm\`), never
to the terminal: `tetanus-rmm.log` for the TUI and `viewer.log` for viewers it
launches.

## Running

```sh
# Against a dev server started from the repo root (see the main README):
tetanus-rmm --server-url https://localhost:8443 --ca ../dev-certs/ca.crt \
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
keyring). `tetanus-rmm --logout` only forgets the saved token locally; the
server-side session then expires on its own.

### Keys

**Agent table**

| Key | Action |
|---|---|
| `m` or `F10` | Open the menu bar (`←`/`→`: other menus, `Enter`: choose, `Esc`: close) |
| `d` or `Enter` | Remote desktop: launch the viewer for the selected agent |
| `s` | Shell console on the selected agent |
| `r` | Script runner (the selected agent is pre-selected) |
| `n` | New agent: download link / MSI (admins and support engineers) |
| `h` | Quick assist: a one-time session by six-digit code (admins and support engineers) |
| `/` | Search by hostname, IP address or group name (`Enter`: back to the table, `Esc`: clear) |
| `f` | Jump to the group list |
| `k` | Classify the selected agent as server, desktop or other (admins) |
| `g` | Groups |
| `u` | Users: accounts, roles, access and passwords (admins; hidden otherwise) |
| `a` | Audit log (admins and auditors) |
| `c` | Choose columns |
| `p` | Show or hide the stats panel (kept between runs) |
| `t` | Choose the colour theme (kept between runs; `tetanus` is the default) |
| `e` | Theme editor: make, change and delete themes of your own |
| `v` | Viewer buttons: the remote viewer's command buttons |
| `F5` | Refresh now (the table also refreshes every `poll_interval`) |
| `l` | Sign out |
| `q` | Quit |

The **menu bar** over the table groups the same actions into Agent, View,
Manage and Session menus, each item with its key; click a menu or press `m`.

The **stats panel** under the table shows the selected agent's CPU, memory
and system disk, each with a graph of the last hour, and its uptime,
sessions, signed-in users and fixed disks. The figures come with the agent
list, so they appear at once; the graphs are fetched when the selection has
rested on an agent for a moment (a bar at the panel's top right shows while
they load), and are kept, so moving through the list or back to an agent
asks the server for nothing. A gap in a graph is time the agent was not
connected. `p` hides the panel, and hidden it fetches nothing. An older server
keeps no history: the panel then shows the figures alone.

The **theme editor** (`e`) lists every theme on the left. Pick one to start
from, change its colours (`#RRGGBB`) and the whole screen takes them on as
you type. **Save** (`Ctrl+S`) keeps it under the name in the form and makes
it your theme; a built-in theme is never changed, so starting from one makes
a copy under a new name. Your own themes are marked "yours" and can be
edited again or deleted. `Esc` leaves, putting back the theme you last saved
there or else the one you had.

The footer shows only the essentials (menu, remote desktop, shell, search,
quit); the other keys still work and are listed in the menus. Remote desktop
and shell are only offered where you may use them on the **selected agent**
(the menus grey out what you may not do). A
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
agents with the text in their hostname, IP address (their own or their
public one) or the name of a group they are in, ignoring case.

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

**Quick assist** (`h`): for a computer with no agent installed.
1. Tell the user the page address shown (**Copy** puts it on the clipboard).
   They download quick assist there and open it; it makes them read a
   warning about scams for five seconds first.
2. Read them the six-digit code. It works once and expires after ten
   minutes; `Ctrl+G` makes a new one.
3. When they have typed it, the viewer starts by itself and they are asked,
   with your username, whether to allow you.

You get remote desktop and file transfer, no shell, scripts or command
buttons. The session lasts until they close quick assist. Until then their
computer is in the agent table, so if you close the viewer, press `d` on it
to open another (they are asked again).

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
