"""User accounts, for admins only (the server enforces the same rule): add
and remove users, change their role, say which agents a support engineer may
work on, and set their password or TOTP secret."""

from __future__ import annotations

from typing import TYPE_CHECKING

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen, Screen
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

from .api import (
    CAPABILITIES,
    MIN_PASSWORD_LEN,
    ROLES,
    Agent,
    ApiError,
    Grant,
    Group,
    TotpEnrollment,
    Unauthorized,
    User,
)
from .groups import ConfirmScreen

if TYPE_CHECKING:
    from .app import RmmApp

#: What each role may do, beside its name in the role lists.
ROLE_HELP = {
    "admin": "everything, on every agent",
    "support_engineer": "remote control, where granted access",
    "auditor": "read only, plus the audit log",
}


def role_label(role: str) -> str:
    return role.replace("_", " ").capitalize()


def capability_label(capability: str) -> str:
    return capability.replace("_", " ")


def password_problem(password: str, repeat: str) -> str | None:
    """Why this password cannot be used, if it cannot."""
    if len(password) < MIN_PASSWORD_LEN:
        return f"A password needs at least {MIN_PASSWORD_LEN} characters."
    if password != repeat:
        return "The passwords do not match."
    return None


def role_options() -> list[Option]:
    return [Option(f"{role_label(r)}: {ROLE_HELP[r]}", id=r) for r in ROLES]


class NewUserScreen(ModalScreen[tuple[str, str, str] | None]):
    """Username, password and role for a new user."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="new-user-box"):
            yield Static("[b]New user[/b]")
            yield Input(placeholder="Username", id="nu-username")
            yield Input(placeholder="Password", password=True, id="nu-password")
            yield Input(placeholder="Password again", password=True, id="nu-repeat")
            yield OptionList(*role_options(), id="nu-role")
            yield Static("", id="nu-status")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="nu-cancel")
                yield Button("Create", variant="primary", id="nu-save")

    def on_mount(self) -> None:
        roles = self.query_one("#nu-role", OptionList)
        roles.highlighted = roles.get_option_index("support_engineer")
        self.query_one("#nu-username").focus()

    @on(Input.Submitted)
    @on(Button.Pressed, "#nu-save")
    def save(self) -> None:
        username = self.query_one("#nu-username", Input).value.strip()
        password = self.query_one("#nu-password", Input).value
        roles = self.query_one("#nu-role", OptionList)
        problem = (
            "A user needs a username."
            if not username
            else password_problem(password, self.query_one("#nu-repeat", Input).value)
        )
        if problem or roles.highlighted is None:
            self.query_one("#nu-status", Static).update(f"[red]{problem}[/red]")
            return
        role = roles.get_option_at_index(roles.highlighted).id
        self.dismiss((username, password, str(role)))

    @on(Button.Pressed, "#nu-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)


class RoleScreen(ModalScreen[str | None]):
    """Pick a user's role."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, user: User) -> None:
        super().__init__()
        self.user = user

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="role-box"):
            yield Static(f"[b]Role of {self.user.username}[/b]")
            yield OptionList(*role_options(), id="role-list")
            yield Static("Enter: choose · Esc: cancel", classes="dialog-help")

    def on_mount(self) -> None:
        roles = self.query_one("#role-list", OptionList)
        if self.user.role in ROLES:
            roles.highlighted = roles.get_option_index(self.user.role)
        roles.focus()

    @on(OptionList.OptionSelected, "#role-list")
    def chosen(self, event: OptionList.OptionSelected) -> None:
        self.dismiss(event.option.id)

    def action_cancel(self) -> None:
        self.dismiss(None)


