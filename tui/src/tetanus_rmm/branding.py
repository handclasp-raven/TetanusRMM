"""Company branding: the name, logo and accent colour that take
TetanusRMM's place on what the people being supported see (the agent's
windows and tray icon, quick assist, the installer and the server's
install pages). One per server; admins set it.

Layout, type and the safety wording stay as they are. Without a logo the
TetanusRMM mark is used, on the company's colour; without a colour,
TetanusRMM's rust. The server refuses a colour too light for white text
and a logo that is not a PNG of 16 to 512 pixels a side (128 KiB at most).
"""

from __future__ import annotations

import re
from pathlib import Path

from rich.text import Text
from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import Button, Checkbox, Input, Static

from .api import ApiClient, ApiError, Branding, Unauthorized

#: TetanusRMM's own accent, shown when no colour is set.
RUST = "#B5441C"
#: Largest logo the server takes.
MAX_LOGO_BYTES = 128 * 1024

_COLOUR = re.compile(r"#?[0-9A-Fa-f]{6}")


def normalise_colour(text: str) -> str | None:
    """``b5441c`` or ``#B5441C`` as ``#B5441C``; ``None`` if it is not a colour."""
    text = text.strip()
    if not _COLOUR.fullmatch(text):
        return None
    return "#" + text.removeprefix("#").upper()


def read_logo(path: str) -> bytes:
    """The PNG at ``path``. Raises :class:`ValueError` saying what is wrong."""
    file = Path(path).expanduser()
    try:
        data = file.read_bytes()
    except OSError as e:
        raise ValueError(f"cannot read {file}: {e.strerror or e}") from e
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError(f"{file.name} is not a PNG image")
    if len(data) > MAX_LOGO_BYTES:
        raise ValueError(
            f"{file.name} is {len(data) // 1024} KiB; a logo may be {MAX_LOGO_BYTES // 1024} KiB"
        )
    return data


class BrandingScreen(ModalScreen[None]):
    """Set or reset the server's company branding."""

    BINDINGS = [Binding("escape", "cancel", "Close")]

    def __init__(self, api: ApiClient) -> None:
        super().__init__()
        self._api = api
        self.current: Branding | None = None

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="branding-box"):
            yield Static("[b]Company branding[/b]")
            yield Static(
                "What the people you support see: on the agent's windows and tray icon, "
                "quick assist, the installer and the install pages.",
                classes="dialog-help",
            )
            yield Input(placeholder="Company name, e.g. Contoso IT", max_length=48, id="br-name")
            with Horizontal(id="br-colour-row"):
                yield Input(placeholder=f"Accent colour, e.g. {RUST}", id="br-accent")
                yield Static("", id="br-swatch")
            yield Input(placeholder="Logo: path to a PNG file", id="br-logo")
            yield Checkbox("No logo: use the TetanusRMM mark", id="br-no-logo")
            yield Static("", id="br-current", classes="dialog-help")
            yield Static("", id="br-status")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Reset to TetanusRMM", id="br-reset")
                yield Button("Close", id="br-close")
                yield Button("Save", variant="primary", id="br-save")

    def on_mount(self) -> None:
        self.query_one("#br-name").focus()
        self.load()

    def _status(self, text: str, error: bool = False) -> None:
        self.query_one("#br-status", Static).update(f"[red]{text}[/red]" if error else text)

    def _show(self, branding: Branding | None) -> None:
        self.current = branding
        self.query_one("#br-name", Input).value = branding.name if branding else ""
        self.query_one("#br-accent", Input).value = (branding.accent or "") if branding else ""
        self.query_one("#br-logo", Input).value = ""
        self.query_one("#br-no-logo", Checkbox).value = False
        if branding is None:
            now = "Not set: everything shows as TetanusRMM."
        elif branding.logo_png is None:
            now = f"Set: {branding.name}, with the TetanusRMM mark."
        else:
            size = len(branding.logo_png) // 1024 + 1
            now = (
                f"Set: {branding.name}, with a logo ({size} KiB). "
                "Leave the logo's path empty to keep it."
            )
        self.query_one("#br-current", Static).update(now)
        self.query_one("#br-reset", Button).disabled = branding is None
        self._swatch()

    @work(exclusive=True, group="branding")
    async def load(self) -> None:
        try:
            branding = await self._api.branding()
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self._status(e.message, error=True)
            return
        self._show(branding)

    @on(Input.Changed, "#br-accent")
    def _swatch(self) -> None:
        typed = self.query_one("#br-accent", Input).value
        colour = normalise_colour(typed) if typed.strip() else RUST
        swatch = self.query_one("#br-swatch", Static)
        if colour is None:
            swatch.update(Text(" ? ", style="dim"))
        else:
            swatch.update(Text("  Aa  ", style=f"bold white on {colour}"))

    @on(Input.Submitted)
    @on(Button.Pressed, "#br-save")
    def save(self) -> None:
        name = self.query_one("#br-name", Input).value.strip()
        if not name:
            self._status("Give the company's name.", error=True)
            return
        typed = self.query_one("#br-accent", Input).value.strip()
        accent = normalise_colour(typed) if typed else None
        if typed and accent is None:
            self._status("The accent colour must look like #B5441C.", error=True)
            return
        logo = self.current.logo_png if self.current else None
        path = self.query_one("#br-logo", Input).value.strip()
        if self.query_one("#br-no-logo", Checkbox).value:
            logo = None
        elif path:
            try:
                logo = read_logo(path)
            except ValueError as e:
                self._status(str(e), error=True)
                return
        self._status("Saving…")
        self.send(Branding(name=name, accent=accent, logo_png=logo))

    @work(exclusive=True, group="branding")
    async def send(self, branding: Branding | None) -> None:
        try:
            saved = (
                await self._api.set_branding(branding)
                if branding is not None
                else await self._api.reset_branding()
            )
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self._status(e.message[:1].upper() + e.message[1:], error=True)
            return
        self._show(saved)
        self._status(
            "Saved. Connected agents show it at once; new quick assist downloads and "
            "installers carry it."
            if saved is not None
            else "Reset. Everything shows as TetanusRMM again."
        )

    @on(Button.Pressed, "#br-reset")
    def reset(self) -> None:
        self._status("Resetting…")
        self.send(None)

    @on(Button.Pressed, "#br-close")
    def action_cancel(self) -> None:
        self.dismiss(None)
