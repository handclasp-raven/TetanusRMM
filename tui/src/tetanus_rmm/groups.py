"""Agent groups: everyone may look; admins create, rename, delete and set
members (the server enforces the same rule)."""

from __future__ import annotations

from typing import TYPE_CHECKING

from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen, Screen
from textual.widgets import (
    Button,
    DataTable,
    Footer,
    Header,
    Input,
    Label,
    SelectionList,
    Static,
)

from .api import Agent, ApiError, Group, Unauthorized

if TYPE_CHECKING:
    from .app import RmmApp


class ConfirmScreen(ModalScreen[bool]):
    """Yes or no."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, question: str, confirm: str = "Delete") -> None:
        super().__init__()
        self.question = question
        self.confirm = confirm

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="confirm-box"):
            yield Static(self.question, id="confirm-question")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="confirm-cancel")
                yield Button(self.confirm, variant="error", id="confirm-ok")

    def on_mount(self) -> None:
        self.query_one("#confirm-cancel").focus()

    @on(Button.Pressed, "#confirm-ok")
    def ok(self) -> None:
        self.dismiss(True)

    @on(Button.Pressed, "#confirm-cancel")
    def action_cancel(self) -> None:
        self.dismiss(False)


class GroupEditScreen(ModalScreen[tuple[str, str] | None]):
    """Name and description, for a new group or an existing one."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, group: Group | None = None) -> None:
        super().__init__()
        self.group = group

    def compose(self) -> ComposeResult:
        with Vertical(classes="dialog", id="group-edit-box"):
            title = "New group" if self.group is None else f"Edit {self.group.name}"
            yield Static(f"[b]{title}[/b]")
            yield Input(self.group.name if self.group else "", placeholder="Name", id="ge-name")
            yield Input(
                self.group.description if self.group else "",
                placeholder="Description (optional)",
                id="ge-description",
            )
            yield Static("", id="ge-status")
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="ge-cancel")
                yield Button("Save", variant="primary", id="ge-save")

    def on_mount(self) -> None:
        self.query_one("#ge-name").focus()

    @on(Input.Submitted)
    @on(Button.Pressed, "#ge-save")
    def save(self) -> None:
        name = self.query_one("#ge-name", Input).value.strip()
        if not name:
            self.query_one("#ge-status", Static).update("[red]A group needs a name.[/red]")
            return
        self.dismiss((name, self.query_one("#ge-description", Input).value.strip()))

    @on(Button.Pressed, "#ge-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)


class MembersScreen(ModalScreen[list[str] | None]):
    """Tick the group's members."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, group: Group, agents: list[Agent]) -> None:
        super().__init__()
        self.group = group
        self.agents = sorted(agents, key=lambda a: a.label.lower())

    def compose(self) -> ComposeResult:
        members = set(self.group.agent_ids)
        with Vertical(classes="dialog", id="members-box"):
            yield Static(f"[b]Members of {self.group.name}[/b] (Space toggles)")
            yield SelectionList[str](
                *[
                    (
                        f"{a.long_label} ({'online' if a.online else 'offline'})",
                        a.id,
                        a.id in members,
                    )
                    for a in self.agents
                ],
                id="members-list",
            )
            with Horizontal(classes="dialog-buttons"):
                yield Button("Cancel", id="members-cancel")
                yield Button("Save", variant="primary", id="members-save")

    def on_mount(self) -> None:
        self.query_one("#members-list").focus()

    @on(Button.Pressed, "#members-save")
    def save(self) -> None:
        self.dismiss(list(self.query_one("#members-list", SelectionList).selected))

    @on(Button.Pressed, "#members-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)


class GroupsScreen(Screen):
    """The groups, and who is in them."""

    app: RmmApp

    BINDINGS = [
        Binding("escape", "back", "Back"),
        Binding("n", "new", "New group"),
        Binding("e", "edit", "Rename"),
        Binding("m", "members", "Members"),
        Binding("delete", "delete", "Delete"),
        Binding("f5", "refresh", "Refresh"),
    ]

    ADMIN_ACTIONS = {"new", "edit", "members", "delete"}

    def __init__(self, agents: list[Agent]) -> None:
        super().__init__()
        self.agents = {a.id: a for a in agents}
        self.groups: dict[int, Group] = {}

    def compose(self) -> ComposeResult:
        yield Header()
        yield DataTable(id="groups-table", cursor_type="row", zebra_stripes=True)
        with VerticalScroll(id="group-detail"):
            yield Label("", id="group-detail-title")
            yield Static("", id="group-members")
        yield Static("", id="groups-status")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "Groups"
        table = self.query_one("#groups-table", DataTable)
        table.add_column("Name", key="name")
        table.add_column("Description", key="description")
        table.add_column("Agents", key="count")
        table.add_column("Online", key="online")
        table.focus()
        user = self.app.session.user
        if not (user and user.is_admin):
            self.set_status("Read only: only admins can change groups.")
        self.action_refresh()

    def check_action(self, action: str, parameters: tuple[object, ...]) -> bool | None:
        if action in self.ADMIN_ACTIONS:
            user = self.app.session.user
            if not (user and user.is_admin):
                return False
            if action != "new" and self.selected() is None:
                return None  # shown, greyed out
        return True

    def set_status(self, text: str) -> None:
        self.query_one("#groups-status", Static).update(text)

    def action_back(self) -> None:
        # Dismissed (not just popped) so the agent list hears about it.
        self.dismiss(None)

    @work(exclusive=True, group="groups")
    async def action_refresh(self) -> None:
        try:
            groups = await self.app.session.api.list_groups()
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.set_status(f"[red]{e.message}[/red]")
            return
        self.show_groups(groups)

    def show_groups(self, groups: list[Group], select: int | None = None) -> None:
        table = self.query_one("#groups-table", DataTable)
        current = self.selected()
        select = select if select is not None else (current.id if current else None)
        self.groups = {g.id: g for g in groups}
        table.clear()
        for group in sorted(groups, key=lambda g: g.name.lower()):
            online = sum(
                1 for a in group.agent_ids if (agent := self.agents.get(a)) and agent.online
            )
            table.add_row(
                group.name,
                group.description or "–",
                str(len(group.agent_ids)),
                str(online),
                key=str(group.id),
            )
        if select is not None and select in self.groups:
            table.move_cursor(row=table.get_row_index(str(select)))
        self.show_detail()
        self.refresh_bindings()

    def selected(self) -> Group | None:
        table = self.query_one("#groups-table", DataTable)
        if not table.row_count:
            return None
        row_key, _ = table.coordinate_to_cell_key(table.cursor_coordinate)
        return self.groups.get(int(str(row_key.value)))

    @on(DataTable.RowHighlighted, "#groups-table")
    def show_detail(self) -> None:
        group = self.selected()
        title = self.query_one("#group-detail-title", Label)
        members = self.query_one("#group-members", Static)
        if group is None:
            title.update("No groups yet." if not self.groups else "")
            members.update("")
            return
        title.update(f"[b]{group.name}[/b]: {len(group.agent_ids)} agent(s)")
        names = sorted(
            (
                (self.agents[a].long_label, self.agents[a].online)
                if a in self.agents
                else (a, False)
                for a in group.agent_ids
            ),
            key=lambda item: item[0].lower(),
        )
        members.update(
            "\n".join(f"[{'green' if online else 'red'}]●[/] {name}" for name, online in names)
            or "(no members)"
        )
        self.refresh_bindings()

    # --- admin actions ------------------------------------------------------

    def action_new(self) -> None:
        def done(result: tuple[str, str] | None) -> None:
            if result is not None:
                self.run_change(self.app.session.api.create_group(*result), "Created")

        self.app.push_screen(GroupEditScreen(), done)

    def action_edit(self) -> None:
        group = self.selected()
        if group is None:
            return

        def done(result: tuple[str, str] | None) -> None:
            if result is not None:
                self.run_change(self.app.session.api.update_group(group.id, *result), "Saved")

        self.app.push_screen(GroupEditScreen(group), done)

    def action_members(self) -> None:
        group = self.selected()
        if group is None:
            return

        def done(agent_ids: list[str] | None) -> None:
            if agent_ids is not None:
                self.run_change(
                    self.app.session.api.set_group_members(group.id, agent_ids), "Updated"
                )

        self.app.push_screen(MembersScreen(group, list(self.agents.values())), done)

    def action_delete(self) -> None:
        group = self.selected()
        if group is None:
            return

        def done(confirmed: bool | None) -> None:
            if confirmed:
                self.run_delete(group)

        self.app.push_screen(
            ConfirmScreen(
                f"Delete group [b]{group.name}[/b]? Its {len(group.agent_ids)} agent(s) "
                "stay, but lose any access granted through it."
            ),
            done,
        )

    @work(group="change")
    async def run_change(self, request, verb: str) -> None:  # noqa: ANN001
        try:
            group = await request
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        self.app.notify(f"{verb} {group.name!r}.")
        await self._reload(select=group.id)

    @work(group="change")
    async def run_delete(self, group: Group) -> None:
        try:
            await self.app.session.api.delete_group(group.id)
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.app.notify(e.message, severity="error")
            return
        self.app.notify(f"Deleted {group.name!r}.")
        await self._reload()

    async def _reload(self, select: int | None = None) -> None:
        try:
            groups = await self.app.session.api.list_groups()
        except ApiError as e:
            self.set_status(f"[red]{e.message}[/red]")
            return
        self.show_groups(groups, select)
