"""Updating the TUI from the server it is signed in to.

The server publishes one TUI wheel (its install page offers it). A running
TUI asks which (``GET /api/tui/manifest``) when the agent list opens, and
on request (``U``); if that wheel is newer than the TUI itself, it offers to
update: the wheel is downloaded into the data directory and checked against
the SHA-256 in the manifest, the TUI closes, the wheel is installed the way
the TUI was (``uv tool``, ``pipx``, or ``pip`` in its environment), and the
TUI starts again.

The install runs after the TUI has closed, in the terminal, because it
replaces the files the TUI is running from. If it fails, or cannot be done
from here (Windows will not replace a running program; a source checkout
is updated with git), the command to run by hand is printed instead.
"""

from __future__ import annotations

import hashlib
import importlib.util
import logging
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import Button, Static

from . import __version__
from .api import ApiClient, ApiError, TuiBuild

log = logging.getLogger(__name__)

#: Where downloaded wheels are kept, in the data directory.
UPDATES_DIR = "updates"


def parse_version(version: str) -> tuple[int, ...] | None:
    """``1.2.3`` as ``(1, 2, 3)``. ``None`` for anything else (a
    pre-release, a development build), which is never offered."""
    if not re.fullmatch(r"\d+(\.\d+)*", version):
        return None
    return tuple(int(part) for part in version.split("."))


def is_newer(published: str, running: str = __version__) -> bool:
    """Whether ``published`` is a release later than ``running``."""
    new, old = parse_version(published), parse_version(running)
    if new is None or old is None:
        return False
    width = max(len(new), len(old))
    pad = lambda v: v + (0,) * (width - len(v))  # noqa: E731
    return pad(new) > pad(old)


def is_source_checkout(prefix: Path | None = None) -> bool:
    """Whether the TUI runs from a source tree (an editable install)
    rather than from files installed into its environment."""
    prefix = (prefix or Path(sys.prefix)).resolve()
    return not Path(__file__).resolve().is_relative_to(prefix)


def install_command(wheel: Path, prefix: Path | None = None) -> list[str] | None:
    """The command that installs ``wheel`` over this TUI, going by where
    it is installed. ``None`` if it cannot be worked out."""
    parts = (prefix or Path(sys.prefix)).resolve().parts
    if "uv" in parts and "tools" in parts and shutil.which("uv"):
        return ["uv", "tool", "install", "--force", str(wheel)]
    if "pipx" in parts and "venvs" in parts and shutil.which("pipx"):
        return ["pipx", "install", "--force", str(wheel)]
    if importlib.util.find_spec("pip") is not None:
        return [sys.executable, "-m", "pip", "install", "--upgrade", str(wheel)]
    return None


async def download(api: ApiClient, build: TuiBuild, directory: Path) -> Path:
    """Fetch ``build``'s wheel into ``directory`` and check it against the
    manifest. Raises :class:`ApiError`."""
    if not re.fullmatch(r"[0-9a-f]{64}", build.sha256) or Path(build.file).name != build.file:
        raise ApiError(0, "the server sent a malformed TUI manifest")
    try:
        directory.mkdir(parents=True, exist_ok=True)
        for old in directory.glob("*.whl"):
            old.unlink(missing_ok=True)
        dest = directory / build.file
        await api.download(f"/install/{build.file}", dest)
        if hashlib.sha256(dest.read_bytes()).hexdigest() != build.sha256:
            dest.unlink(missing_ok=True)
            raise ApiError(0, "the downloaded TUI does not match the server's manifest")
    except OSError as e:
        raise ApiError(0, f"cannot save the update in {directory}: {e}") from e
    return dest


@dataclass(frozen=True)
class PendingUpdate:
    """What the app exits with when the user accepted an update: the wheel,
    downloaded and verified, to install once the TUI has closed."""

    version: str
    wheel: Path


def finish(update: PendingUpdate, argv: list[str] | None = None) -> None:
    """Install ``update`` and start the TUI again. Called after the app has
    closed; talks to the terminal. Does not return if the restart works."""
    command = None if is_source_checkout() else install_command(update.wheel)
    by_hand = f"uv tool install --force {update.wheel}"
    if command is None or sys.platform == "win32":
        # Windows will not replace the program that is running.
        shown = subprocess.list2cmdline(command) if command else by_hand
        print(f"TetanusRMM {update.version} is downloaded. To install it, run:\n\n  {shown}\n")
        return
    print(f"Updating TetanusRMM to {update.version}...", flush=True)
    try:
        code = subprocess.run(command, check=False).returncode
    except OSError as e:
        print(f"Could not run {command[0]}: {e}")
        code = 1
    if code != 0:
        print(
            f"\nThe update did not install. This version still works; to try again, run:\n\n"
            f"  {subprocess.list2cmdline(command)}\n"
        )
        sys.exit(1)
    argv = sys.argv if argv is None else argv
    launcher = shutil.which(argv[0]) if argv else None
    try:
        if launcher:
            os.execv(launcher, [launcher, *argv[1:]])
        os.execv(sys.executable, [sys.executable, "-m", "tetanus_rmm", *argv[1:]])
    except OSError as e:
        print(f"Updated to {update.version}. Start tetanus-rmm again ({e}).")


class UpdateScreen(ModalScreen[Path | None]):
    """Offers the newer TUI the server publishes. Dismissed with the
    downloaded, verified wheel if the user takes it, else ``None``."""

    BINDINGS = [Binding("escape", "cancel", "Later")]

    def __init__(self, api: ApiClient, build: TuiBuild, directory: Path) -> None:
        super().__init__()
        self._api = api
        self.build = build
        self._directory = directory

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="update-box"):
            yield Static(f"[b]TetanusRMM {self.build.version} is available[/b]")
            yield Static(
                f"You have {__version__}. The server publishes {self.build.version}: "
                "an older TUI can miss what the server and viewer now do.",
                classes="dialog-text",
            )
            yield Static(
                "Updating downloads it from the server, closes the TUI and any viewers it "
                "opened, installs it and starts the TUI again.",
                classes="dialog-help",
            )
            yield Static("", id="update-status")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Later", id="update-later")
                yield Button("Update and restart", variant="primary", id="update-now")

    def on_mount(self) -> None:
        self.query_one("#update-now").focus()

    @on(Button.Pressed, "#update-now")
    def update(self) -> None:
        self.query_one("#update-now", Button).disabled = True
        self.query_one("#update-status", Static).update("Downloading…")
        self.fetch()

    @work(exclusive=True, group="update")
    async def fetch(self) -> None:
        try:
            wheel = await download(self._api, self.build, self._directory)
        except ApiError as e:
            self.query_one("#update-status", Static).update(f"[red]{e.message}[/red]")
            self.query_one("#update-now", Button).disabled = False
            return
        self.dismiss(wheel)

    @on(Button.Pressed, "#update-later")
    def action_cancel(self) -> None:
        self.dismiss(None)
