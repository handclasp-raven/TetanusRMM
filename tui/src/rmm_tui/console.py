"""Live command console: PowerShell on the agent, through the server's shell
WebSocket, rendered in a pane. No viewer involved."""

from __future__ import annotations

from typing import TYPE_CHECKING

from rich.text import Text
from textual import on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import VerticalScroll
from textual.events import Resize
from textual.screen import Screen
from textual.widgets import Footer, Header, Input, Static

from .api import Agent, ApiError, ShellConnection, Unauthorized
from .shell import (
    INTERRUPT,
    Exited,
    Failed,
    Output,
    ShellProtocolError,
    Started,
    Terminal,
    clamp_size,
    encode_line,
    parse_message,
    resize_message,
)

if TYPE_CHECKING:
    from .app import RmmApp

#: Redraws per second at most, however fast output arrives.
FRAME_RATE = 20
#: Wait for the pane to stop changing size before telling the agent.
RESIZE_DEBOUNCE = 0.25


class TerminalView(VerticalScroll):
    """Scrollable pane showing the terminal's scrollback and screen."""

    def compose(self) -> ComposeResult:
        yield Static(id="terminal-content")

    def cells(self) -> tuple[int, int]:
        """Columns and rows that fit, for the PTY size."""
        region = self.scrollable_content_region
        return clamp_size(region.width, region.height)

    def show(self, lines: list[Text]) -> None:
        at_bottom = self.scroll_offset.y >= self.max_scroll_y - 1
        self.query_one("#terminal-content", Static).update(Text("\n").join(lines))
        if at_bottom:
            self.call_after_refresh(self.scroll_end, animate=False)


class ConsoleScreen(Screen):
    app: RmmApp

    BINDINGS = [
        Binding("ctrl+c", "interrupt", "Send Ctrl-C", priority=True),
        Binding("escape", "close", "Close"),
        Binding("up", "history(-1)", "Previous", show=False),
        Binding("down", "history(1)", "Next", show=False),
    ]

    def __init__(self, agent: Agent) -> None:
        super().__init__()
        self.agent = agent
        self.terminal: Terminal | None = None
        self.connection: ShellConnection | None = None
        self.history: list[str] = []
        self.history_index = 0
        self._dirty = False
        self._resize_timer = None

    def compose(self) -> ComposeResult:
        yield Header()
        yield Static(f"Starting PowerShell on {self.agent.label}…", id="shell-status")
        yield TerminalView(id="terminal")
        yield Input(
            placeholder="Command · Enter to send · Ctrl+C interrupt · Esc close",
            id="shell-input",
            disabled=True,
        )
        yield Footer()

    def on_mount(self) -> None:
        self.title = f"Shell · {self.agent.label}"
        self.set_interval(1 / FRAME_RATE, self.redraw)
        # Size the PTY from the pane once it has been laid out.
        self.call_after_refresh(self.run_shell)

    def set_status(self, text: str) -> None:
        self.query_one("#shell-status", Static).update(text)

    @property
    def input(self) -> Input:
        return self.query_one("#shell-input", Input)

    # --- the session ------------------------------------------------------------

    @work(exclusive=True, group="shell")
    async def run_shell(self) -> None:
        view = self.query_one(TerminalView)
        cols, rows = view.cells()
        self.terminal = Terminal(cols, rows)
        try:
            self.connection = await self.app.session.api.open_shell(self.agent.id, cols, rows)
        except Unauthorized:
            self.app.session_expired()
            return
        except ApiError as e:
            self.set_status(f"[red]Could not start the shell:[/red] {e.message}")
            return
        try:
            async for message in self.connection.messages():
                try:
                    event = parse_message(message)
                except ShellProtocolError as e:
                    self.set_status(f"[red]{e}[/red]")
                    break
                match event:
                    case Started():
                        self.set_status(
                            f"Connected to {self.agent.label} ({self.agent.id}) · {cols}×{rows}"
                        )
                        self.input.disabled = False
                        self.input.focus()
                    case Output(data):
                        self.terminal.feed(data)
                        self._dirty = True
                    case Exited(code):
                        self.set_status(f"Shell exited with code {code}. Esc to close.")
                        break
                    case Failed(message):
                        self.set_status(f"[red]Shell failed:[/red] {message}")
                        break
            else:
                self.set_status("[red]The connection closed.[/red] Esc to close.")
        finally:
            connection, self.connection = self.connection, None
            # Closing the screen with Esc cancels this worker after the
            # widgets are gone, so only touch them while still attached.
            if self.is_attached:
                self.input.disabled = True
                self._dirty = True
            await connection.close()

    def redraw(self) -> None:
        if self._dirty and self.terminal is not None:
            self._dirty = False
            self.query_one(TerminalView).show(
                self.terminal.render_lines(show_cursor=self.connection is not None)
            )

    async def send(self, data: bytes) -> None:
        if self.connection is not None:
            try:
                await self.connection.send_input(data)
            except Exception as e:  # the receive loop reports the close
                self.set_status(f"[red]Send failed:[/red] {e}")

    # --- input ------------------------------------------------------------------

    @on(Input.Submitted, "#shell-input")
    async def submit(self, event: Input.Submitted) -> None:
        line = event.value
        if line.strip() and (not self.history or self.history[-1] != line):
            self.history.append(line)
        self.history_index = len(self.history)
        event.input.value = ""
        await self.send(encode_line(line))

    async def action_interrupt(self) -> None:
        await self.send(INTERRUPT)

    def action_history(self, step: int) -> None:
        if not self.history:
            return
        self.history_index = max(0, min(len(self.history), self.history_index + step))
        value = self.history[self.history_index] if self.history_index < len(self.history) else ""
        self.input.value = value
        self.input.cursor_position = len(value)

    def action_close(self) -> None:
        self.app.pop_screen()

    # --- resize -----------------------------------------------------------------

    @on(Resize)
    def pane_resized(self) -> None:
        if self._resize_timer is not None:
            self._resize_timer.stop()
        self._resize_timer = self.set_timer(RESIZE_DEBOUNCE, self.apply_resize)

    async def apply_resize(self) -> None:
        if self.terminal is None:
            return
        cols, rows = self.query_one(TerminalView).cells()
        if (cols, rows) == self.terminal.size:
            return
        self.terminal.resize(cols, rows)
        self._dirty = True
        if self.connection is not None:
            try:
                await self.connection.send_control(resize_message(cols, rows))
            except Exception:
                return
            self.set_status(f"Connected to {self.agent.label} ({self.agent.id}) · {cols}×{rows}")
