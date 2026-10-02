"""The remote viewer's command buttons: what each is called and what it
starts on the agent (``cmd``, ``ncpa.cpl``, ``mstsc``...).

The list is the technician's own, kept with the TUI's other settings in
``state.json``, and handed to the viewer when it starts (``--command
LABEL=COMMAND``). A button starts its command on the agent's desktop as the
signed-in user, like typing it into the Run dialog.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from textual import on
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import Button, Input, OptionList, Static
from textual.widgets.option_list import Option

if TYPE_CHECKING:
    from .app import RmmApp

#: As the server checks it (``protocol::launch::MAX_COMMAND_LEN``).
MAX_COMMAND_LEN = 1024
#: Longer labels are cut short on the button anyway.
MAX_LABEL_LEN = 40


class CommandError(Exception):
    pass


@dataclass(frozen=True)
class QuickCommand:
    label: str
    command: str

    @classmethod
    def create(cls, label: str, command: str) -> QuickCommand:
        """Checked and trimmed. The label defaults to the command."""
        label, command = label.strip(), command.strip()
        if not command:
            raise CommandError("Enter the command to run, e.g. ncpa.cpl.")
        if len(command.encode()) > MAX_COMMAND_LEN:
            raise CommandError(f"The command is longer than {MAX_COMMAND_LEN} bytes.")
        label = label or command
        if "=" in label:
            raise CommandError("The label cannot contain '='.")
        if len(label) > MAX_LABEL_LEN:
            raise CommandError(f"Keep the label to {MAX_LABEL_LEN} characters.")
        for text in (label, command):
            if any(ord(c) < 32 or ord(c) == 127 for c in text):
                raise CommandError("Use a single line.")
        return cls(label, command)

    @property
    def argument(self) -> str:
        """For the viewer's ``--command``."""
        return f"{self.label}={self.command}"

    def to_json(self) -> dict[str, str]:
        return {"label": self.label, "command": self.command}


#: Offered until the technician changes the list.
DEFAULT_COMMANDS: tuple[QuickCommand, ...] = (
    QuickCommand("Command prompt", "cmd"),
    QuickCommand("Network connections", "ncpa.cpl"),
    QuickCommand("Remote desktop", "mstsc"),
    QuickCommand("Task manager", "taskmgr"),
    QuickCommand("Services", "services.msc"),
    QuickCommand("Event viewer", "eventvwr.msc"),
)


def from_json(raw: Any) -> list[QuickCommand] | None:
    """The list saved in ``state.json``; ``None`` (the defaults) if it is
    missing or not a list. Bad entries are skipped."""
    if not isinstance(raw, list):
        return None
    commands = []
    for entry in raw:
        if not isinstance(entry, dict):
            continue
        try:
            commands.append(
                QuickCommand.create(str(entry.get("label", "")), str(entry.get("command", "")))
            )
        except CommandError:
            continue
    return commands


class CommandsScreen(ModalScreen[list[QuickCommand] | None]):
    """Add, remove and order the viewer's command buttons. Dismissed with
    the new list, or ``None`` if cancelled."""

    app: RmmApp

    BINDINGS = [
        Binding("escape", "cancel", "Cancel"),
        Binding("delete", "remove", "Remove", show=False),
        Binding("shift+up", "move(-1)", "Move up", show=False),
        Binding("shift+down", "move(1)", "Move down", show=False),
    ]

    def __init__(self, commands: list[QuickCommand]) -> None:
        super().__init__()
        self.commands = list(commands)

    def compose(self) -> ComposeResult:
        with Vertical(id="commands-box"):
            yield Static("[b]Viewer command buttons[/b]", id="commands-title")
            yield Static(
                "Each button in the remote viewer's panel starts its command on the "
                "agent's desktop, as the signed-in user.",
                id="commands-intro",
            )
            yield OptionList(id="commands-list")
            with Horizontal(id="commands-edit"):
                yield Input(placeholder="Label (e.g. Network connections)", id="command-label")
                yield Input(placeholder="Command (e.g. ncpa.cpl)", id="command-text")
                yield Button("Add", variant="primary", id="command-add")
            yield Static("Del: remove · Shift+↑/↓: move · Esc: cancel", id="commands-help")
            yield Static("", id="commands-status")
            with Horizontal(id="commands-buttons"):
                yield Button("▲", id="command-up", tooltip="Move up (Shift+↑)")
                yield Button("▼", id="command-down", tooltip="Move down (Shift+↓)")
                yield Button("Remove", variant="error", id="command-remove")
                yield Button("Defaults", id="command-defaults")
                yield Button("Cancel", id="command-cancel")
                yield Button("Save", variant="success", id="command-save")

    def on_mount(self) -> None:
        self.fill(highlight=0)
        self.query_one("#commands-list").focus()

    def fill(self, highlight: int | None) -> None:
        options = self.query_one("#commands-list", OptionList)
        options.clear_options()
        options.add_options(
            [Option(f"{c.label}  →  {c.command}") for c in self.commands]
            or [Option("(no buttons)", disabled=True)]
        )
        if self.commands and highlight is not None:
            options.highlighted = max(0, min(highlight, len(self.commands) - 1))

    def set_status(self, text: str) -> None:
        self.query_one("#commands-status", Static).update(text)

    def highlighted(self) -> int | None:
        index = self.query_one("#commands-list", OptionList).highlighted
        if index is None or not 0 <= index < len(self.commands):
            return None
        return index

    @on(Button.Pressed, "#command-add")
    @on(Input.Submitted, "#command-text")
    def add(self) -> None:
        label = self.query_one("#command-label", Input)
        command = self.query_one("#command-text", Input)
        try:
            new = QuickCommand.create(label.value, command.value)
        except CommandError as e:
            self.set_status(f"[red]{e}[/red]")
            return
        if any(c.label == new.label for c in self.commands):
            self.set_status(f"[red]There is already a button called {new.label!r}.[/red]")
            return
        self.commands.append(new)
        label.value = command.value = ""
        self.set_status(f"Added {new.label}.")
        self.fill(highlight=len(self.commands) - 1)
        label.focus()

    @on(Input.Submitted, "#command-label")
    def next_field(self) -> None:
        self.query_one("#command-text", Input).focus()

    @on(Button.Pressed, "#command-remove")
    def action_remove(self) -> None:
        index = self.highlighted()
        if index is None:
            return
        removed = self.commands.pop(index)
        self.set_status(f"Removed {removed.label}.")
        self.fill(highlight=index)

    def action_move(self, step: int) -> None:
        index = self.highlighted()
        if index is None or not 0 <= index + step < len(self.commands):
            return
        c = self.commands
        c[index], c[index + step] = c[index + step], c[index]
        self.fill(highlight=index + step)

    @on(Button.Pressed, "#command-up")
    def up(self) -> None:
        self.action_move(-1)

    @on(Button.Pressed, "#command-down")
    def down(self) -> None:
        self.action_move(1)

    @on(Button.Pressed, "#command-defaults")
    def defaults(self) -> None:
        self.commands = list(DEFAULT_COMMANDS)
        self.set_status("Back to the default buttons (not saved yet).")
        self.fill(highlight=0)

    @on(Button.Pressed, "#command-save")
    def save(self) -> None:
        self.dismiss(self.commands)

    @on(Button.Pressed, "#command-cancel")
    def action_cancel(self) -> None:
        self.dismiss(None)