class PasswordScreen(ModalScreen[str | None]):
    """A new password for a user."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, user: User, own: bool) -> None:
        super().__init__()
        self.user = user
        self.own = own

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="password-box"):
            yield Static(f"[b]New password for {self.user.username}[/b]")
            yield Input(placeholder="Password", password=True, id="pw-password")
            yield Input(placeholder="Password again", password=True, id="pw-repeat")
            yield Static(
                "Your other sessions are signed out."
                if self.own
                else f"{self.user.username} is signed out everywhere.",
                classes="dialog-help",
            )
            yield Static("", id="pw-status")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="pw-cancel")
                yield Button("Set password", variant="primary", id="pw-save")

    def on_mount(self) -> None:
        self.query_one("#pw-password").focus()

    @on(Input.Submitted)
    @on(Button.Pressed, "#pw-save")
    def save(self) -> None:
        password = self.query_one("#pw-password", Input).value
        problem = password_problem(password, self.query_one("#pw-repeat", Input).value)
        if problem:
            self.query_one("#pw-status", Static).update(f"[red]{problem}[/red]")
            return
        self.dismiss(password)

    @on(Button.Pressed, "#pw-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)


class TotpScreen(ModalScreen[None]):
    """A user's new TOTP secret. The server does not show it again."""

    BINDINGS = [Binding("escape", "close", "Close")]

    def __init__(self, enrollment: TotpEnrollment) -> None:
        super().__init__()
        self.enrollment = enrollment

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="totp-box"):
            yield Static(f"[b]Authenticator setup for {self.enrollment.user.username}[/b]")
            yield Static(
                "Give this to the user for their authenticator app. It is not shown again: "
                "if it is lost, reset their TOTP.",
                classes="dialog-help",
            )
            yield Static("TOTP secret", classes="dialog-help")
            with Horizontal(classes="na-url-row"):
                yield Input(self.enrollment.totp_secret, id="totp-secret", classes="na-url")
                yield Button("Copy", id="totp-copy-secret")
            yield Static("otpauth URL (for a QR code)", classes="dialog-help")
            with Horizontal(classes="na-url-row"):
                yield Input(self.enrollment.otpauth_url, id="totp-url", classes="na-url")
                yield Button("Copy", id="totp-copy-url")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Close", variant="primary", id="totp-close")

    def on_mount(self) -> None:
        self.query_one("#totp-close").focus()

    @on(Button.Pressed, "#totp-copy-secret")
    def copy_secret(self) -> None:
        self.app.copy_to_clipboard(self.enrollment.totp_secret)
        self.app.notify("TOTP secret copied.")

    @on(Button.Pressed, "#totp-copy-url")
    def copy_url(self) -> None:
        self.app.copy_to_clipboard(self.enrollment.otpauth_url)
        self.app.notify("otpauth URL copied.")

    @on(Button.Pressed, "#totp-close")
    def action_close(self) -> None:
        self.dismiss(None)


#: A new grant: capabilities, then the agent or group it covers (neither:
#: every agent).
NewGrant = tuple[list[str], str | None, int | None]


class GrantEditScreen(ModalScreen[NewGrant | None]):
    """Where a new grant applies, and what it allows there."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    ALL_AGENTS = "all"

    def __init__(self, user: User, agents: list[Agent], groups: list[Group]) -> None:
        super().__init__()
        self.user = user
        self.agents = sorted(agents, key=lambda a: a.label.lower())
        self.groups = sorted(groups, key=lambda g: g.name.lower())

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="grant-edit-box"):
            yield Static(f"[b]Grant {self.user.username} access to[/b]")
            yield OptionList(
                Option("All agents (also those enrolled later)", id=self.ALL_AGENTS),
                *[Option(f"Group: {g.name}", id=f"group:{g.id}") for g in self.groups],
                *[Option(f"Agent: {a.long_label}", id=f"agent:{a.id}") for a in self.agents],
                id="gr-scope",
            )
            yield SelectionList[str](
                *[(capability_label(c).capitalize(), c, True) for c in CAPABILITIES],
                id="gr-capabilities",
            )
            yield Static("", id="gr-status")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="gr-cancel")
                yield Button("Grant", variant="primary", id="gr-save")

    def on_mount(self) -> None:
        scope = self.query_one("#gr-scope", OptionList)
        scope.highlighted = 0
        scope.focus()

    @on(Button.Pressed, "#gr-save")
    def save(self) -> None:
        scope = self.query_one("#gr-scope", OptionList)
        capabilities = list(self.query_one("#gr-capabilities", SelectionList).selected)
        if not capabilities or scope.highlighted is None:
            self.query_one("#gr-status", Static).update("[red]Tick at least one capability.[/red]")
            return
        kind, _, value = str(scope.get_option_at_index(scope.highlighted).id).partition(":")
        self.dismiss(
            (
                capabilities,
                value if kind == "agent" else None,
                int(value) if kind == "group" else None,
            )
        )

    @on(Button.Pressed, "#gr-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)


class GrantsScreen(ModalScreen[None]):
    """A support engineer's grants: add and remove them."""

    app: RmmApp

    BINDINGS = [
        Binding("escape", "close", "Close"),
        Binding("n", "add", "Add"),
        Binding("delete", "remove", "Remove"),
    ]

    def __init__(self, user: User, agents: dict[str, Agent], groups: list[Group]) -> None:
        super().__init__()
        self.user = user
        self.agents = agents
        self.groups = groups
        self.grants: dict[int, Grant] = {}

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="grants-box"):
            yield Static(f"[b]Access of {self.user.username}[/b]")
            yield OptionList(id="grants-list")
            yield Static("Loading…", id="grants-status", classes="dialog-help")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Add (n)", id="grants-add")
                yield Button("Remove (Del)", variant="error", id="grants-remove")
                yield Button("Close", variant="primary", id="grants-close")

    def on_mount(self) -> None:
        self.query_one("#grants-list").focus()
        self.reload()

    def describe(self, grant: Grant) -> str:
        return (
            f"{scope_label(grant, self.agents)}: "
            f"{', '.join(capability_label(c) for c in grant.capabilities)}"
        )

    @work(group="grants")
    async def reload(self) -> None:
        await self._reload()

    async def _reload(self) -> None:
        try:
            grants = await self.app.session.api.list_grants(self.user.id)
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.query_one("#grants-status", Static).update(f"[red]{e.message}[/red]")
            return
        self.grants = {g.id: g for g in grants}
        options = self.query_one("#grants-list", OptionList)
        options.clear_options()
        options.add_options([Option(self.describe(g), id=str(g.id)) for g in grants])
        if grants:
            options.highlighted = 0
        self.query_one("#grants-status", Static).update(
            "" if grants else "No access yet: this user sees no agents."
        )

    @on(Button.Pressed, "#grants-add")
    def action_add(self) -> None:
        def done(result: NewGrant | None) -> None:
            if result is not None:
                capabilities, agent_id, group_id = result
                self.change(
                    self.app.session.api.create_grant(
                        self.user.id, capabilities, agent_id=agent_id, group_id=group_id
                    )
                )

        self.app.push_screen(
            GrantEditScreen(self.user, list(self.agents.values()), self.groups), done
        )

    @on(Button.Pressed, "#grants-remove")
    def action_remove(self) -> None:
        options = self.query_one("#grants-list", OptionList)
        if options.highlighted is None:
            return
        grant_id = int(str(options.get_option_at_index(options.highlighted).id))
        self.change(self.app.session.api.delete_grant(grant_id))

    @work(group="grants")
    async def change(self, request) -> None:  # noqa: ANN001
        try:
            await request
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        await self._reload()

    @on(Button.Pressed, "#grants-close")
    def action_close(self) -> None:
        self.dismiss(None)


