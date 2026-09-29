"""Login, agent list and audit screens."""

from __future__ import annotations

import json
from datetime import UTC, datetime
from typing import TYPE_CHECKING

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Vertical
from textual.screen import Screen
from textual.widgets import Button, DataTable, Footer, Header, Input, Static

from . import formatting
from .api import DESKTOP, SCRIPT, SHELL, Agent, ApiError, Forbidden, Unauthorized
from .viewer import ViewerError, build_command

if TYPE_CHECKING:
    from .app import RmmApp


class SplashScreen(Screen):
    def compose(self) -> ComposeResult:
        yield Static("Connecting…", id="splash")


class LoginScreen(Screen):
    """Username, password, TOTP."""

    app: RmmApp

    def __init__(self, message: str | None = None) -> None:
        super().__init__()
        self.message = message

    def compose(self) -> ComposeResult:
        with Vertical(id="login-box"):
            yield Static(f"Sign in to [b]{self.app.config.server_url}[/b]", id="login-title")
            yield Input(placeholder="Username", id="username")
            yield Input(placeholder="Password", password=True, id="password")
            yield Input(placeholder="TOTP code", id="code", restrict=r"[0-9]*", max_length=8)
            yield Button("Sign in", variant="primary", id="sign-in")
            yield Static(self.message or "", id="login-status")

    def on_mount(self) -> None:
        self.query_one("#username", Input).focus()

    @on(Input.Submitted)
    def next_field(self, event: Input.Submitted) -> None:
        order = ["username", "password", "code"]
        index = order.index(event.input.id or "")
        if index + 1 < len(order):
            self.query_one(f"#{order[index + 1]}", Input).focus()
        else:
            self.submit()

    @on(Button.Pressed, "#sign-in")
    def submit(self) -> None:
        username = self.query_one("#username", Input).value.strip()
        password = self.query_one("#password", Input).value
        code = self.query_one("#code", Input).value.strip()
        if not (username and password and code):
            self.set_status("Enter username, password and TOTP code.")
            return
        self.set_status("Signing in…")
        self.query_one("#sign-in", Button).disabled = True
        self.sign_in(username, password, code)

    def set_status(self, text: str) -> None:
        self.query_one("#login-status", Static).update(text)

    @work(exclusive=True)
    async def sign_in(self, username: str, password: str, code: str) -> None:
        try:
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
        self.app.show_main()


AGENT_COLUMNS = [
    ("host", "Hostname"),
    ("status", "Status"),
    ("groups", "Groups"),
    ("last_seen", "Last seen"),
    ("cpu", "CPU"),
    ("ram", "RAM"),
    ("disk", "Disk"),
    ("sessions", "Active sessions"),
]


class MainScreen(Screen):
    """Live table of agents, and the actions on the selected one."""

    app: RmmApp

    BINDINGS = [
        Binding("d", "desktop", "Remote desktop"),
        Binding("s", "shell", "Shell"),
        Binding("r", "scripts", "Scripts"),
        Binding("a", "audit", "Audit log"),
        Binding("f5", "refresh", "Refresh"),
        Binding("l", "logout", "Sign out"),
        Binding("q", "app.quit", "Quit"),
    ]

    #: Actions on the selected agent, and the capability each needs there.
    AGENT_ACTIONS = {"desktop": DESKTOP, "shell": SHELL}

    def __init__(self) -> None:
        super().__init__()
        self.agents: dict[str, Agent] = {}

    def compose(self) -> ComposeResult:
        yield Header()
        user = self.app.session.user
        role = user.role.replace("_", " ") if user else ""
        yield Static(
            f"{user.username if user else ''} ({role}) @ {self.app.config.server_url}",
            id="whoami",
        )
        yield DataTable(id="agents", cursor_type="row", zebra_stripes=True)
        yield Static("Loading agents…", id="status")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "RMM support"
        table = self.query_one("#agents", DataTable)
        for key, label in AGENT_COLUMNS:
            table.add_column(label, key=key)
        table.focus()
        self.refresh_agents()
        self.set_interval(self.app.config.poll_interval, self.refresh_agents)

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
        return True

    @on(DataTable.RowHighlighted, "#agents")
    def selection_moved(self) -> None:
        # What the footer offers depends on the selected agent.
        self.refresh_bindings()

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
        table = self.query_one("#agents", DataTable)
        now = datetime.now(UTC)
        fresh = {a.id: a for a in agents}
        for gone in set(self.agents) - set(fresh):
            table.remove_row(gone)
        new = [a for a in agents if a.id not in self.agents]
        for agent in agents:
            if agent.id in self.agents:
                for (key, _), value in zip(
                    AGENT_COLUMNS, formatting.agent_row(agent, now), strict=True
                ):
                    table.update_cell(agent.id, key, value)
        # Online agents first, then by name.
        for agent in sorted(new, key=lambda a: (not a.online, a.label.lower())):
            table.add_row(*formatting.agent_row(agent, now), key=agent.id)
        self.agents = fresh
        self.refresh_bindings()
        online = sum(a.online for a in agents)
        self.query_one("#status", Static).update(
            f"{len(agents)} agents, {online} online · updated {now.astimezone():%H:%M:%S}"
        )

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
            session = await self.app.session.api.create_viewer_session(agent.id)
            if not session.online:
                self.app.notify(f"{agent.label} went offline.", severity="error")
                return
            command = build_command(self.app.config, session)
            process = self.app.launch_viewer(command)
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

    @work(exclusive=True, group="logout")
    async def action_logout(self) -> None:
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
