"""New agent: make a single-use download link, optionally joining groups,
and get it as a Windows MSI (installs and enrolls unattended) or a plain
binary. The MSI can also be saved locally from here."""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import Screen
from textual.widgets import Button, Footer, Header, Input, Label, Select, SelectionList, Static

from .api import ApiError, EnrollmentLink, Unauthorized
from .config import DEFAULT_QUIC_PORT, Config

if TYPE_CHECKING:
    from .app import RmmApp

PLATFORMS = [
    ("Windows x64 (MSI installer or .exe)", "windows-x86_64"),
    ("Linux x64 (binary)", "linux-x86_64"),
]
LIFETIMES = [("1 hour", 3600), ("24 hours", 86400), ("7 days", 7 * 86400)]
DEFAULT_LIFETIME = 86400


def default_server(config: Config) -> str:
    """Where new agents connect by default: the configured QUIC address,
    else the server this TUI talks to, on the QUIC port."""
    if config.quic_addr:
        return config.quic_addr
    host = config.api_host
    if ":" in host:  # IPv6
        host = f"[{host}]"
    return f"{host}:{DEFAULT_QUIC_PORT}"


def unique_path(path: Path) -> Path:
    """``path``, or ``name-2.ext``, ``name-3.ext``… if it exists."""
    candidate, n = path, 1
    while candidate.exists():
        n += 1
        candidate = path.with_name(f"{path.stem}-{n}{path.suffix}")
    return candidate


def default_msi_path() -> Path:
    downloads = Path.home() / "Downloads"
    folder = downloads if downloads.is_dir() else Path.home()
    return unique_path(folder / "rmm-agent.msi")


def install_command(link: EnrollmentLink, platform: str) -> str:
    """The manual alternative to the MSI, for the plain binary."""
    args = (
        f"--server {link.server} --server-name {link.server_name} "
        f"--server-ca ca.crt --token {link.token}"
    )
    if platform.startswith("windows"):
        return f"rmm-agent.exe service install {args}"
    return f"agent enroll {args} && agent run"