def scope_label(grant: Grant, agents: dict[str, Agent]) -> str:
    """Where a grant applies."""
    if grant.all_agents:
        return "All agents"
    if grant.group_id is not None:
        return f"Group {grant.group_name or grant.group_id}"
    agent = agents.get(grant.agent_id or "")
    return f"Agent {agent.long_label if agent else grant.agent_id}"


class UsersScreen(Screen):
    """The server's users, and the changes an admin can make to them."""

    app: RmmApp

    BINDINGS = [
        Binding("escape", "back", "Back"),
        Binding("n", "new", "New user"),
        Binding("r", "role", "Role"),
        Binding("a", "access", "Access"),
        Binding("p", "password", "Password"),
        Binding("t", "totp", "Reset TOTP"),
        Binding("delete", "delete", "Delete"),
        Binding("f5", "refresh", "Refresh"),
    ]

    #: Actions on the selected user.
    USER_ACTIONS = {"role", "access", "password", "totp", "delete"}

    def __init__(self, agents: list[Agent]) -> None:
        super().__init__()
        self.agents = {a.id: a for a in agents}
        self.users: dict[int, User] = {}
        self.grants: list[Grant] = []
        self.groups: list[Group] = []

    def compose(self) -> ComposeResult:
        yield Header()
        yield DataTable(id="users-table", cursor_type="row", zebra_stripes=True)
        yield Static("", id="users-status")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "Users"
        table = self.query_one("#users-table", DataTable)
        table.add_column("Username", key="username")
        table.add_column("Role", key="role")
        table.add_column("Access", key="access")
        table.focus()
        self.action_refresh()

    def check_action(self, action: str, parameters: tuple[object, ...]) -> bool | None:
        if action in self.USER_ACTIONS:
            user = self.selected()
            if user is None:
                return None  # shown, greyed out
            if action == "access" and user.role != "support_engineer":
                return None  # grants are for support engineers
            if action == "delete" and self.is_me(user):
                return None  # the server refuses it too
        return True

    def is_me(self, user: User) -> bool:
        me = self.app.session.user
        return bool(me and me.id == user.id)

    def set_status(self, text: str) -> None:
        self.query_one("#users-status", Static).update(text)

    def action_back(self) -> None:
        self.app.pop_screen()

    @work(exclusive=True, group="users")
    async def action_refresh(self) -> None:
        await self._reload()

    async def _reload(self, select: int | None = None) -> None:
        api = self.app.session.api
        try:
            users = await api.list_users()
            self.grants = await api.list_grants()
            self.groups = await api.list_groups()
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.set_status(f"[red]{e.message}[/red]")
            return
        self.set_status(f"{len(users)} user(s)")
        self.show_users(users, select)

    def access(self, user: User) -> str:
        """What the user can reach, for the table."""
        if user.role == "admin":
            return "Everything"
        if user.role == "auditor":
            return "Read only"
        grants = [scope_label(g, self.agents) for g in self.grants if g.user_id == user.id]
        return ", ".join(sorted(grants)) or "Nothing yet"

    def show_users(self, users: list[User], select: int | None = None) -> None:
        table = self.query_one("#users-table", DataTable)
        current = self.selected()
        select = select if select is not None else (current.id if current else None)
        self.users = {u.id: u for u in users}
        table.clear()
        for user in sorted(users, key=lambda u: u.username.lower()):
            name = f"{user.username} (you)" if self.is_me(user) else user.username
            table.add_row(name, role_label(user.role), self.access(user), key=str(user.id))
        if select is not None and select in self.users:
            table.move_cursor(row=table.get_row_index(str(select)))
        self.refresh_bindings()

    def selected(self) -> User | None:
        table = self.query_one("#users-table", DataTable)
        if not table.row_count:
            return None
        row_key, _ = table.coordinate_to_cell_key(table.cursor_coordinate)
        return self.users.get(int(str(row_key.value)))

    @on(DataTable.RowHighlighted, "#users-table")
    def row_changed(self) -> None:
        self.refresh_bindings()

    # --- changes ------------------------------------------------------------

    def action_new(self) -> None:
        def done(result: tuple[str, str, str] | None) -> None:
            if result is not None:
                self.run_enrollment(self.app.session.api.create_user(*result), "Created")

        self.app.push_screen(NewUserScreen(), done)

    def action_role(self) -> None:
        user = self.selected()
        if user is None:
            return

        def done(role: str | None) -> None:
            if role is not None and role != user.role:
                self.run_role(user, role)

        self.app.push_screen(RoleScreen(user), done)

    def action_access(self) -> None:
        user = self.selected()
        if user is None or user.role != "support_engineer":
            return

        def closed(_: None) -> None:
            self.action_refresh()

        self.app.push_screen(GrantsScreen(user, self.agents, self.groups), closed)

    def action_password(self) -> None:
        user = self.selected()
        if user is None:
            return

        def done(password: str | None) -> None:
            if password is not None:
                self.run_password(user, password)

        self.app.push_screen(PasswordScreen(user, self.is_me(user)), done)

    def action_totp(self) -> None:
        user = self.selected()
        if user is None:
            return

        def done(confirmed: bool | None) -> None:
            if confirmed:
                self.run_enrollment(self.app.session.api.reset_user_totp(user.id), "Reset TOTP of")

        self.app.push_screen(
            ConfirmScreen(
                f"Reset the TOTP secret of [b]{user.username}[/b]? Their authenticator "
                "app stops working until it has the new secret, and they are signed out.",
                confirm="Reset",
            ),
            done,
        )

    def action_delete(self) -> None:
        user = self.selected()
        if user is None or self.is_me(user):
            return

        def done(confirmed: bool | None) -> None:
            if confirmed:
                self.run_delete(user)

        self.app.push_screen(
            ConfirmScreen(
                f"Delete user [b]{user.username}[/b]? They are signed out and lose their "
                "access; the audit log keeps their name."
            ),
            done,
        )

    async def _attempt(self, request):  # noqa: ANN001, ANN202
        """The request's result, or ``None`` once its failure has been shown."""
        try:
            return await request
        except Unauthorized:
            self.app.session_expired()
        except ApiError as e:
            self.app.notify(e.message, severity="error")
        return None

    @work(group="change")
    async def run_enrollment(self, request, verb: str) -> None:  # noqa: ANN001
        enrollment = await self._attempt(request)
        if enrollment is None:
            return
        self.app.notify(f"{verb} {enrollment.user.username!r}.")
        self.app.push_screen(TotpScreen(enrollment))
        await self._reload(select=enrollment.user.id)

    @work(group="change")
    async def run_role(self, user: User, role: str) -> None:
        changed = await self._attempt(self.app.session.api.set_user_role(user.id, role))
        if changed is None:
            return
        self.app.notify(f"{changed.username} is now {role_label(changed.role).lower()}.")
        if self.is_me(changed):
            self.app.session.user = changed
            if not changed.is_admin:
                # No longer allowed here: back to the agent list, as it now is.
                self.app.show_main()
                return
        await self._reload(select=changed.id)

    @work(group="change")
    async def run_password(self, user: User, password: str) -> None:
        changed = await self._attempt(self.app.session.api.set_user_password(user.id, password))
        if changed is not None:
            self.app.notify(f"Password of {changed.username!r} set.")

    @work(group="change")
    async def run_delete(self, user: User) -> None:
        # ``None`` is also what a successful delete returns.
        try:
            await self.app.session.api.delete_user(user.id)
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        self.app.notify(f"Deleted {user.username!r}.")
        await self._reload()
