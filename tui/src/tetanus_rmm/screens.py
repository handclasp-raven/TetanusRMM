"""Login, agent list (with its column menu, classify dialog and stats
panel) and audit screens."""

from __future__ import annotations

import asyncio
import json
import logging
from datetime import UTC, datetime
from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING

from rich.text import Text
from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen, Screen
from textual.timer import Timer
from textual.widgets import (
    Button,
    DataTable,
    Footer,
    Header,
    Input,
    OptionList,
    SelectionList,
    Static,
)
from textual.widgets.option_list import Option
from textual.widgets.selection_list import Selection

from . import formatting
from .api import (
    CLASSIFICATIONS,
    DESKTOP,
    SCRIPT,
    SHELL,
    Agent,
    ApiError,
    Forbidden,
    Unauthorized,
    UntrustedServer,
    derived_classification,
)
from .config import ConfigError, normalize_server_url
from .menu import MenuBar
from .stats import StatsPanel, TelemetryCache
from .trust import fingerprint
from .viewer import ViewerError

if TYPE_CHECKING:
    from .app import RmmApp

log = logging.getLogger(__name__)


class SplashScreen(Screen):
    def compose(self) -> ComposeResult:
        yield Static("Connecting…", id="splash")


class TrustScreen(ModalScreen[bool]):
    """First contact with a server whose certificate is signed by its own
    CA: show the CA's fingerprint and ask whether to trust it. Dismissed
    with ``True`` to trust it."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, server_url: str, ca_fingerprint: str) -> None:
        super().__init__()
        self.server_url = server_url
        self.ca_fingerprint = ca_fingerprint

    def compose(self) -> ComposeResult:
        pairs = self.ca_fingerprint.split(":")
        half = len(pairs) // 2
        with Vertical(classes="dialog", id="trust-box"):
            yield Static(Text("Trust this server?", style="bold"))
            yield Static(
                Text(
                    f"{self.server_url} uses its own certificate authority, which this "
                    "computer has not seen before. Its SHA-256 fingerprint is:"
                ),
                classes="dialog-text",
            )
            yield Static(
                Text(":".join(pairs[:half]) + "\n" + ":".join(pairs[half:]), style="bold"),
                id="trust-fingerprint",
            )
            yield Static(
                Text(
                    "Check it against the fingerprint from your administrator. Once "
                    "trusted, you are not asked again for this server."
                ),
                classes="dialog-text",
            )
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="cancel")
                yield Button("Trust", variant="primary", id="trust")

    def on_mount(self) -> None:
        self.query_one("#cancel", Button).focus()

    @on(Button.Pressed, "#trust")
    def trust(self) -> None:
        self.dismiss(True)

    @on(Button.Pressed, "#cancel")
    def action_cancel(self) -> None:
        self.dismiss(False)


class LoginScreen(Screen):
    """Server, username, password, TOTP. The server defaults to the one
    signed in to last."""

    app: RmmApp

    def __init__(self, message: str | None = None) -> None:
        super().__init__()
        self.message = message

    def compose(self) -> ComposeResult:
        with Vertical(id="login-box"):
            yield Static("[b]Sign in[/b]", id="login-title")
            yield Input(
                self.app.config.server_url,
                placeholder="Server URL (https://host:8443)",
                id="server",
            )
            yield Input(placeholder="Username", id="username")
            yield Input(placeholder="Password", password=True, id="password")
            yield Input(placeholder="TOTP code", id="code", restrict=r"[0-9]*", max_length=8)
            yield Button("Sign in", variant="primary", id="sign-in")
            yield Static(self.message or "", id="login-status")

    def on_mount(self) -> None:
        self.query_one("#username", Input).focus()

    @on(Input.Submitted)
    def next_field(self, event: Input.Submitted) -> None:
        order = ["server", "username", "password", "code"]
        index = order.index(event.input.id or "")
        if index + 1 < len(order):
            self.query_one(f"#{order[index + 1]}", Input).focus()
        else:
            self.submit()

    @on(Button.Pressed, "#sign-in")
    def submit(self) -> None:
        try:
            server_url = normalize_server_url(self.query_one("#server", Input).value)
        except ConfigError as e:
            self.set_status(str(e))
            self.query_one("#server", Input).focus()
            return
        self.query_one("#server", Input).value = server_url
        username = self.query_one("#username", Input).value.strip()
        password = self.query_one("#password", Input).value
        code = self.query_one("#code", Input).value.strip()
        if not (username and password and code):
            self.set_status("Enter username, password and TOTP code.")
            return
        self.set_status("Signing in…")
        self.query_one("#sign-in", Button).disabled = True
        self.sign_in(server_url, username, password, code)

    def set_status(self, text: str) -> None:
        self.query_one("#login-status", Static).update(text)

    @work(exclusive=True)
    async def sign_in(self, server_url: str, username: str, password: str, code: str) -> None:
        try:
            await self.app.use_server(server_url)
            try:
                await self.app.session.login(username, password, code)
            except UntrustedServer as e:
                # Nothing was sent: the TLS handshake failed first.
                await self.accept_ca(server_url, e)
                await self.app.session.login(username, password, code)
        except ApiError as e:
            self.set_status(e.message)
            self.query_one("#code", Input).value = ""
            self.query_one("#sign-in", Button).disabled = False
            return
        if not self.app.session.persisted:
            self.app.notify(
                "Signed in, but the OS keyring is unavailable: you will need to sign in "
                "again next time.",
                severity="warning",
            )
        self.app.remember_server()
        self.app.show_main()

    async def accept_ca(self, server_url: str, error: UntrustedServer) -> None:
        """Offer to trust the server's CA, on first contact only. Returns
        once it is trusted; raises :class:`ApiError` otherwise."""
        if self.app.config.ca_path is not None:
            raise ApiError(0, f"{error.message} (checked against {self.app.config.ca_path})")
        if self.app.trust.pinned(server_url) is not None:
            raise ApiError(
                0,
                "The server's certificate no longer matches the one you trusted. If an "
                "administrator confirms its certificates were replaced, run "
                f"`tetanus-rmm --forget-ca --server-url {server_url}` and sign in again. "
                "Otherwise the connection may be being intercepted.",
            )
        self.set_status("Checking the server's certificate…")
        pem = await self.app.fetch_ca(server_url)
        if not await self.app.push_screen_wait(TrustScreen(server_url, fingerprint(pem))):
            raise ApiError(0, "Not signed in: the server's certificate was not trusted.")
        await self.app.trust_server(server_url, pem)
        self.set_status("Signing in…")


class ColumnsScreen(ModalScreen[list[str] | None]):
    """Pick and order the agent table's columns. Dismissed with the new
    column keys, or ``None`` if cancelled."""

    BINDINGS = [
        Binding("escape", "cancel", "Cancel"),
        Binding("shift+up", "move(-1)", "Move up", show=False),
        Binding("shift+down", "move(1)", "Move down", show=False),
    ]

    def __init__(self, columns: list[str]) -> None:
        super().__init__()
        self.shown = set(columns)
        # Shown columns in their order, then the rest in the menu's order.
        self.order = list(columns) + [k for k in formatting.COLUMNS if k not in self.shown]

    def compose(self) -> ComposeResult:
        with Vertical(id="columns-box"):
            yield Static("[b]Agent table columns[/b]", id="columns-title")
            yield SelectionList[str](id="columns-list")
            yield Static("Space: show/hide · Shift+↑/↓: move · Esc: cancel", id="columns-help")
            with Horizontal(id="columns-buttons"):
                yield Button("▲", id="column-up", tooltip="Move up (Shift+↑)")
                yield Button("▼", id="column-down", tooltip="Move down (Shift+↓)")
                yield Button("Defaults", id="column-defaults")
                yield Button("Cancel", id="column-cancel")
                yield Button("Apply", variant="primary", id="column-apply")

    def on_mount(self) -> None:
        self.fill(highlight=0)
        self.query_one("#columns-list").focus()

    def fill(self, highlight: int) -> None:
        options = self.query_one("#columns-list", SelectionList)
        options.clear_options()
        options.add_options(
            [Selection(formatting.COLUMNS[key].label, key, key in self.shown) for key in self.order]
        )
        options.highlighted = highlight

    @on(SelectionList.SelectedChanged, "#columns-list")
    def toggled(self, event: SelectionList.SelectedChanged) -> None:
        self.shown = set(event.selection_list.selected)

    def action_move(self, step: int) -> None:
        index = self.query_one("#columns-list", SelectionList).highlighted
        if index is None or not 0 <= index + step < len(self.order):
            return
        order = self.order
        order[index], order[index + step] = order[index + step], order[index]
        self.fill(highlight=index + step)

    @on(Button.Pressed, "#column-up")
    def up(self) -> None:
        self.action_move(-1)

    @on(Button.Pressed, "#column-down")
    def down(self) -> None:
        self.action_move(1)

    @on(Button.Pressed, "#column-defaults")
    def defaults(self) -> None:
        self.shown = set(formatting.DEFAULT_COLUMNS)
        self.order = list(formatting.DEFAULT_COLUMNS) + [
            k for k in formatting.COLUMNS if k not in self.shown
        ]
        self.fill(highlight=0)

    @on(Button.Pressed, "#column-apply")
    def apply(self) -> None:
        columns = [k for k in self.order if k in self.shown]
        if not columns:
            self.app.notify("Show at least one column.", severity="warning")
            return
        self.dismiss(columns)

    @on(Button.Pressed, "#column-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)


class ClassifyScreen(ModalScreen[str | None]):
    """Pick an agent's classification. Dismissed with one of
    :data:`CLASSIFICATIONS`, :attr:`AUTO` to let the device kind decide,
    or ``None`` if cancelled."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    AUTO = "auto"

    def __init__(self, agent: Agent) -> None:
        super().__init__()
        self.agent = agent

    def compose(self) -> ComposeResult:
        derived = derived_classification(self.agent.device_kind)
        with Vertical(classes="dialog", id="classify-box"):
            yield Static(Text.assemble(("Classify ", "bold"), (self.agent.label, "bold")))
            yield OptionList(
                *[Option(formatting.classification_label(c), id=c) for c in CLASSIFICATIONS],
                Option(f"Automatic ({formatting.classification_label(derived)})", id=self.AUTO),
                id="classify-list",
            )
            yield Static("Enter: choose · Esc: cancel", classes="dialog-help")

    def on_mount(self) -> None:
        options = self.query_one("#classify-list", OptionList)
        options.highlighted = options.get_option_index(
            self.agent.classification_override or self.AUTO
        )
        options.focus()

    @on(OptionList.OptionSelected, "#classify-list")
    def chosen(self, event: OptionList.OptionSelected) -> None:
        self.dismiss(event.option.id)

    def action_cancel(self) -> None:
        self.dismiss(None)


