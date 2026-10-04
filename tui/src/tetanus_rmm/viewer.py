"""Launching the native remote-desktop viewer.

The TUI does not render video. It asks the server for a short-lived,
single-use viewer token and starts the viewer binary (``crates/viewer``) as
a separate process. The token goes in the environment (``RMM_VIEWER_TOKEN``)
rather than on the command line, so other local users cannot read it from
the process list.

The viewer's side panel (the agent's status, command buttons, file
transfer) calls the HTTPS API as the signed-in technician, so it also gets
the API address (``--api-url``), this session's token (``RMM_API_TOKEN``,
in the environment for the same reason) and the command buttons
(``--command LABEL=COMMAND``).

The text size picked in a viewer (its Text menu) is remembered: the viewer
writes it to ``viewer.json`` in the TUI's data directory
(``--remember-font-size``), and the next viewer starts at that size.
Until one is picked, ``viewer_font_size`` from the config applies. The
viewer keeps the display mode picked (its Display menu) in the same file
and starts in it by itself.

The viewer's Vault button uses Bitwarden's ``bw`` client (see
``bitwarden``). Once the vault is unlocked in the TUI, each viewer gets the
session key (``RMM_BW_SESSION``, in the environment like the tokens) and
need not ask for the master password; ``bw_path`` from the config goes in
``RMM_BW``.
"""

from __future__ import annotations

import json
import logging
import os
import shutil
import socket
import subprocess
import sys
import time
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from pathlib import Path

from .api import ViewerSession
from .commands import QuickCommand
from .config import DEFAULT_QUIC_PORT, VIEWER_FONT_SIZES, Config

log = logging.getLogger(__name__)

#: Where viewers remember their text size, next to ``state.json``.
FONT_FILE = "viewer.json"


def remembered_font_size(path: Path) -> int | None:
    """The text size last picked in a viewer, if any (and sensible)."""
    try:
        size = json.loads(path.read_text(encoding="utf-8")).get("font_size")
    except FileNotFoundError:
        return None
    except (OSError, ValueError, AttributeError) as e:
        log.warning("ignoring unreadable %s: %s", path, e)
        return None
    if isinstance(size, int) and not isinstance(size, bool) and size in VIEWER_FONT_SIZES:
        return size
    return None


class ViewerError(Exception):
    pass


Resolver = Callable[[str, int], str]


def pick_address(infos: list[tuple]) -> str:
    """IPv4 if the name has one: the server listens on ``0.0.0.0`` by
    default, and ``localhost`` often resolves to ``::1`` first."""
    for family in (socket.AF_INET, socket.AF_INET6):
        for info in infos:
            if info[0] == family:
                return str(info[4][0])
    raise ViewerError("no usable address")


def resolve_ip(host: str, port: int) -> str:
    """The viewer takes a socket address, not a hostname: resolve it."""
    try:
        infos = socket.getaddrinfo(host, port, type=socket.SOCK_DGRAM)
    except socket.gaierror as e:
        raise ViewerError(f"cannot resolve {host}: {e}") from e
    return pick_address(infos)


def split_host_port(value: str, default_port: int) -> tuple[str, int]:
    """``host``, ``host:port``, ``[v6]:port`` or a bare IPv6 address."""
    if value.startswith("["):
        host, _, rest = value[1:].partition("]")
        port = rest.removeprefix(":")
    elif value.count(":") == 1:
        host, _, port = value.partition(":")
    else:
        host, port = value, ""
    try:
        return host, int(port) if port else default_port
    except ValueError:
        raise ViewerError(f"bad port in {value!r}") from None


@dataclass(frozen=True)
class ViewerCommand:
    argv: list[str]
    env: dict[str, str]


def build_command(
    config: Config,
    session: ViewerSession,
    resolve: Resolver = resolve_ip,
    *,
    api_token: str | None = None,
    commands: Iterable[QuickCommand] = (),
    font_file: Path | None = None,
    bw_session: str | None = None,
) -> ViewerCommand:
    """The viewer's command line and extra environment for ``session``.
    With ``api_token``, the side panel can use the API as this session.
    With ``font_file``, the viewer starts at the text size remembered there
    (else ``viewer_font_size``) and saves the one picked in it. With
    ``bw_session``, its Vault button opens the Bitwarden vault without
    asking for the master password."""
    if config.ca_path is None:
        raise ViewerError("the viewer needs a CA certificate: set ca_path in the config")
    host, port = split_host_port(config.quic_addr or config.api_host, DEFAULT_QUIC_PORT)
    ip = resolve(host, port)
    addr = f"[{ip}]:{port}" if ":" in ip else f"{ip}:{port}"
    # The certificate is checked against the name, not the address.
    server_name = host if config.quic_addr else config.api_host
    argv = [
        config.viewer_path or "viewer",
        "--server",
        addr,
        "--server-name",
        server_name,
        "--ca",
        str(config.ca_path),
    ]
    font_size = remembered_font_size(font_file) if font_file else None
    if font_size is None:
        font_size = config.viewer_font_size
    if font_size is not None:
        argv += ["--font-size", str(font_size)]
    if font_file is not None:
        argv += ["--remember-font-size", str(font_file)]
    env = {"RMM_VIEWER_TOKEN": session.token}
    # In the environment, not arguments: a viewer too old to know them
    # ignores them, and the session key must not be on a command line.
    if config.bw_path:
        env["RMM_BW"] = config.bw_path
    if bw_session:
        env["RMM_BW_SESSION"] = bw_session
    if api_token:
        argv += ["--api-url", config.server_url]
        env["RMM_API_TOKEN"] = api_token
        buttons = list(commands)
        for command in buttons:
            argv += ["--command", command.argument]
        if not buttons:
            argv.append("--no-default-commands")
    return ViewerCommand(argv=argv, env=env)


def launch(command: ViewerCommand, log_path: Path) -> subprocess.Popen[bytes]:
    """Start the viewer detached from the TUI's terminal; its output goes to
    ``log_path`` so it cannot scribble over the TUI."""
    exe = command.argv[0]
    if shutil.which(exe) is None and not Path(exe).is_file():
        raise ViewerError(f"viewer binary not found: {exe} (set viewer_path in the config)")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, **command.env}
    kwargs: dict = {}
    if sys.platform == "win32":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    with open(log_path, "ab") as log:
        try:
            return subprocess.Popen(
                command.argv,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=log,
                **kwargs,
            )
        except OSError as e:
            raise ViewerError(f"cannot start the viewer: {e}") from e


#: How long viewers get to exit after being asked before they are killed.
CLOSE_TIMEOUT = 3.0


def close_all(processes: Iterable[subprocess.Popen[bytes]], timeout: float = CLOSE_TIMEOUT) -> None:
    """Close every viewer still running: ask them all to exit, then kill any
    that are still there after ``timeout``."""
    running = [p for p in processes if p.poll() is None]
    for process in running:
        try:
            process.terminate()
        except OSError:  # exited in the meantime
            pass
    deadline = time.monotonic() + timeout
    for process in running:
        try:
            process.wait(max(0.0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
