"""Deployment MSI: make a reusable deployment key and save its MSI, which
installs and enrolls the agent unattended on any number of PCs (Group
Policy, Intune). Lists the keys made so far, and revokes them."""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import Screen
from textual.widgets import (
    Button,
    DataTable,
    Footer,
    Header,
    Input,
    Label,
    Select,
    SelectionList,
    Static,
)

from .api import ApiError, DeploymentKey, NewDeploymentKey, Unauthorized
from .enroll import default_server, unique_path
from .groups import ConfirmScreen

if TYPE_CHECKING:
    from .app import RmmApp

NEVER = 0
LIFETIMES = [
    ("30 days", 30 * 86400),
    ("90 days", 90 * 86400),
    ("1 year", 365 * 86400),
    ("Never expires", NEVER),
]
DEFAULT_LIFETIME = 90 * 86400
MSI_NAME = "rmm-agent-deploy.msi"


def default_msi_path() -> Path:
    downloads = Path.home() / "Downloads"
    folder = downloads if downloads.is_dir() else Path.home()
    return unique_path(folder / MSI_NAME)


def install_command(path: Path) -> str:
    """Installs the MSI with no UI at all."""
    return f"msiexec /i {path.name} /qn /norestart"


def local_time(when: datetime) -> str:
    return f"{when.astimezone():%Y-%m-%d %H:%M}"


def key_status(key: DeploymentKey, now: datetime) -> str:
    if key.revoked_at is not None:
        return f"[red]Revoked[/red] by {key.revoked_by}"
    if key.expires_at is not None and key.expires_at <= now:
        return "[red]Expired[/red]"
    return "[green]Active[/green]"


