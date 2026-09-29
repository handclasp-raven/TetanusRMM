"""Script runner: pick agents and/or agent groups, type or load a script,
fire it through the server, and read each agent's result. No viewer
involved. Only agents the user may run scripts on are offered."""

from __future__ import annotations

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
    OptionList,
    SelectionList,
    Static,
    TextArea,
)
from textual.widgets.option_list import Option

from .api import SCRIPT, Agent, ApiError, Forbidden, Group, Unauthorized
from .scripts import SavedScript, ScriptError, ScriptLibrary, ScriptRun, validate_timeout

if TYPE_CHECKING:
    from .app import RmmApp


class ScriptScreen(Screen):
    app: RmmApp

    BINDINGS = [
        Binding("ctrl+r", "run", "Run", priority=True),
        Binding("escape", "app.pop_screen", "Back"),
    ]

    def __init__(self, agents: list[Agent], preselected: list[str], user=None) -> None:
        super().__init__()
        self.agents = sorted(
            (a for a in agents if a.allows(SCRIPT, user)), key=lambda a: a.label.lower()
        )
        self.labels = {a.id: a.long_label for a in agents}
        self.preselected = set(preselected)
        self.groups: dict[int, Group] = {}
        self.current_run: ScriptRun | None = None
        self.scripts: list[SavedScript] = []

    @property
    def library(self) -> ScriptLibrary:
        return self.app.library

    def compose(self) -> ComposeResult:
        yield Header()
        with Horizontal(id="script-top"):
            with Vertical(id="targets-box"):
                yield Label("Targets (space toggles)")
                yield SelectionList[str](
                    *[
                        (
                            f"{a.long_label} ({'online' if a.online else 'offline'})",
                            a.id,
                            a.id in self.preselected,
                        )
                        for a in self.agents
                    ],
                    id="targets",
                )
                yield Label("Groups (members at run time)")
                yield SelectionList[int](id="groups")
            with Vertical(id="library-box"):
                yield Label("Saved scripts (Enter loads)")
                yield OptionList(id="library")
                with Horizontal(id="library-controls"):
                    yield Input(placeholder="Name to save as", id="save-name")
                    yield Button("Save", id="save")
                    yield Button("Delete", id="delete", variant="error")
        yield TextArea(id="script-body", show_line_numbers=True)
        with Horizontal(id="run-controls"):
            yield Label("Timeout (s)")
            yield Input(placeholder="300", id="timeout", restrict=r"[0-9]*", max_length=4)
            yield Button("Run  ^R", variant="primary", id="run")
            yield Static("", id="run-summary")
        with Horizontal(id="results-box"):
            yield DataTable(id="results", cursor_type="row")
            with VerticalScroll(id="output-box"):
                yield Static("", id="output")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "Script runner"
        results = self.query_one("#results", DataTable)
        results.add_column("Agent", key="agent")
        results.add_column("Status", key="status")
        results.add_column("Exit", key="exit")
        results.add_column("Time", key="time")
        self.reload_library()
        self.load_groups()
        self.query_one("#script-body", TextArea).focus()

    @work(exclusive=True, group="groups")
    async def load_groups(self) -> None:
        try:
            groups = await self.app.session.api.list_groups()
        except ApiError:
            # An older server has no groups; the agent list still works.
            return
        self.groups = {g.id: g for g in groups}
        selection = self.query_one("#groups", SelectionList)
        selection.clear_options()
        selection.add_options([(f"{g.name} ({len(g.agent_ids)} agents)", g.id) for g in groups])

    # --- library ------------------------------------------------------------------

    def reload_library(self) -> None:
        try:
            self.scripts = self.library.load()
        except ScriptError as e:
            self.app.notify(str(e), severity="error")
            self.scripts = []
        options = self.query_one("#library", OptionList)
        options.clear_options()
        options.add_options([Option(s.name, id=s.name) for s in self.scripts])

    @on(OptionList.OptionSelected, "#library")
    def load_script(self, event: OptionList.OptionSelected) -> None:
        script = next((s for s in self.scripts if s.name == event.option.id), None)
        if script is None:
            return
        self.query_one("#script-body", TextArea).text = script.body
        self.query_one("#timeout", Input).value = str(script.timeout_secs or "")
        self.query_one("#save-name", Input).value = script.name

    @on(Button.Pressed, "#save")
    def save_script(self) -> None:
        try:
            script = SavedScript(
                name=self.query_one("#save-name", Input).value,
                body=self.query_one("#script-body", TextArea).text,
                timeout_secs=validate_timeout(self.query_one("#timeout", Input).value),
            )
            self.library.save(script)
        except (ScriptError, OSError) as e:
            self.app.notify(str(e), severity="error")
            return
        self.app.notify(f"Saved {script.name.strip()!r}.")
        self.reload_library()

    @on(Button.Pressed, "#delete")
    def delete_script(self) -> None:
        options = self.query_one("#library", OptionList)
        if options.highlighted is None:
            self.app.notify("Highlight a saved script to delete.", severity="warning")
            return
        name = options.get_option_at_index(options.highlighted).id or ""
        try:
            self.library.delete(name)
        except (ScriptError, OSError) as e:
            self.app.notify(str(e), severity="error")
            return
        self.app.notify(f"Deleted {name!r}.")
        self.reload_library()

    # --- running ------------------------------------------------------------------

    @on(Button.Pressed, "#run")
    def action_run(self) -> None:
        if self.current_run is not None and not self.current_run.done:
            self.app.notify("A run is already in progress.", severity="warning")
            return
        try:
            timeout = validate_timeout(self.query_one("#timeout", Input).value)
            group_ids = list(self.query_one("#groups", SelectionList).selected)
            agent_ids = list(self.query_one("#targets", SelectionList).selected)
            # Show the members we know of as running; the server has the
            # final say on membership.
            for group_id in group_ids:
                agent_ids += self.groups[group_id].agent_ids
            run = ScriptRun(
                agent_ids=agent_ids,
                script=self.query_one("#script-body", TextArea).text,
                group_ids=group_ids,
            )
        except ScriptError as e:
            self.app.notify(str(e), severity="error")
            return
        self.current_run = run
        self.show_run()
        self.fire(run, timeout)

    @work(group="script-run")
    async def fire(self, run: ScriptRun, timeout: int | None) -> None:
        self.query_one("#run", Button).disabled = True
        try:
            explicit = list(self.query_one("#targets", SelectionList).selected)
            report = await self.app.session.api.run_script(
                explicit, run.script, timeout, group_ids=run.group_ids
            )
            run.apply_report(report)
        except Unauthorized:
            run.fail("session expired")
            self.app.session_expired()
            return
        except Forbidden:
            run.fail("you may not run scripts on every one of these agents")
        except ApiError as e:
            run.fail(e.message)
        finally:
            self.query_one("#run", Button).disabled = False
        if run is self.current_run:
            self.show_run()

    def show_run(self) -> None:
        run = self.current_run
        if run is None:
            return
        table = self.query_one("#results", DataTable)
        cursor = table.cursor_row
        table.clear()
        for result in run.ordered():
            status = result.status_label
            if result.succeeded:
                status = f"[green]{status}[/green]"
            elif result.status != "running":
                status = f"[red]{status}[/red]"
            table.add_row(
                self.labels.get(result.agent_id, result.agent_id),
                status,
                result.exit_label,
                result.duration_label,
                key=result.agent_id,
            )
        if table.row_count:
            table.move_cursor(row=min(cursor, table.row_count - 1))
        self.query_one("#run-summary", Static).update(run.summary())
        self.show_output()

    @on(DataTable.RowHighlighted, "#results")
    def show_output(self) -> None:
        output = self.query_one("#output", Static)
        table = self.query_one("#results", DataTable)
        if self.current_run is None or not table.row_count:
            output.update("")
            return
        row_key, _ = table.coordinate_to_cell_key(table.cursor_coordinate)
        result = self.current_run.results.get(str(row_key.value))
        if result is not None:
            # Output is the agent's, not markup: show it literally.
            from rich.text import Text

            output.update(Text(result.output_text()))