class MainScreen(Screen):
    """Live table of agents, and the actions on the selected one. A search
    box over it filters by hostname, IP address and group name, and a list
    of groups beside it by group."""

    app: RmmApp

    # The footer shows the essentials; the menu bar has them all.
    BINDINGS = [
        Binding("m,f10", "menu", "Menu"),
        Binding("d", "desktop", "Remote desktop"),
        Binding("s", "shell", "Shell"),
        Binding("r", "scripts", "Scripts", show=False),
        Binding("n", "new_agent", "New agent", show=False),
        Binding("i", "deployment", "Deployment MSI", show=False),
        Binding("h", "quick_assist", "Quick assist", show=False),
        Binding("k", "classify", "Classify", show=False),
        Binding("g", "groups", "Groups", show=False),
        Binding("u", "users", "Users", show=False),
        Binding("slash", "search", "Search"),
        Binding("f", "filter", "Filter by group", show=False),
        Binding("a", "audit", "Audit log", show=False),
        Binding("c", "columns", "Columns", show=False),
        Binding("p", "stats", "Stats panel", show=False),
        Binding("t", "app.change_theme", "Theme", show=False),
        Binding("e", "themes", "Edit themes", show=False),
        Binding("v", "viewer_commands", "Viewer buttons", show=False),
        Binding("b", "bitwarden", "Bitwarden: unlock / lock", show=False),
        Binding("B", "branding", "Company branding", show=False),
        Binding("U", "check_updates", "Check for updates", show=False),
        Binding("f5", "refresh", "Refresh", show=False),
        Binding("l", "logout", "Sign out", show=False),
        Binding("q", "app.quit", "Quit"),
        Binding("escape", "clear_search", "Clear search", show=False),
    ]

    #: The menu bar: each menu's name and the actions in it.
    MENUS = {
        "Agent": [
            "desktop",
            "shell",
            "scripts",
            "classify",
            "new_agent",
            "deployment",
            "quick_assist",
        ],
        "View": ["search", "filter", "columns", "stats", "app.change_theme", "themes", "refresh"],
        "Manage": ["groups", "users", "audit", "viewer_commands", "branding"],
        "Session": ["bitwarden", "check_updates", "logout", "app.quit"],
    }

    #: Actions on the selected agent, and the capability each needs there.
    AGENT_ACTIONS = {"desktop": DESKTOP, "shell": SHELL}

    #: Group list entries (besides ``group:<name>``).
    ALL_GROUPS = "all"
    NO_GROUP = "none"

    #: Seconds the selection must rest on an agent before its telemetry
    #: history is fetched: moving through the list fetches nothing.
    STATS_DEBOUNCE = 0.25

    def __init__(self) -> None:
        super().__init__()
        #: Every agent the user can see; the table shows those the filter lets through.
        self.agents: dict[str, Agent] = {}
        self.columns: list[str] = []
        #: Names of the groups the server has (for the group list).
        self.group_names: set[str] = set()
        self.group_filter = self.ALL_GROUPS
        #: The group list's entries as last shown: values, and (value,
        #: prompt) pairs.
        self.filter_values: list[str] = []
        self.group_entries: list[tuple[str, str]] = []
        #: Lower-cased search text; agents with it in their hostname, an IP
        #: address or a group name are shown.
        self.search = ""
        #: Telemetry history already fetched, for the stats panel.
        self.stats_cache = TelemetryCache()
        #: The pending fetch for the agent the selection rests on.
        self.stats_timer: Timer | None = None
        #: The agent whose history is being fetched.
        self.stats_loading: str | None = None
        #: Why an agent has no history to show, by agent id.
        self.stats_errors: dict[str, str] = {}
        #: The server is too old to keep telemetry history.
        self.stats_unavailable = False

    def compose(self) -> ComposeResult:
        yield Header()
        yield MenuBar(self.MENUS, id="menu-bar")
        yield Static(self._whoami(), id="whoami")
        yield Input(placeholder="Search by hostname, IP address or group  ( / )", id="search")
        with Horizontal(id="agent-body"):
            with Vertical(id="group-pane"):
                yield Static("[b]Groups[/b]", id="group-title")
                yield OptionList(id="group-list")
            # The status dot keeps its colour on the highlighted row.
            yield DataTable(
                id="agents",
                cursor_type="row",
                zebra_stripes=True,
                cursor_foreground_priority="renderable",
            )
        yield StatsPanel(id="stats")
        yield Static("Loading agents…", id="status")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "TetanusRMM"
        self.set_columns(formatting.valid_columns(self.app.state.agent_columns))
        self.update_group_options()
        self.query_one("#stats", StatsPanel).display = self.app.state.stats_panel
        self.query_one("#agents", DataTable).focus()
        self.refresh_agents()
        self.load_groups()
        self.set_interval(self.app.config.poll_interval, self.refresh_agents)
        if self.app.config.check_updates:
            self.check_for_update(announce=False)

    def action_check_updates(self) -> None:
        self.check_for_update(announce=True)

    @work(exclusive=True, group="update")
    async def check_for_update(self, announce: bool) -> None:
        """Offer the TUI the server publishes if it is newer than this one.
        ``announce``: also say when there is nothing to do, or why not."""
        from . import __version__, selfupdate

        app = self.app
        try:
            build = await app.session.api.tui_build()
        except ApiError as e:
            log.warning("could not check for a newer TUI: %s", e.message)
            if announce:
                app.notify(f"Could not check for updates: {e.message}", severity="error")
            return
        if build is None or not selfupdate.is_newer(build.version):
            if announce:
                app.notify(f"You are up to date ({__version__}).")
            return

        def chosen(wheel: Path | None) -> None:
            if wheel is not None:
                app.exit(selfupdate.PendingUpdate(build.version, wheel))

        directory = app.log_dir / selfupdate.UPDATES_DIR
        app.push_screen(selfupdate.UpdateScreen(app.session.api, build, directory), chosen)

    def _whoami(self) -> str:
        user = self.app.session.user
        role = user.role.replace("_", " ") if user else ""
        vault = " · vault unlocked" if self.app.bw_session else ""
        return f"{user.username if user else ''} ({role}) @ {self.app.config.server_url}{vault}"

    def check_action(self, action: str, parameters: tuple[object, ...]) -> bool | None:
        user = self.app.session.user
        if action in self.AGENT_ACTIONS:
            # Per agent: support engineers may only act where granted.
            agent = self.selected()
            if agent is None:
                return bool(user and user.can_control)
            return agent.allows(self.AGENT_ACTIONS[action], user)
        if action == "scripts":
            if self.agents:
                return any(a.allows(SCRIPT, user) for a in self.agents.values())
            return bool(user and user.can_control)
        if action == "audit":
            return bool(user and user.can_read_audit)
        if action in ("new_agent", "deployment"):
            return bool(user and user.can_enroll)
        if action == "quick_assist":
            return bool(user and user.can_control)
        if action in ("classify", "users", "branding"):
            return bool(user and user.is_admin)
        return True

    @on(DataTable.RowHighlighted, "#agents")
    def selection_moved(self) -> None:
        # What the footer offers depends on the selected agent.
        self.refresh_bindings()
        self.update_stats()

    # --- agent list -----------------------------------------------------------

    @work(exclusive=True, group="poll")
    async def refresh_agents(self) -> None:
        try:
            agents = await self.app.session.api.list_agents()
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.query_one("#status", Static).update(f"[red]{e.message}[/red]")
            return
        self.show_agents(agents)

    def show_agents(self, agents: list[Agent]) -> None:
        self.agents = {a.id: a for a in agents}
        self.update_group_options()
        self.update_table()

    def matches(self, agent: Agent) -> bool:
        """Whether the search and the group list let ``agent`` through."""
        if self.search:
            searched = (agent.label, agent.local_ip, agent.remote_ip, *agent.groups)
            if not any(self.search in text.lower() for text in searched if text):
                return False
        if self.group_filter == self.ALL_GROUPS:
            return True
        if self.group_filter == self.NO_GROUP:
            return not agent.groups
        return self.group_filter.removeprefix("group:") in agent.groups

    def update_table(self) -> None:
        """Bring the rows in line with the agents and the filter: rows are
        updated in place, so the selection and scroll position stay."""
        table = self.query_one("#agents", DataTable)
        now = datetime.now(UTC)
        visible = {a.id: a for a in self.agents.values() if self.matches(a)}
        shown = {str(key.value) for key in table.rows}
        for gone in shown - set(visible):
            table.remove_row(gone)
        for agent_id in shown & set(visible):
            agent = visible[agent_id]
            for key, value in zip(
                self.columns, formatting.agent_row(agent, now, self.columns), strict=True
            ):
                table.update_cell(agent_id, key, value)
        new = [a for a in visible.values() if a.id not in shown]
        # Online agents first, then by name.
        for agent in sorted(new, key=lambda a: (not a.online, a.label.lower())):
            table.add_row(*formatting.agent_row(agent, now, self.columns), key=agent.id)
        self.refresh_bindings()
        self.update_stats()
        online = sum(a.online for a in visible.values())
        count = f"{len(visible)} agents"
        if len(visible) != len(self.agents):
            count = f"{len(visible)} of {len(self.agents)} agents"
        self.query_one("#status", Static).update(
            f"{count}, {online} online · updated {now.astimezone():%H:%M:%S}"
        )

    # --- stats panel ----------------------------------------------------------

    def update_stats(self) -> None:
        """Show the selected agent in the stats panel from what is already
        known (the agent list and any history held), and fetch its history
        once the selection has rested on it, unless what is held is fresh."""
        if not self.app.state.stats_panel:
            return
        panel = self.query_one("#stats", StatsPanel)
        agent = self.selected()
        if self.stats_timer is not None:
            self.stats_timer.stop()
            self.stats_timer = None
        if agent is None:
            panel.show(None)
            panel.loading_history = False
            return
        cache = self.stats_cache
        note = self.stats_errors.get(agent.id, "")
        if self.stats_unavailable:
            note = "no history on this server"
        panel.show(agent, cache.get(agent.id), step=cache.step(agent.id), note=note)
        if self.stats_loading == agent.id:
            panel.loading_history = True
        elif self.stats_unavailable or cache.fresh(agent.id):
            panel.loading_history = False
        else:
            panel.loading_history = True
            self.stats_timer = self.set_timer(
                self.STATS_DEBOUNCE, partial(self.load_stats, agent.id)
            )

    # Exclusive: a fetch for an agent the selection has left is cancelled.
    @work(exclusive=True, group="stats")
    async def load_stats(self, agent_id: str) -> None:
        self.stats_timer = None
        self.stats_loading = agent_id
        cache = self.stats_cache
        try:
            history = await self.app.session.api.agent_telemetry(
                agent_id, since=cache.last(agent_id)
            )
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            if e.status == 404:
                # The agent is one the server lists, so it is the route
                # that is missing: do not ask again.
                self.stats_unavailable = True
            else:
                self.stats_errors[agent_id] = e.message
                cache.failed(agent_id)
        else:
            self.stats_errors.pop(agent_id, None)
            cache.store(agent_id, history)
        finally:
            if self.stats_loading == agent_id:
                self.stats_loading = None
        self.update_stats()

    def action_stats(self) -> None:
        """Show or hide the stats panel. Hidden, it fetches nothing."""
        shown = not self.app.state.stats_panel
        self.app.state.stats_panel = shown
        self.app.state.save()
        self.query_one("#stats", StatsPanel).display = shown
        if shown:
            self.update_stats()
            return
        if self.stats_timer is not None:
            self.stats_timer.stop()
            self.stats_timer = None
        self.workers.cancel_group(self, "stats")

    # --- search ----------------------------------------------------------------

    @on(Input.Changed, "#search")
    def search_changed(self, event: Input.Changed) -> None:
        self.search = event.value.strip().lower()
        self.update_table()

    @on(Input.Submitted, "#search")
    def search_submitted(self) -> None:
        self.query_one("#agents", DataTable).focus()

    def action_menu(self) -> None:
        self.query_one(MenuBar).open()

    def action_search(self) -> None:
        self.query_one("#search", Input).focus()

    def action_clear_search(self) -> None:
        search = self.query_one("#search", Input)
        search.value = ""
        self.query_one("#agents", DataTable).focus()

    # --- group list -----------------------------------------------------------

    def group_options(self) -> list[tuple[str, str]]:
        """(value, prompt) for every entry of the group list: all agents,
        those in no group, then each group, with how many agents each has."""
        names = set(self.group_names)
        counts: dict[str, int] = {}
        for agent in self.agents.values():
            names.update(agent.groups)
            for name in agent.groups:
                counts[name] = counts.get(name, 0) + 1
        ungrouped = sum(not a.groups for a in self.agents.values())
        return [
            (self.ALL_GROUPS, f"All agents ({len(self.agents)})"),
            (self.NO_GROUP, f"No group ({ungrouped})"),
            *[
                (f"group:{name}", f"{name} ({counts.get(name, 0)})")
                for name in sorted(names, key=str.lower)
            ],
        ]

    def update_group_options(self) -> None:
        """List every group there is, keeping the choice while it exists."""
        entries = self.group_options()
        if entries == self.group_entries:
            return
        self.group_entries = entries
        self.filter_values = [value for value, _ in entries]
        if self.group_filter not in self.filter_values:
            self.group_filter = self.ALL_GROUPS
        group_list = self.query_one("#group-list", OptionList)
        with group_list.prevent(OptionList.OptionHighlighted):
            group_list.clear_options()
            # Text, not markup: group names are the admins' own.
            group_list.add_options([Option(Text(prompt), id=value) for value, prompt in entries])
            group_list.highlighted = self.filter_values.index(self.group_filter)

    @work(exclusive=True, group="groups")
    async def load_groups(self) -> None:
        try:
            groups = await self.app.session.api.list_groups()
        except ApiError:
            return  # an older server has no groups; the agents still list theirs
        self.group_names = {g.name for g in groups}
        self.update_group_options()
        self.update_table()

    @on(OptionList.OptionHighlighted, "#group-list")
    def group_highlighted(self, event: OptionList.OptionHighlighted) -> None:
        self.group_filter = str(event.option.id)
        self.update_table()

    @on(OptionList.OptionSelected, "#group-list")
    def group_selected(self) -> None:
        self.query_one("#agents", DataTable).focus()

    def action_filter(self) -> None:
        self.query_one("#group-list", OptionList).focus()

    def set_columns(self, columns: list[str]) -> None:
        """Rebuild the table with ``columns``, keeping the selection."""
        table = self.query_one("#agents", DataTable)
        selected = self.selected()
        self.columns = columns
        table.clear(columns=True)
        for key in columns:
            table.add_column(formatting.COLUMNS[key].label, key=key)
        self.update_table()
        if selected is not None and selected.id in table.rows:
            table.move_cursor(row=table.get_row_index(selected.id))

    def action_columns(self) -> None:
        def chosen(columns: list[str] | None) -> None:
            if columns is None or columns == self.columns:
                return
            self.set_columns(columns)
            self.app.state.agent_columns = columns
            self.app.state.save()

        self.app.push_screen(ColumnsScreen(self.columns), chosen)

    def selected(self) -> Agent | None:
        table = self.query_one("#agents", DataTable)
        if not table.row_count:
            return None
        row_key, _ = table.coordinate_to_cell_key(table.cursor_coordinate)
        return self.agents.get(str(row_key.value))

    def action_refresh(self) -> None:
        self.refresh_agents()

    # --- actions ----------------------------------------------------------------

    @on(DataTable.RowSelected, "#agents")
    def row_selected(self) -> None:
        if self.check_action("desktop", ()):
            self.action_desktop()

    def _online_selection(self, what: str) -> Agent | None:
        agent = self.selected()
        if agent is None:
            self.app.notify("No agent selected.", severity="warning")
            return None
        if not agent.online:
            self.app.notify(f"{agent.label} is offline: cannot {what}.", severity="error")
            return None
        return agent

    def action_desktop(self) -> None:
        agent = self._online_selection("start a remote session")
        if agent and self._allowed(agent, DESKTOP, "remote desktop"):
            self.launch_viewer(agent)

    def _allowed(self, agent: Agent, capability: str, what: str) -> bool:
        if agent.allows(capability, self.app.session.user):
            return True
        self.app.notify(f"You have no {what} access to {agent.label}.", severity="error")
        return False

    @work(group="viewer")
    async def launch_viewer(self, agent: Agent) -> None:
        try:
            # A quick assist session allows no command buttons.
            commands = [] if agent.quick_assist else self.app.state.commands
            process = await self.app.start_viewer(agent.id, commands)
            if process is None:
                self.app.notify(f"{agent.label} went offline.", severity="error")
                return
        except Unauthorized:
            self.app.session_expired()
            return
        except Forbidden:
            self.app.notify(
                f"You may not start remote sessions on {agent.label}.", severity="error"
            )
            return
        except (ApiError, ViewerError) as e:
            self.app.notify(str(e), severity="error")
            return
        self.app.notify(f"Viewer started for {agent.label} (pid {process.pid}).")

    def action_shell(self) -> None:
        from .console import ConsoleScreen

        agent = self._online_selection("open a shell")
        if agent and self._allowed(agent, SHELL, "shell"):
            self.app.push_screen(ConsoleScreen(agent))

    def action_scripts(self) -> None:
        from .runner import ScriptScreen

        selected = self.selected()
        self.app.push_screen(
            ScriptScreen(
                list(self.agents.values()),
                [selected.id] if selected else [],
                self.app.session.user,
            )
        )

    def action_audit(self) -> None:
        self.app.push_screen(AuditScreen())

    def action_viewer_commands(self) -> None:
        from .commands import CommandsScreen

        def chosen(commands: list | None) -> None:
            if commands is None:
                return
            self.app.state.viewer_commands = commands
            self.app.state.save()
            self.app.notify(
                f"Saved {len(commands)} viewer buttons; viewers started from now on show them."
            )

        self.app.push_screen(CommandsScreen(self.app.state.commands), chosen)

    def action_themes(self) -> None:
        from .theme_editor import ThemeEditorScreen

        self.app.push_screen(ThemeEditorScreen())

    def action_classify(self) -> None:
        agent = self.selected()
        if agent is None:
            self.app.notify("No agent selected.", severity="warning")
            return

        def chosen(choice: str | None) -> None:
            if choice is None:
                return
            classification = None if choice == ClassifyScreen.AUTO else choice
            if classification != agent.classification_override:
                self.classify(agent, classification)

        self.app.push_screen(ClassifyScreen(agent), chosen)

    @work(group="classify")
    async def classify(self, agent: Agent, classification: str | None) -> None:
        try:
            now = await self.app.session.api.set_classification(agent.id, classification)
        except Unauthorized:
            self.app.session_expired()
            return
        except Forbidden:
            self.app.notify(f"You may not classify {agent.label}.", severity="error")
            return
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        self.app.notify(f"{agent.label} is now {formatting.classification_label(now).lower()}.")
        self.refresh_agents()

    def action_new_agent(self) -> None:
        from .enroll import NewAgentScreen

        self.app.push_screen(NewAgentScreen())

    def action_deployment(self) -> None:
        from .deploy import DeploymentScreen

        self.app.push_screen(DeploymentScreen())

    def action_quick_assist(self) -> None:
        from .assist import QuickAssistScreen

        self.app.push_screen(QuickAssistScreen())

    def action_groups(self) -> None:
        from .groups import GroupsScreen

        def closed(_: object) -> None:
            # Groups may have changed: names, members, the filter's options.
            self.refresh_agents()
            self.load_groups()

        self.app.push_screen(GroupsScreen(list(self.agents.values())), closed)

    def action_users(self) -> None:
        from .users import UsersScreen

        self.app.push_screen(UsersScreen(list(self.agents.values())))

    def action_branding(self) -> None:
        from .branding import BrandingScreen

        self.app.push_screen(BrandingScreen(self.app.session.api))

    def action_bitwarden(self) -> None:
        """Unlock the Bitwarden vault for the viewers, or lock it again."""
        from . import bitwarden

        app = self.app
        if app.bw_session is not None:
            self._lock_vault()
            return

        def unlocked(session: str | None) -> None:
            if session is None:
                return
            app.bw_session = session
            self.query_one("#whoami", Static).update(self._whoami())
            app.notify(
                "Vault unlocked: viewers started from now on can use it. "
                "It is locked again when you quit or sign out."
            )

        unlock = partial(bitwarden.unlock, app.bw_path)
        app.push_screen(bitwarden.UnlockScreen(unlock), unlocked)

    @work(group="bitwarden")
    async def _lock_vault(self) -> None:
        from . import bitwarden

        try:
            await asyncio.to_thread(self.app.lock_vault)
        except bitwarden.BitwardenError as e:
            self.app.notify(f"Could not lock the vault: {e}", severity="error")
        else:
            self.app.notify("Vault locked, for the viewers already open too.")
        self.query_one("#whoami", Static).update(self._whoami())

    @work(exclusive=True, group="logout")
    async def action_logout(self) -> None:
        from . import bitwarden

        try:
            # The next person to sign in here does not get this vault.
            await asyncio.to_thread(self.app.lock_vault)
        except bitwarden.BitwardenError as e:
            self.app.notify(f"Could not lock the vault: {e}", severity="error")
        await self.app.session.logout()
        self.app.show_login("Signed out.")