class NewAgentScreen(Screen):
    app: RmmApp

    BINDINGS = [
        Binding("escape", "app.pop_screen", "Back"),
        Binding("ctrl+g", "create", "Create link", priority=True),
    ]

    def __init__(self) -> None:
        super().__init__()
        self.link: EnrollmentLink | None = None
        self.link_platform = ""

    @property
    def is_admin(self) -> bool:
        user = self.app.session.user
        return bool(user and user.is_admin)

    def compose(self) -> ComposeResult:
        config = self.app.config
        yield Header()
        with VerticalScroll(id="new-agent"):
            yield Static(
                "Make a single-use link for installing a new agent. It expires "
                "after the time you choose, or once an agent has enrolled with it.",
                id="na-intro",
            )
            yield Label("Platform")
            yield Select(PLATFORMS, value="windows-x86_64", allow_blank=False, id="na-platform")
            yield Label("Server address the agent connects to (host:port)")
            yield Input(default_server(config), id="na-server")
            yield Label("TLS name (a name on the server's certificate)")
            yield Input(config.api_host, id="na-server-name")
            yield Label("Link valid for")
            yield Select(LIFETIMES, value=DEFAULT_LIFETIME, allow_blank=False, id="na-ttl")
            yield Label("Groups the agent joins")
            if self.is_admin:
                yield SelectionList[int](id="na-groups")
            else:
                yield Static("Only admins can put new agents into groups.", id="na-groups-note")
            with Horizontal(id="na-actions"):
                yield Button("Create link  ^G", variant="primary", id="na-create")
                yield Static("", id="na-status")
            with Vertical(id="na-result"):
                yield Static("", id="na-summary")
                with Vertical(id="na-msi"):
                    yield Label("MSI installer: double-click it, or msiexec /i rmm-agent.msi /qn")
                    with Horizontal(classes="na-url-row"):
                        yield Input(id="na-msi-url", classes="na-url")
                        yield Button("Copy", id="na-copy-msi")
                    with Horizontal(classes="na-url-row"):
                        yield Input(id="na-save-path", classes="na-url")
                        yield Button("Save MSI", id="na-save", variant="success")
                yield Label("Agent binary (needs ca.crt and this command):")
                with Horizontal(classes="na-url-row"):
                    yield Input(id="na-exe-url", classes="na-url")
                    yield Button("Copy", id="na-copy-exe")
                with Horizontal(classes="na-url-row"):
                    yield Input(id="na-command", classes="na-url")
                    yield Button("Copy", id="na-copy-command")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "New agent"
        self.query_one("#na-result").display = False
        if self.is_admin:
            self.load_groups()

    @work(exclusive=True, group="groups")
    async def load_groups(self) -> None:
        try:
            groups = await self.app.session.api.list_groups()
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.set_status(f"[red]Cannot load groups: {e.message}[/red]")
            return
        selection = self.query_one("#na-groups", SelectionList)
        selection.clear_options()
        selection.add_options([(g.name, g.id) for g in sorted(groups, key=lambda g: g.name)])

    def set_status(self, text: str) -> None:
        self.query_one("#na-status", Static).update(text)

    @on(Button.Pressed, "#na-create")
    def action_create(self) -> None:
        server = self.query_one("#na-server", Input).value.strip()
        server_name = self.query_one("#na-server-name", Input).value.strip()
        if not server or not server_name:
            self.set_status("[red]Enter the server address and TLS name.[/red]")
            return
        platform = self.query_one("#na-platform", Select).value
        ttl = self.query_one("#na-ttl", Select).value
        group_ids = (
            list(self.query_one("#na-groups", SelectionList).selected) if self.is_admin else []
        )
        self.set_status("Creating…")
        self.query_one("#na-create", Button).disabled = True
        self.create(str(platform), int(ttl), group_ids, server, server_name)  # type: ignore[arg-type]

    @work(exclusive=True, group="create")
    async def create(
        self, platform: str, ttl: int, group_ids: list[int], server: str, server_name: str
    ) -> None:
        try:
            link = await self.app.session.api.create_enrollment_link(
                platform=platform,
                ttl_secs=ttl,
                group_ids=group_ids,
                server=server,
                server_name=server_name,
            )
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.set_status(f"[red]{e.message}[/red]")
            return
        finally:
            self.query_one("#na-create", Button).disabled = False
        self.set_status("[green]Link created.[/green]")
        self.show_link(link, platform, group_ids)

    def show_link(self, link: EnrollmentLink, platform: str, group_ids: list[int]) -> None:
        self.link, self.link_platform = link, platform
        groups = ""
        if group_ids:
            groups = f" It joins: {', '.join(self._group_name(g) for g in group_ids)}."
        self.query_one("#na-summary", Static).update(
            f"Single use, valid until [b]{link.expires_at.astimezone():%Y-%m-%d %H:%M}[/b]. "
            f"The agent connects to [b]{link.server}[/b] (TLS name {link.server_name}).{groups}"
        )
        has_msi = link.msi_url is not None
        self.query_one("#na-msi").display = has_msi
        if has_msi:
            self.query_one("#na-msi-url", Input).value = link.msi_url or ""
            self.query_one("#na-save-path", Input).value = str(default_msi_path())
        self.query_one("#na-exe-url", Input).value = link.download_url
        self.query_one("#na-command", Input).value = install_command(link, platform)
        result = self.query_one("#na-result")
        result.display = True
        self.call_after_refresh(result.scroll_visible)

    def _group_name(self, group_id: int) -> str:
        selection = self.query_one("#na-groups", SelectionList)
        for index in range(selection.option_count):
            option = selection.get_option_at_index(index)
            if option.value == group_id:
                return str(option.prompt)
        return str(group_id)

    def _copy(self, text: str, what: str) -> None:
        self.app.copy_to_clipboard(text)
        self.app.notify(f"{what} copied to the clipboard.")

    @on(Button.Pressed, "#na-copy-msi")
    def copy_msi(self) -> None:
        self._copy(self.query_one("#na-msi-url", Input).value, "MSI link")

    @on(Button.Pressed, "#na-copy-exe")
    def copy_exe(self) -> None:
        self._copy(self.query_one("#na-exe-url", Input).value, "Download link")

    @on(Button.Pressed, "#na-copy-command")
    def copy_command(self) -> None:
        self._copy(self.query_one("#na-command", Input).value, "Install command")

    @on(Button.Pressed, "#na-save")
    def save_msi(self) -> None:
        if self.link is None or self.link.msi_url is None:
            return
        raw = self.query_one("#na-save-path", Input).value.strip()
        if not raw:
            self.app.notify("Enter where to save the MSI.", severity="warning")
            return
        path = Path(raw).expanduser()
        if path.is_dir():
            path = unique_path(path / "rmm-agent.msi")
        self.query_one("#na-save", Button).disabled = True
        self.download(self.link.msi_url, path)

    @work(exclusive=True, group="download")
    async def download(self, url: str, path: Path) -> None:
        try:
            size = await self.app.session.api.download(url, path)
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        finally:
            self.query_one("#na-save", Button).disabled = False
        self.app.notify(f"Saved {path} ({size / 1_048_576:.1f} MiB).")
        # Ready for another copy without overwriting this one.
        self.query_one("#na-save-path", Input).value = str(unique_path(path))