class DeploymentScreen(Screen):
    app: RmmApp

    BINDINGS = [
        Binding("escape", "app.pop_screen", "Back"),
        Binding("ctrl+g", "create", "Create key", priority=True),
        Binding("delete", "revoke", "Revoke"),
        Binding("f5", "refresh", "Refresh"),
    ]

    def __init__(self) -> None:
        super().__init__()
        self.keys: dict[int, DeploymentKey] = {}
        self.group_names: dict[int, str] = {}
        self.made: NewDeploymentKey | None = None

    @property
    def is_admin(self) -> bool:
        user = self.app.session.user
        return bool(user and user.is_admin)

    def compose(self) -> ComposeResult:
        config = self.app.config
        yield Header()
        with VerticalScroll(id="deploy"):
            yield Static(
                "A deployment MSI installs the agent silently on as many PCs as you push "
                "it to (Group Policy, Intune), and enrolls each with its key. Anyone "
                "holding the MSI can enroll agents until the key expires or is revoked.",
                id="dp-intro",
            )
            yield DataTable(id="dp-keys", cursor_type="row", zebra_stripes=True)
            yield Label("New key: what it is for (e.g. the customer or site)")
            yield Input(id="dp-name", max_length=64)
            yield Label("Server address the agents connect to (host:port)")
            yield Input(default_server(config), id="dp-server")
            yield Label("TLS name (a name on the server's certificate)")
            yield Input(config.api_host, id="dp-server-name")
            yield Label("Key valid for")
            yield Select(LIFETIMES, value=DEFAULT_LIFETIME, allow_blank=False, id="dp-ttl")
            yield Label("Groups the agents join")
            if self.is_admin:
                yield SelectionList[int](id="dp-groups")
            else:
                yield Static("Only admins can put new agents into groups.", id="dp-groups-note")
            with Horizontal(id="dp-actions"):
                yield Button("Create key  ^G", variant="primary", id="dp-create")
                yield Static("", id="dp-status")
            with Vertical(id="dp-result"):
                yield Static("", id="dp-summary")
                with Horizontal(classes="na-url-row"):
                    yield Input(id="dp-save-path", classes="na-url")
                    yield Button("Save MSI", id="dp-save", variant="success")
                yield Label("Silent install, as the deployment tool runs it:")
                with Horizontal(classes="na-url-row"):
                    yield Input(id="dp-command", classes="na-url")
                    yield Button("Copy", id="dp-copy-command")
                yield Label("MSI link (the key is in it):")
                with Horizontal(classes="na-url-row"):
                    yield Input(id="dp-msi-url", classes="na-url")
                    yield Button("Copy", id="dp-copy-msi")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "Deployment MSI"
        self.query_one("#dp-result").display = False
        table = self.query_one("#dp-keys", DataTable)
        table.add_column("Name", key="name")
        table.add_column("Made by", key="by")
        table.add_column("Expires", key="expires")
        table.add_column("Enrolled", key="enrolled")
        table.add_column("Last enrolled", key="last")
        table.add_column("Groups", key="groups")
        table.add_column("Status", key="status")
        table.focus()
        self.action_refresh()

    def check_action(self, action: str, parameters: tuple[object, ...]) -> bool | None:
        if action == "revoke":
            key = self.selected()
            if key is None or key.revoked_at is not None:
                return None  # shown, greyed out
        return True

    def set_status(self, text: str) -> None:
        self.query_one("#dp-status", Static).update(text)

    @work(exclusive=True, group="keys")
    async def action_refresh(self) -> None:
        api = self.app.session.api
        try:
            groups = await api.list_groups()
            keys = await api.list_deployment_keys()
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.set_status(f"[red]{e.message}[/red]")
            return
        self.group_names = {g.id: g.name for g in groups}
        if self.is_admin:
            selection = self.query_one("#dp-groups", SelectionList)
            chosen = set(selection.selected)
            selection.clear_options()
            selection.add_options(
                [(g.name, g.id, g.id in chosen) for g in sorted(groups, key=lambda g: g.name)]
            )
        self.show_keys(keys)

    def show_keys(self, keys: list[DeploymentKey]) -> None:
        table = self.query_one("#dp-keys", DataTable)
        current = self.selected()
        self.keys = {k.id: k for k in keys}
        now = datetime.now(UTC)
        table.clear()
        for key in keys:
            table.add_row(
                key.name,
                key.created_by,
                local_time(key.expires_at) if key.expires_at else "Never",
                str(key.enrolled_count),
                local_time(key.last_enrolled_at) if key.last_enrolled_at else "–",
                ", ".join(self.group_names.get(g, str(g)) for g in key.group_ids) or "–",
                key_status(key, now),
                key=str(key.id),
            )
        table.display = bool(keys)
        if current is not None and current.id in self.keys:
            table.move_cursor(row=table.get_row_index(str(current.id)))
        self.refresh_bindings()

    def selected(self) -> DeploymentKey | None:
        table = self.query_one("#dp-keys", DataTable)
        if not table.row_count:
            return None
        row_key, _ = table.coordinate_to_cell_key(table.cursor_coordinate)
        return self.keys.get(int(str(row_key.value)))

    @on(DataTable.RowHighlighted, "#dp-keys")
    def selection_moved(self) -> None:
        self.refresh_bindings()

    @on(Button.Pressed, "#dp-create")
    def action_create(self) -> None:
        name = self.query_one("#dp-name", Input).value.strip()
        server = self.query_one("#dp-server", Input).value.strip()
        server_name = self.query_one("#dp-server-name", Input).value.strip()
        if not name:
            self.set_status("[red]Say what the key is for.[/red]")
            return
        if not server or not server_name:
            self.set_status("[red]Enter the server address and TLS name.[/red]")
            return
        ttl = int(self.query_one("#dp-ttl", Select).value)  # type: ignore[arg-type]
        group_ids = (
            list(self.query_one("#dp-groups", SelectionList).selected) if self.is_admin else []
        )
        self.set_status("Creating…")
        self.query_one("#dp-create", Button).disabled = True
        self.create(name, None if ttl == NEVER else ttl, group_ids, server, server_name)

    @work(exclusive=True, group="create")
    async def create(
        self, name: str, ttl: int | None, group_ids: list[int], server: str, server_name: str
    ) -> None:
        try:
            made = await self.app.session.api.create_deployment_key(
                name=name,
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
            self.query_one("#dp-create", Button).disabled = False
        self.set_status("[green]Key created.[/green]")
        self.show_made(made)
        self.action_refresh()

    def show_made(self, made: NewDeploymentKey) -> None:
        self.made = made
        expires = (
            f"valid until [b]{local_time(made.expires_at)}[/b]"
            if made.expires_at
            else "[b]never expires[/b]"
        )
        self.query_one("#dp-summary", Static).update(
            f"[b]{made.name}[/b]: {expires}. Agents connect to [b]{made.server}[/b] "
            f"(TLS name {made.server_name}). Save the MSI now: the key is shown only "
            "once, so it cannot be made again later (make a new key instead)."
        )
        path = default_msi_path()
        self.query_one("#dp-save-path", Input).value = str(path)
        self.query_one("#dp-command", Input).value = install_command(path)
        self.query_one("#dp-msi-url", Input).value = made.msi_url
        result = self.query_one("#dp-result")
        result.display = True
        self.call_after_refresh(result.scroll_visible)

    def _copy(self, text: str, what: str) -> None:
        self.app.copy_to_clipboard(text)
        self.app.notify(f"{what} copied to the clipboard.")

    @on(Button.Pressed, "#dp-copy-msi")
    def copy_msi(self) -> None:
        self._copy(self.query_one("#dp-msi-url", Input).value, "MSI link")

    @on(Button.Pressed, "#dp-copy-command")
    def copy_command(self) -> None:
        self._copy(self.query_one("#dp-command", Input).value, "Install command")

    @on(Button.Pressed, "#dp-save")
    def save_msi(self) -> None:
        if self.made is None:
            return
        raw = self.query_one("#dp-save-path", Input).value.strip()
        if not raw:
            self.app.notify("Enter where to save the MSI.", severity="warning")
            return
        path = Path(raw).expanduser()
        if path.is_dir():
            path = unique_path(path / MSI_NAME)
        self.query_one("#dp-save", Button).disabled = True
        self.download(self.made.msi_url, path)

    @work(exclusive=True, group="download")
    async def download(self, url: str, path: Path) -> None:
        try:
            size = await self.app.session.api.download(url, path)
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        finally:
            self.query_one("#dp-save", Button).disabled = False
        self.app.notify(f"Saved {path} ({size / 1_048_576:.1f} MiB).")
        self.query_one("#dp-command", Input).value = install_command(path)
        # Ready for another copy without overwriting this one.
        self.query_one("#dp-save-path", Input).value = str(unique_path(path))

    def action_revoke(self) -> None:
        key = self.selected()
        if key is None or key.revoked_at is not None:
            return

        def done(confirmed: bool | None) -> None:
            if confirmed:
                self.run_revoke(key)

        self.app.push_screen(
            ConfirmScreen(
                f"Revoke [b]{key.name}[/b]? Its MSI stops enrolling new agents. "
                f"The {key.enrolled_count} already enrolled keep working.",
                confirm="Revoke",
            ),
            done,
        )

    @work(group="change")
    async def run_revoke(self, key: DeploymentKey) -> None:
        try:
            await self.app.session.api.revoke_deployment_key(key.id)
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        self.app.notify(f"{key.name} revoked.")
        self.action_refresh()
