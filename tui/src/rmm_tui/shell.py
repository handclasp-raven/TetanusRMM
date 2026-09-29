"""The interactive shell's WebSocket protocol (``protocol::shell`` on the
server) and a terminal emulator to render its output.

- Binary messages are raw terminal bytes both ways: keystrokes in, VT output
  out.
- Text messages are JSON control messages. From the client:
  ``{"type":"resize","cols":C,"rows":R}``. From the server:
  ``{"type":"started"}`` once, then ``{"type":"exit","code":N}`` or
  ``{"type":"error","message":M}`` as the last message.
"""

from __future__ import annotations

import json
from dataclasses import dataclass

import pyte
from rich.style import Style
from rich.text import Text

#: Largest terminal dimension the server accepts.
MAX_DIMENSION = 1000
#: Lines kept above the visible screen.
SCROLLBACK = 5000

#: Ctrl+C: the PTY turns it into an interrupt for the running command.
INTERRUPT = b"\x03"
#: Enter, as a terminal sends it.
ENTER = b"\r"


class ShellProtocolError(Exception):
    pass


@dataclass(frozen=True)
class Started:
    pass


@dataclass(frozen=True)
class Output:
    data: bytes


@dataclass(frozen=True)
class Exited:
    code: int | None


@dataclass(frozen=True)
class Failed:
    message: str


ShellEvent = Started | Output | Exited | Failed


def parse_message(message: bytes | str) -> ShellEvent:
    """Turn a WebSocket message from the server into an event."""
    if isinstance(message, bytes):
        return Output(message)
    try:
        control = json.loads(message)
        kind = control["type"]
    except (ValueError, KeyError, TypeError) as e:
        raise ShellProtocolError(f"bad control message {message!r}") from e
    if kind == "started":
        return Started()
    if kind == "exit":
        return Exited(control.get("code"))
    if kind == "error":
        return Failed(str(control.get("message", "shell failed")))
    raise ShellProtocolError(f"unknown control message type {kind!r}")


def clamp_size(cols: int, rows: int) -> tuple[int, int]:
    """Fit a widget size into what the server accepts."""
    return (max(1, min(cols, MAX_DIMENSION)), max(1, min(rows, MAX_DIMENSION)))


def resize_message(cols: int, rows: int) -> str:
    if not (1 <= cols <= MAX_DIMENSION and 1 <= rows <= MAX_DIMENSION):
        raise ValueError(f"terminal size must be 1-{MAX_DIMENSION} columns and rows")
    return json.dumps({"type": "resize", "cols": cols, "rows": rows}, separators=(",", ":"))


def encode_line(line: str) -> bytes:
    """A line typed in the input box, submitted with Enter."""
    return line.encode("utf-8") + ENTER


# --- rendering ----------------------------------------------------------------

_NAMED = {
    "black",
    "red",
    "green",
    "blue",
    "magenta",
    "cyan",
    "white",
}


def _color(value: str) -> str | None:
    """pyte colour to a Rich colour."""
    if value == "default":
        return None
    bright = value.startswith("bright")
    name = value.removeprefix("bright")
    if name == "brown":  # pyte's name for ANSI yellow
        name = "yellow"
    if name in _NAMED:
        return f"bright_{name}" if bright else name
    if len(value) == 6:
        try:
            int(value, 16)
            return f"#{value}"
        except ValueError:
            pass
    return None


def _style(char: pyte.screens.Char) -> Style:
    return Style(
        color=_color(char.fg),
        bgcolor=_color(char.bg),
        bold=char.bold or None,
        italic=char.italics or None,
        underline=char.underscore or None,
        strike=char.strikethrough or None,
        reverse=char.reverse or None,
    )


class Terminal:
    """A VT terminal fed with the shell's output, with scrollback."""

    def __init__(self, cols: int = 120, rows: int = 30) -> None:
        self.screen = pyte.HistoryScreen(cols, rows, history=SCROLLBACK, ratio=0.5)
        self._stream = pyte.ByteStream(self.screen)
        # Scrollback lines never change once scrolled off, so each is
        # rendered once: id(row) -> (row, rendered). The row is kept so its
        # id cannot be reused while cached.
        self._history_cache: dict[int, tuple[dict, Text]] = {}

    @property
    def size(self) -> tuple[int, int]:
        return self.screen.columns, self.screen.lines

    def feed(self, data: bytes) -> None:
        self._stream.feed(data)

    def resize(self, cols: int, rows: int) -> None:
        self.screen.resize(lines=rows, columns=cols)

    def _line(self, row: dict, width: int, cursor_x: int | None = None) -> Text:
        text = Text(no_wrap=True, end="")
        default = self.screen.default_char
        run: list[str] = []
        run_style: Style | None = None
        for x in range(width):
            char = row[x] if x in row else default
            style = _style(char)
            if x == cursor_x:
                style += Style(reverse=True)
            if style != run_style and run:
                text.append("".join(run), run_style)
                run = []
            run_style = style
            run.append(char.data or " ")
        if run:
            text.append("".join(run), run_style)
        text.rstrip()
        if cursor_x is not None and len(text) <= cursor_x:
            text.pad_right(cursor_x - len(text))
            text.append(" ", Style(reverse=True))
        return text

    def render_lines(self, show_cursor: bool = True) -> list[Text]:
        """Scrollback, then the screen. Trailing blank screen lines are
        dropped (except the cursor's) so output reads top-down."""
        width = self.screen.columns
        cache: dict[int, tuple[dict, Text]] = {}
        lines = []
        for row in self.screen.history.top:
            hit = self._history_cache.get(id(row))
            if hit is None or hit[0] is not row:
                hit = (row, self._line(row, width))
            cache[id(row)] = hit
            lines.append(hit[1])
        self._history_cache = cache
        cursor = self.screen.cursor
        screen_lines = []
        for y in range(self.screen.lines):
            cx = cursor.x if show_cursor and not cursor.hidden and y == cursor.y else None
            screen_lines.append(self._line(self.screen.buffer[y], width, cx))
        last = max(
            [y for y, line in enumerate(screen_lines) if line.plain.strip()]
            + [cursor.y if show_cursor else -1]
        )
        return lines + screen_lines[: last + 1]

    def plain_text(self) -> str:
        return "\n".join(line.plain.rstrip() for line in self.render_lines(show_cursor=False))