class AuditScreen(Screen):
    """Recent audit entries and the chain check (admins and auditors)."""

    app: RmmApp

    BINDINGS = [
        Binding("escape", "app.pop_screen", "Back"),
        Binding("f5", "refresh", "Refresh"),
        Binding("v", "verify", "Verify chain"),
    ]

    def compose(self) -> ComposeResult:
        yield Header()
        yield DataTable(id="audit", cursor_type="row", zebra_stripes=True)
        yield Static("", id="chain")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "Audit log"
        table = self.query_one("#audit", DataTable)
        table.add_columns("#", "Time", "Actor", "Action", "Target", "Detail")
        table.focus()
        self.action_refresh()

    @work(exclusive=True, group="audit")
    async def action_refresh(self) -> None:
        try:
            entries = await self.app.session.api.audit(limit=200)
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.query_one("#chain", Static).update(f"[red]{e.message}[/red]")
            return
        table = self.query_one("#audit", DataTable)
        table.clear()
        for e in entries:
            detail = json.dumps(e.detail, separators=(",", ":")) if e.detail else ""
            table.add_row(
                str(e.id),
                f"{e.ts.astimezone():%Y-%m-%d %H:%M:%S}",
                e.actor,
                e.action,
                e.target or "",
                detail[:120],
            )

    @work(exclusive=True, group="verify")
    async def action_verify(self) -> None:
        chain = self.query_one("#chain", Static)
        chain.update("Verifying the audit chain…")
        try:
            result = await self.app.session.api.verify_audit()
        except ApiError as e:
            chain.update(f"[red]{e.message}[/red]")
            return
        if result.get("status") == "valid":
            chain.update(f"[green]Chain intact[/green]: {result.get('entries')} entries verified.")
        else:
            chain.update(
                f"[red]Chain BROKEN[/red] at entry {result.get('id')}: {result.get('reason')}"
            )
