"""Quick assist: help someone on a machine with no agent installed, once.

The technician gets a six-digit code and a page address. The user opens the
page, downloads and runs the quick assist program, and types the code. This
screen waits for that, then starts the viewer; the user is asked, with the
technician's name, before anything is shown.

The session is remote desktop and file transfer only, and lasts until the
user closes the program. While it does, the machine is in the agent table,
so a viewer closed by accident can be reopened from there.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import TYPE_CHECKING

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Button, Footer, Header, Input, Label, Static

from .api import (
    ASSIST_CONNECTED,
    ASSIST_CONNECTING,
    ASSIST_ENDED,
    ASSIST_EXPIRED,
    ApiError,
    AssistCode,
    AssistStatus,
    Forbidden,
    Unauthorized,
)
from .viewer import ViewerError

if TYPE_CHECKING:
    from .app import RmmApp

#: Seconds between checks on whether the code has been typed.
POLL_INTERVAL = 1.0


def remaining(code: AssistCode, now: datetime) -> str:
    """How long the code is still good for: ``9:41``, or ``0:00``."""
    seconds = max(0, int((code.expires_at - now).total_seconds()))
    return f"{seconds // 60}:{seconds % 60:02d}"


class QuickAssistScreen(Screen):
    app: RmmApp

    BINDINGS = [
        Binding("escape", "app.pop_screen", "Back"),
        Binding("ctrl+g", "new_code", "New code", priority=True),
    ]

    def __init__(self) -> None:
        super().__init__()
        self.code: AssistCode | None = None
        #: The session's status as last fetched.
        self.status: AssistStatus | None = None
        #: The viewer was started for the current code (once is enough:
        #: reopening is done from the agent table).
        self.started = False

    def compose(self) -> ComposeResult:
        yield Header()
        with Vertical(id="quick-assist"):
            yield Static(
                "Help someone whose computer has no agent installed, for one session. "
                "They are asked, with your name, before you see anything; you get "
                "remote desktop and file transfer, until they close the program.",
                id="qa-intro",
            )
            yield Label("1. Send them to this page. They download and open quick assist.")
            with Horizontal(classes="na-url-row"):
                yield Input(id="qa-url", classes="na-url")
                yield Button("Copy", id="qa-copy")
            yield Label("2. Read them this code. It works once.")
            yield Static("", id="qa-code")
            yield Static("", id="qa-status")
            with Horizontal(id="qa-actions"):
                yield Button("New code  ^G", id="qa-new")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "Quick assist"
        self.set_interval(POLL_INTERVAL, self.tick)
        self.action_new_code()

    def set_status(self, text: str) -> None:
        self.query_one("#qa-status", Static).update(text)

    @on(Button.Pressed, "#qa-new")
    def action_new_code(self) -> None:
        self.query_one("#qa-new", Button).disabled = True
        self.set_status("Getting a code…")
        self.new_code()

    @work(exclusive=True, group="code")
    async def new_code(self) -> None:
        try:
            code = await self.app.session.api.create_assist_code()
        except Unauthorized:
            self.app.session_expired()
            return
        except Forbidden:
            self.set_status("[red]You may not start quick assist sessions.[/red]")
            return
        except ApiError as e:
            self.set_status(f"[red]{e.message}[/red]")
            return
        finally:
            self.query_one("#qa-new", Button).disabled = False
        self.code, self.status, self.started = code, None, False
        self.query_one("#qa-url", Input).value = code.url
        self.query_one("#qa-code", Static).update(code.spaced)
        self.show_progress()

    @on(Button.Pressed, "#qa-copy")
    def copy_url(self) -> None:
        self.app.copy_to_clipboard(self.query_one("#qa-url", Input).value)
        self.app.notify("Page address copied to the clipboard.")

    def tick(self) -> None:
        if self.code is None or self.started:
            return
        if self.status is not None and self.status.status in (ASSIST_EXPIRED, ASSIST_ENDED):
            return
        self.show_progress()
        self.check()

    def show_progress(self) -> None:
        """The status line for where the session is."""
        if self.code is None:
            return
        state = self.status.status if self.status else None
        if state == ASSIST_EXPIRED:
            text = "[red]The code has expired.[/red] Press New code for another."
        elif state == ASSIST_ENDED:
            text = "The session has ended."
        elif state == ASSIST_CONNECTING:
            text = "Code accepted. Waiting for their computer to connect…"
        elif state == ASSIST_CONNECTED:
            text = "Connected. Starting the viewer…"
        else:
            left = remaining(self.code, datetime.now(UTC))
            text = f"Waiting for them to type the code. It expires in {left}."
        self.set_status(text)

    @work(exclusive=True, group="status")
    async def check(self) -> None:
        code = self.code
        if code is None:
            return
        try:
            status = await self.app.session.api.assist_status(code.id)
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            if e.status == 404:
                # Forgotten by the server: as good as expired.
                status = AssistStatus(code.id, ASSIST_EXPIRED, None, None)
            else:
                self.set_status(f"[red]{e.message}[/red]")
                return
        if code is not self.code or self.started:
            return  # a new code was made meanwhile
        self.status = status
        self.show_progress()
        if status.status == ASSIST_CONNECTED and status.agent_id:
            self.started = True
            self.query_one("#qa-code", Static).update("")
            self.start_viewer(status)

    @work(exclusive=True, group="viewer")
    async def start_viewer(self, status: AssistStatus) -> None:
        assert status.agent_id is not None
        try:
            # No command buttons: a quick assist session allows none.
            process = await self.app.start_viewer(status.agent_id, [])
        except Unauthorized:
            self.app.session_expired()
            return
        except (ApiError, ViewerError) as e:
            self.set_status(
                f"[red]{e}[/red]\n{status.label} is in the agent list: press d there to try again."
            )
            return
        if process is None:
            self.set_status(
                f"[red]{status.label} disconnected.[/red] Press New code to start over."
            )
            return
        self.set_status(
            f"[green]Viewer started for {status.label}.[/green] They are being asked to "
            "allow you. While they keep quick assist open, their computer is in the "
            "agent list: press d there to open the viewer again."
        )
        self.app.notify(f"Viewer started for {status.label} (pid {process.pid}).")
