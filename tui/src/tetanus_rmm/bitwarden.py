"""Unlocking the technician's Bitwarden vault for the viewers.

A viewer's Vault tab searches the vault through Bitwarden's command
line client, ``bw``, and has a username, password or one-time code typed on
the remote machine. So that the master password is not asked for in every
viewer, the TUI can unlock once: ``bw unlock`` returns a *session key*,
which the TUI keeps in memory and gives each viewer it starts, in the
environment (``RMM_BW_SESSION``), like the tokens.

The master password is never stored: it goes to that one ``bw unlock`` in
its environment (not on its command line, which other local users can
read) and is dropped. The session key is never written anywhere either,
and stops working when the vault is locked: from here, when the TUI quits
or signs out, or with ``bw lock`` anywhere.

``bw`` must be installed and signed in (``bw login``, once).
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
from collections.abc import Callable

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Static

#: The child's environment variable the master password is passed in.
PASSWORD_ENV = "RMM_BW_PASSWORD"

#: Seconds ``bw`` gets; it talks to nothing but its own files here.
TIMEOUT = 60.0


class BitwardenError(Exception):
    pass


class WrongPassword(BitwardenError):
    pass


def _run(bw: str, args: list[str], env: dict[str, str] | None = None) -> str:
    """Run ``bw args...`` and return what it printed. ``env`` is added to
    the child's environment only."""
    child_env = {k: v for k, v in os.environ.items() if k != "BW_SESSION"}
    # Never wait at a prompt nobody can see.
    child_env |= {"BW_NOINTERACTION": "true", **(env or {})}
    kwargs: dict = {}
    if sys.platform == "win32":
        kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW
    try:
        done = subprocess.run(
            [bw, *args],
            env=child_env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
            check=False,
            **kwargs,
        )
    except FileNotFoundError:
        raise BitwardenError(
            f"Bitwarden's command line client was not found ({bw}): install bw, "
            "or set bw_path in the config"
        ) from None
    except (OSError, subprocess.TimeoutExpired) as e:
        raise BitwardenError(f"cannot run {bw}: {e}") from e
    if done.returncode == 0:
        return done.stdout
    message = done.stderr.strip()
    lower = message.lower()
    if "not logged in" in lower:
        raise BitwardenError("not signed in to Bitwarden: run `bw login` once, then try again")
    if "invalid master password" in lower:
        raise WrongPassword("Wrong master password.")
    # The last line: earlier ones are progress and warnings.
    raise BitwardenError(message.splitlines()[-1][:200] if message else "bw failed")


def status(bw: str, session: str | None = None) -> str:
    """``unauthenticated`` (``bw login`` not done), ``locked``, or
    ``unlocked`` (``session`` opens the vault)."""
    out = _run(bw, ["status"], {"BW_SESSION": session} if session else None)
    try:
        return str(json.loads(out)["status"])
    except (ValueError, KeyError, TypeError) as e:
        raise BitwardenError(f"unexpected `bw status` output: {e}") from e


def unlock(bw: str, master_password: str) -> str:
    """The session key for ``master_password``. Raises
    :class:`WrongPassword`, or :class:`BitwardenError` for anything else."""
    session = _run(
        bw,
        ["unlock", "--raw", "--passwordenv", PASSWORD_ENV],
        {PASSWORD_ENV: master_password},
    ).strip()
    if not session:
        raise BitwardenError("`bw unlock` gave no session key")
    return session


def lock(bw: str) -> None:
    """Lock the vault: every session key stops working, including those
    viewers already hold."""
    _run(bw, ["lock"])


Unlock = Callable[[str], str]


class UnlockScreen(ModalScreen[str | None]):
    """Asks for the master password and unlocks. Dismissed with the session
    key, or ``None`` if cancelled. The password is not kept."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, unlock: Unlock) -> None:  # noqa: A002
        super().__init__()
        self._unlock = unlock

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="bw-box"):
            yield Static("[b]Unlock your Bitwarden vault[/b]")
            yield Input(placeholder="Master password", password=True, id="bw-password")
            yield Static(
                "Viewers started from now on can search your vault and type usernames, "
                "passwords and one-time codes on the remote machine. The master password "
                "is not kept; the vault is locked again when you quit or sign out.",
                classes="dialog-help",
            )
            yield Static("", id="bw-status")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="bw-cancel")
                yield Button("Unlock", variant="primary", id="bw-unlock")

    def on_mount(self) -> None:
        self.query_one("#bw-password").focus()

    @on(Input.Submitted)
    @on(Button.Pressed, "#bw-unlock")
    def submit(self) -> None:
        field = self.query_one("#bw-password", Input)
        password, field.value = field.value, ""
        if password:
            self.query_one("#bw-status", Static).update("Unlocking…")
            self.try_unlock(password)

    @work(exclusive=True, group="bitwarden")
    async def try_unlock(self, password: str) -> None:
        try:
            # bw takes a second or so: not on the UI's thread.
            session = await asyncio.to_thread(self._unlock, password)
        except BitwardenError as e:
            self.query_one("#bw-status", Static).update(f"[red]{e}[/red]")
            self.query_one("#bw-password").focus()
            return
        self.dismiss(session)

    @on(Button.Pressed, "#bw-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)
