"""The shell WebSocket messages and the terminal that renders them."""

from __future__ import annotations

import json

import pytest

from tetanus_rmm.shell import (
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


def test_server_messages_become_events() -> None:
    # The exact JSON the server sends (see protocol::shell tests).
    assert parse_message('{"type":"started"}') == Started()
    assert parse_message(b"\x1b[2JPS C:\\> ") == Output(b"\x1b[2JPS C:\\> ")
    assert parse_message('{"type":"exit","code":3}') == Exited(3)
    assert parse_message('{"type":"exit","code":null}') == Exited(None)
    assert parse_message('{"type":"error","message":"no ConPTY"}') == Failed("no ConPTY")


@pytest.mark.parametrize("bad", ["", "exit", "[]", '{"kind":"exit"}', '{"type":"reboot"}'])
def test_unknown_control_messages_are_errors(bad: str) -> None:
    with pytest.raises(ShellProtocolError):
        parse_message(bad)


def test_resize_matches_what_the_server_parses() -> None:
    assert resize_message(132, 43) == '{"type":"resize","cols":132,"rows":43}'
    assert json.loads(resize_message(1, 1000)) == {"type": "resize", "cols": 1, "rows": 1000}
    for cols, rows in [(0, 24), (80, 0), (1001, 24)]:
        with pytest.raises(ValueError):
            resize_message(cols, rows)
    assert clamp_size(0, 5000) == (1, 1000)


def test_input_lines_end_with_carriage_return() -> None:
    assert encode_line("Get-Date") == b"Get-Date\r"
    assert encode_line("Write-Output 'é'") == "Write-Output 'é'\r".encode()
    assert encode_line("") == b"\r"


def test_terminal_renders_colour_and_cursor() -> None:
    t = Terminal(20, 5)
    t.feed(b"\x1b[31mred\x1b[0m plain\r\n\x1b[1;38;5;33mbold\x1b[0m\r\nPS> ")
    lines = t.render_lines()
    assert [line.plain.rstrip() for line in lines] == ["red plain", "bold", "PS>"]
    red, bold, prompt = lines
    assert str(red.spans[0].style) == "red" and red.spans[0].end == 3
    assert "bold" in str(bold.spans[0].style) and "#0087ff" in str(bold.spans[0].style)
    # The cursor sits after "PS> ", drawn reversed.
    assert "reverse" in str(prompt.spans[-1].style) and prompt.spans[-1].start == 4


def test_multibyte_output_split_across_messages() -> None:
    t = Terminal(20, 2)
    data = "héllo ✓".encode()
    t.feed(data[:2])  # splits the 'é'
    t.feed(data[2:])
    assert t.plain_text() == "héllo ✓"


def test_scrollback_keeps_lines_that_scroll_off() -> None:
    t = Terminal(10, 3)
    for i in range(10):
        t.feed(b"line%d\r\n" % i)
    text = t.plain_text().splitlines()
    assert text[:10] == [f"line{i}" for i in range(10)]
    # Rendering again reuses the cached scrollback and gives the same text.
    assert t.plain_text().splitlines() == text


def test_screen_redraws_overwrite_in_place() -> None:
    # ConPTY repaints with cursor moves rather than appending.
    t = Terminal(20, 3)
    t.feed(b"PS> dir\r\n")
    t.feed(b"\x1b[1;1H\x1b[2KPS> cls")
    assert t.plain_text().splitlines()[0] == "PS> cls"


def test_resize_changes_the_screen() -> None:
    t = Terminal(80, 24)
    t.resize(132, 43)
    assert t.size == (132, 43)
    t.feed(b"x" * 100)
    assert t.plain_text() == "x" * 100, "a wider screen holds the line unwrapped"
