"""The Textual app end to end against a mocked server (headless)."""

from __future__ import annotations

import asyncio
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import httpx
from rich.text import Text
from textual.widgets import DataTable, Input, OptionList, SelectionList, Static, TextArea

from tetanus_rmm.api import ApiClient
from tetanus_rmm.app import RmmApp
from tetanus_rmm.auth import SessionManager, StoredSession, TokenStore
from tetanus_rmm.commands import DEFAULT_COMMANDS, CommandsScreen, QuickCommand
from tetanus_rmm.config import Config
from tetanus_rmm.console import ConsoleScreen
from tetanus_rmm.runner import NewScriptScreen, ScriptScreen
from tetanus_rmm.screens import ColumnsScreen, LoginScreen, MainScreen
from tetanus_rmm.scripts import ScriptLibrary
from tetanus_rmm.state import UiState

from .conftest import BASE, USER, FakeServer, MemoryKeyring, agent_json

LATER = datetime.now(UTC) + timedelta(hours=8)


class FakeProcess:
    """A viewer that exits as soon as it is asked to."""

    pid = 4242

    def __init__(self) -> None:
        self.returncode: int | None = None

    def poll(self) -> int | None:
        return self.returncode

    def terminate(self) -> None:
        self.returncode = -15

    def kill(self) -> None:
        self.returncode = -9

    def wait(self, timeout: float | None = None) -> int | None:
        return self.returncode


def make_app(
    server: FakeServer,
    kr: MemoryKeyring,
    tmp_path: Path,
    launched: list,
    servers: dict[str, FakeServer] | None = None,
) -> RmmApp:
    """The app against ``server`` at BASE, plus any other ``servers`` by
    URL for the login screen to switch to."""
    routes = {BASE: server, **(servers or {})}
    api = ApiClient(BASE, transport=httpx.MockTransport(server))
    config = Config(
        server_url=BASE,
        ca_path=Path("/ca.pem"),
        viewer_path="/opt/viewer",
        quic_addr="127.0.0.1:4433",
    )

    def launcher(command, log_path):
        launched.append(command)
        return FakeProcess()

    return RmmApp(
        config,
        SessionManager(api, TokenStore(BASE, kr)),
        ScriptLibrary(tmp_path / "scripts.json"),
        launcher=launcher,
        log_dir=tmp_path,
        state=UiState.load(tmp_path / "state.json"),
        api_factory=lambda url, _ca: ApiClient(url, transport=httpx.MockTransport(routes[url])),
    )


def serve_agents(server: FakeServer, user=USER) -> None:
    server.on("GET", "/api/me", body=user)
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json("agt-1", hostname="WS-01"),
            agent_json("agt-2", hostname="WS-02", shell_sessions=1),
            agent_json("agt-3", hostname="OLD", online=False),
        ],
    )


def cell_text(table: DataTable, row: str, column: str) -> str:
    cell = table.get_cell(row, column)
    return cell.plain if isinstance(cell, Text) else str(cell)


async def wait_for(pilot, condition, timeout: float = 5.0) -> None:
    deadline = asyncio.get_running_loop().time() + timeout
    while not condition():
        if asyncio.get_running_loop().time() > deadline:
            raise AssertionError("condition not reached")
        await pilot.pause(0.02)


async def test_login_persists_and_shows_the_agent_table(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    serve_agents(server)
    server.on("POST", "/api/auth/login", body={"challenge_token": "c", "expires_in_secs": 300})
    server.on(
        "POST",
        "/api/auth/totp",
        body={"session_token": "sess", "expires_at": LATER.isoformat(), "user": USER},
    )
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        app.screen.query_one("#username", Input).value = "jane"
        app.screen.query_one("#password", Input).value = "correct horse"
        app.screen.query_one("#code", Input).value = "123456"
        await pilot.click("#sign-in")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        table = app.screen.query_one("#agents", DataTable)
        await wait_for(pilot, lambda: table.row_count == 3)
        assert table.get_cell("agt-2", "host") == "WS-02"
        assert table.get_cell("agt-2", "sessions") == "1 shell"
        assert cell_text(table, "agt-3", "status") == "● offline"
    assert kr.entries, "the token was saved to the keyring"


async def test_saved_session_skips_login_and_the_table_updates(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    app = make_app(server, kr, tmp_path, launched)
    app.config = app.config.with_overrides(poll_interval=1)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        table = app.screen.query_one("#agents", DataTable)
        await wait_for(pilot, lambda: table.row_count == 3)
        # The next poll sees an agent go offline and a new one appear.
        server.on(
            "GET",
            "/api/agents",
            body=[
                agent_json("agt-1", hostname="WS-01", online=False),
                agent_json("agt-9", hostname="NEW"),
            ],
        )
        await wait_for(pilot, lambda: table.row_count == 2, timeout=5)
        assert cell_text(table, "agt-1", "status") == "● offline"
        assert table.get_cell("agt-9", "host") == "NEW"


async def test_remote_desktop_launches_the_viewer_with_a_fresh_token(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    server.on(
        "POST",
        "/api/agents/agt-1/viewer-sessions",
        body={
            "token": "vtok",
            "expires_at": LATER.isoformat(),
            "agent_id": "agt-1",
            "online": True,
        },
    )
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        await pilot.press("d")
        await wait_for(pilot, lambda: launched)
        (viewer,) = app.viewers
        assert viewer.poll() is None
    # Quitting the TUI closes the viewer window it opened.
    assert viewer.returncode == -15 and not app.viewers
    (command,) = launched
    # The side panel gets the API and the (default) command buttons.
    buttons = [arg for c in DEFAULT_COMMANDS for arg in ("--command", c.argument)]
    assert command.argv == [
        "/opt/viewer",
        "--server",
        "127.0.0.1:4433",
        "--server-name",
        "127.0.0.1",
        "--ca",
        "/ca.pem",
        "--remember-font-size",
        str(tmp_path / "viewer.json"),
        "--api-url",
        BASE,
        *buttons,
    ]
    # Both tokens stay out of the process list.
    assert command.env == {"RMM_VIEWER_TOKEN": "vtok", "RMM_API_TOKEN": "sess"}
    assert "sess" not in command.argv


async def test_auditors_see_agents_but_no_control_actions(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "reader"))
    serve_agents(server, user={"id": 2, "username": "reader", "role": "auditor"})
    server.on("GET", "/api/audit", body=[])
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        screen = app.screen
        for action in ("desktop", "shell", "scripts"):
            assert screen.check_action(action, ()) is False
        assert screen.check_action("audit", ()) is True
        # The keys do nothing: no viewer token requested, no screen pushed.
        for key in ("d", "s", "r", "enter"):
            await pilot.press(key)
        await pilot.pause(0.1)
        assert app.screen is screen and not launched
        assert not any("viewer-sessions" in r.url.path for r in server.requests)
        await pilot.press("a")
        await wait_for(pilot, lambda: type(app.screen).__name__ == "AuditScreen")


async def test_rejected_session_goes_back_to_login(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    server.on("GET", "/api/me", status=401, body={"error": "invalid or expired session"})
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test() as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
    assert not kr.entries


class FakeShell:
    """Stands in for the WebSocket: replays server messages, records input."""

    def __init__(self) -> None:
        self.sent: list[bytes | str] = []
        self.incoming: asyncio.Queue = asyncio.Queue()
        self.closed = False

    async def send_input(self, data: bytes) -> None:
        self.sent.append(data)
        if data == b"Get-Date\r":
            await self.incoming.put(b"Get-Date\r\nWednesday, 30 September 2026\r\nPS C:\\> ")

    async def send_control(self, text: str) -> None:
        self.sent.append(text)

    async def messages(self):
        while (message := await self.incoming.get()) is not None:
            yield message

    async def close(self) -> None:
        self.closed = True


async def test_console_streams_a_shell_without_the_viewer(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    app = make_app(server, kr, tmp_path, launched)
    shell = FakeShell()
    opened: list = []

    async def open_shell(agent_id, cols, rows):
        opened.append((agent_id, cols, rows))
        await shell.incoming.put('{"type":"started"}')
        await shell.incoming.put(b"PS C:\\> ")
        return shell

    app.session.api.open_shell = open_shell
    async with app.run_test(size=(120, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        await pilot.press("s")
        await wait_for(pilot, lambda: isinstance(app.screen, ConsoleScreen))
        console = app.screen
        box = console.query_one("#shell-input", Input)
        await wait_for(pilot, lambda: not box.disabled)
        agent_id, cols, rows = opened[0]
        assert agent_id == "agt-1" and cols > 50 and rows > 10

        box.value = "Get-Date"
        await pilot.press("enter")
        await wait_for(pilot, lambda: "September" in console.terminal.plain_text())
        await pilot.pause(0.1)
        content = console.query_one("#terminal-content", Static)
        assert "Wednesday" in str(content.render())

        await pilot.press("ctrl+c")
        await wait_for(pilot, lambda: b"\x03" in shell.sent)

        # A smaller terminal resizes the PTY.
        await pilot.resize_terminal(100, 30)
        await wait_for(pilot, lambda: any(isinstance(s, str) for s in shell.sent), timeout=3)
        resize = next(s for s in shell.sent if isinstance(s, str))
        assert '"type":"resize"' in resize and console.terminal.size[1] < rows

        await shell.incoming.put('{"type":"exit","code":0}')
        await wait_for(pilot, lambda: box.disabled)
        assert "exited with code 0" in str(console.query_one("#shell-status", Static).render())
        await pilot.press("escape")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
    assert shell.closed and not launched


async def test_closing_a_live_console_hangs_up_the_shell(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    app = make_app(server, kr, tmp_path, launched)
    shell = FakeShell()

    async def open_shell(agent_id, cols, rows):
        await shell.incoming.put('{"type":"started"}')
        return shell

    app.session.api.open_shell = open_shell
    async with app.run_test(size=(120, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        await pilot.press("s")
        await wait_for(pilot, lambda: isinstance(app.screen, ConsoleScreen))
        box = app.screen.query_one("#shell-input", Input)
        await wait_for(pilot, lambda: not box.disabled)
        # Esc while the shell is still running, not after it exited.
        await pilot.press("escape")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: shell.closed)
        await pilot.pause(0.2)


async def test_script_runner_fires_at_two_agents_and_shows_both(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    server.on(
        "POST",
        "/api/script-runs",
        body={
            "run_id": 12,
            "summary": {"total": 2, "succeeded": 1, "failed": 1, "not_run": 0},
            "results": [
                {
                    "agent_id": "agt-1",
                    "status": "completed",
                    "exit_code": 0,
                    "stdout": "WS-01\n",
                    "stderr": "",
                    "stdout_truncated": False,
                    "stderr_truncated": False,
                    "duration_ms": 800,
                    "error": None,
                },
                {
                    "agent_id": "agt-2",
                    "status": "completed",
                    "exit_code": 1,
                    "stdout": "",
                    "stderr": "access denied",
                    "stdout_truncated": False,
                    "stderr_truncated": False,
                    "duration_ms": 900,
                    "error": None,
                },
            ],
        },
    )
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 50)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        await pilot.press("r")
        await wait_for(pilot, lambda: isinstance(app.screen, ScriptScreen))
        screen = app.screen
        targets = screen.query_one("#targets", SelectionList)
        assert targets.selected == ["agt-1"], "the highlighted agent is preselected"
        targets.select("agt-2")
        screen.query_one("#script-body", TextArea).text = "hostname"
        screen.query_one("#timeout", Input).value = "30"
        await pilot.press("ctrl+r")

        results = screen.query_one("#results", DataTable)
        await wait_for(pilot, lambda: screen.current_run and screen.current_run.done)
        await pilot.pause(0.05)
        assert results.row_count == 2
        assert results.get_row("agt-1")[0] == "WS-01 [1]"
        assert "ok" in str(results.get_row("agt-1")[1])
        assert results.get_row("agt-2")[2] == "1"
        summary = str(screen.query_one("#run-summary", Static).render())
        assert "1/2 succeeded, 1 failed (run 12)" in summary
        assert "WS-01" in str(screen.query_one("#output", Static).render())

        results.move_cursor(row=1)
        await pilot.pause(0.05)
        assert "access denied" in str(screen.query_one("#output", Static).render())

    body = server.body(-1)
    assert body == {"agent_ids": ["agt-1", "agt-2"], "script": "hostname", "timeout_secs": 30}
    assert not launched


async def test_saving_a_script_to_the_library(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 50)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await pilot.press("r")
        await wait_for(pilot, lambda: isinstance(app.screen, ScriptScreen))
        screen = app.screen
        screen.query_one("#script-body", TextArea).text = "Get-Service Spooler"
        screen.query_one("#save-name", Input).value = "Spooler status"
        await pilot.click("#save")
        await pilot.pause(0.05)
        assert "Spooler status" in [s.name for s in screen.scripts]
    assert "Get-Service Spooler" in (tmp_path / "scripts.json").read_text()


async def test_engineers_get_only_the_actions_their_grants_allow(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    server.on("GET", "/api/me", body=USER)
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json("agt-1", hostname="A-DESK", capabilities=["desktop"]),
            agent_json("agt-2", hostname="B-SHELL", capabilities=["shell"], transport="websocket"),
        ],
    )
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        table = app.screen.query_one("#agents", DataTable)
        await wait_for(pilot, lambda: table.row_count == 2)
        screen = app.screen
        assert cell_text(table, "agt-2", "status") == "● online (ws)"

        table.move_cursor(row=table.get_row_index("agt-1"))
        await pilot.pause(0.05)
        assert screen.check_action("desktop", ()) is True
        assert screen.check_action("shell", ()) is False
        # Nobody may run scripts anywhere: the script runner is off.
        assert screen.check_action("scripts", ()) is False

        table.move_cursor(row=table.get_row_index("agt-2"))
        await pilot.pause(0.05)
        assert screen.check_action("desktop", ()) is False
        assert screen.check_action("shell", ()) is True
        await pilot.press("d")
        await pilot.pause(0.1)
        assert not any("viewer-sessions" in r.url.path for r in server.requests)


async def test_scripts_run_on_a_group_resolved_by_the_server(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    server.on("GET", "/api/me", body=USER)
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json("agt-1", hostname="WS-01", capabilities=["script"], groups=["Branch"]),
            agent_json("agt-2", hostname="WS-02", capabilities=["desktop"]),
        ],
    )
    server.on(
        "GET",
        "/api/groups",
        body=[{"id": 5, "name": "Branch", "description": "", "agent_ids": ["agt-1"]}],
    )

    def result(agent_id: str) -> dict:
        return {
            "agent_id": agent_id,
            "status": "completed",
            "exit_code": 0,
            "stdout": f"{agent_id}\n",
            "stderr": "",
            "stdout_truncated": False,
            "stderr_truncated": False,
            "duration_ms": 10,
            "error": None,
        }

    # agt-7 joined the group after the TUI loaded it.
    server.on(
        "POST",
        "/api/script-runs",
        body={"run_id": 3, "results": [result("agt-1"), result("agt-7")]},
    )
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(160, 50)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 2)
        await pilot.press("r")
        await wait_for(pilot, lambda: isinstance(app.screen, ScriptScreen))
        screen = app.screen
        targets = screen.query_one("#targets", SelectionList)
        # Only agents the user may run scripts on are offered.
        assert [targets.get_option_at_index(i).value for i in range(targets.option_count)] == [
            "agt-1"
        ]
        targets.deselect_all()
        groups = screen.query_one("#groups", SelectionList)
        await wait_for(pilot, lambda: groups.option_count == 1)
        groups.select(5)
        screen.query_one("#script-body", TextArea).text = "hostname"
        await pilot.press("ctrl+r")
        await wait_for(pilot, lambda: screen.current_run and screen.current_run.done)
        await pilot.pause(0.05)
        results = screen.query_one("#results", DataTable)
        assert results.row_count == 2
        assert results.get_row("agt-7")[0] == "agt-7"
        assert "2/2 succeeded" in str(screen.query_one("#run-summary", Static).render())

    assert server.body(-1) == {"agent_ids": [], "script": "hostname", "group_ids": [5]}


async def sign_in(pilot, app: RmmApp, server_url: str | None = None) -> None:
    screen = app.screen
    if server_url is not None:
        screen.query_one("#server", Input).value = server_url
    screen.query_one("#username", Input).value = "jane"
    screen.query_one("#password", Input).value = "correct horse"
    screen.query_one("#code", Input).value = "123456"
    await pilot.click("#sign-in")


def serve_login(server: FakeServer) -> None:
    server.on("POST", "/api/auth/login", body={"challenge_token": "c", "expires_in_secs": 300})
    server.on(
        "POST",
        "/api/auth/totp",
        body={"session_token": "sess2", "expires_at": LATER.isoformat(), "user": USER},
    )


async def test_login_to_another_server_switches_and_is_remembered(tmp_path) -> None:
    server, other, kr, launched = FakeServer(), FakeServer(), MemoryKeyring(), []
    serve_agents(other)
    serve_login(other)
    other_url = "https://other.test:9443"
    app = make_app(server, kr, tmp_path, launched, servers={other_url: other})
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        # Prefilled with the configured server.
        assert app.screen.query_one("#server", Input).value == BASE
        # The scheme is optional; a trailing slash is dropped.
        await sign_in(pilot, app, "other.test:9443/")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        assert app.config.server_url == other_url
        assert "other.test:9443" in str(app.screen.query_one("#whoami", Static).render())
    assert not any(r.url.path.startswith("/api/auth") for r in server.requests)
    assert kr.entries.get(("tetanus-rmm", other_url)), "saved for the server signed in to"
    assert UiState.load(tmp_path / "state.json").last_server == other_url


async def test_login_rejects_a_plain_http_server_url(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        await sign_in(pilot, app, "http://rmm.test:8443")
        await pilot.pause(0.05)
        status = str(app.screen.query_one("#login-status", Static).render())
        assert "https://" in status
        assert isinstance(app.screen, LoginScreen)
    assert app.config.server_url == BASE
    assert not server.requests


async def test_agent_table_shows_status_details_and_columns_can_be_changed(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    server.on("GET", "/api/me", body=USER)
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json(
                "agt-1",
                hostname="WS-01",
                logged_in_users=["CORP\\alice"],
                local_ip="192.168.1.20",
                remote_ip="203.0.113.9",
                uptime_secs=2 * 86400 + 3 * 3600,
            ),
            agent_json("agt-3", hostname="OLD", online=False),
        ],
    )
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        screen = app.screen
        table = screen.query_one("#agents", DataTable)
        await wait_for(pilot, lambda: table.row_count == 2)
        status = table.get_cell("agt-1", "status")
        assert status.plain == "● online" and status.spans[0].style == "green"
        assert table.get_cell("agt-3", "status").spans[0].style == "red"
        assert table.get_cell("agt-1", "user") == "CORP\\alice"
        assert table.get_cell("agt-1", "ip") == "192.168.1.20"
        assert table.get_cell("agt-1", "uptime") == "2d 3h"
        assert table.get_cell("agt-3", "uptime") == "–"
        assert "public_ip" not in screen.columns

        # Hide RAM, show Public IP, and move it to the front.
        table.move_cursor(row=table.get_row_index("agt-3"))
        await pilot.press("c")
        await wait_for(pilot, lambda: isinstance(app.screen, ColumnsScreen))
        menu = app.screen
        options = menu.query_one("#columns-list", SelectionList)
        options.deselect("ram")
        options.select("public_ip")
        options.highlighted = menu.order.index("public_ip")
        await pilot.pause(0.05)
        for _ in range(len(menu.order)):
            await pilot.press("shift+up")
        assert menu.order[0] == "public_ip"
        await pilot.click("#column-apply")
        await wait_for(pilot, lambda: app.screen is screen)

        assert screen.columns[0] == "public_ip" and "ram" not in screen.columns
        assert [str(c.label) for c in table.columns.values()][0] == "Public IP"
        assert table.get_cell("agt-1", "public_ip") == "203.0.113.9"
        # The selection survives the rebuild.
        assert screen.selected().id == "agt-3"
    assert UiState.load(tmp_path / "state.json").agent_columns == screen.columns

    # Remembered next launch.
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        assert app.screen.columns == screen.columns


async def test_column_menu_can_be_cancelled_and_needs_a_column(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        screen = app.screen
        before = list(screen.columns)
        await pilot.press("c")
        await wait_for(pilot, lambda: isinstance(app.screen, ColumnsScreen))
        app.screen.query_one("#columns-list", SelectionList).deselect_all()
        await pilot.pause(0.05)
        await pilot.click("#column-apply")
        await pilot.pause(0.05)
        assert isinstance(app.screen, ColumnsScreen), "nothing to show: stays open"
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is screen)
        assert screen.columns == before
    assert UiState.load(tmp_path / "state.json").agent_columns is None


async def test_new_script_button_creates_a_library_entry(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 50)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await pilot.press("r")
        await wait_for(pilot, lambda: isinstance(app.screen, ScriptScreen))
        screen = app.screen
        await pilot.click("#new-script")
        await wait_for(pilot, lambda: isinstance(app.screen, NewScriptScreen))
        dialog = app.screen

        # A name already in the library is refused.
        dialog.query_one("#new-name", Input).value = "Disk space"
        dialog.query_one("#new-body", TextArea).text = "Get-Volume"
        await pilot.click("#new-create")
        await pilot.pause(0.05)
        assert app.screen is dialog
        assert "already exists" in str(dialog.query_one("#new-status", Static).render())

        dialog.query_one("#new-name", Input).value = "Volumes"
        dialog.query_one("#new-timeout", Input).value = "60"
        await pilot.pause(0.3)  # the button ignores clicks while it animates
        await pilot.click("#new-create")
        await wait_for(pilot, lambda: app.screen is screen)

        assert "Volumes" in [s.name for s in screen.scripts]
        options = screen.query_one("#library", OptionList)
        assert options.get_option_at_index(options.highlighted).id == "Volumes"
        # Loaded into the editor, ready to run or tweak.
        assert screen.query_one("#script-body", TextArea).text == "Get-Volume"
        assert screen.query_one("#timeout", Input).value == "60"
        assert screen.query_one("#save-name", Input).value == "Volumes"

        # Ctrl+N opens it too; Esc cancels without saving.
        await pilot.press("ctrl+n")
        await wait_for(pilot, lambda: isinstance(app.screen, NewScriptScreen))
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is screen)
    saved = (tmp_path / "scripts.json").read_text()
    assert "Get-Volume" in saved and '"timeout_secs": 60' in saved


ADMIN = {"id": 1, "username": "root", "role": "admin"}
AUDITOR = {"id": 2, "username": "carol", "role": "auditor"}
GROUPS = [
    {"id": 1, "name": "Accounts", "description": "Finance", "agent_ids": ["agt-1", "agt-2"]},
    {"id": 2, "name": "Servers", "description": "", "agent_ids": ["agt-2"]},
]


def serve_grouped(server: FakeServer, user=ADMIN) -> None:
    server.on("GET", "/api/me", body=user)
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json("agt-1", hostname="WS-01", groups=["Accounts"], local_ip="10.0.5.21"),
            agent_json("agt-2", hostname="SRV-01", groups=["Accounts", "Servers"]),
            agent_json("agt-3", hostname="LONER", online=False, remote_ip="203.0.113.9"),
        ],
    )
    server.on("GET", "/api/groups", body=GROUPS)


def signed_in_app(server, tmp_path, user=ADMIN) -> RmmApp:
    kr = MemoryKeyring()
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, user["username"]))
    serve_grouped(server, user)
    return make_app(server, kr, tmp_path, [])


async def main_screen(pilot, app) -> MainScreen:
    await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
    await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
    await wait_for(pilot, lambda: len(app.screen.filter_values) == 4)
    return app.screen


async def test_agent_list_filters_by_group_from_the_side_list(tmp_path) -> None:
    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 40)) as pilot:
        screen = await main_screen(pilot, app)
        table = screen.query_one("#agents", DataTable)
        groups = screen.query_one("#group-list", OptionList)
        assert screen.filter_values == ["all", "none", "group:Accounts", "group:Servers"]
        # Each entry says how many agents it has.
        prompts = [str(groups.get_option_at_index(i).prompt) for i in range(groups.option_count)]
        assert prompts == ["All agents (3)", "No group (1)", "Accounts (2)", "Servers (1)"]

        # "f" jumps to the list; moving through it filters at once.
        await pilot.press("f")
        assert screen.focused is groups
        await pilot.press("down", "down", "down")
        await wait_for(pilot, lambda: table.row_count == 1)
        assert screen.group_filter == "group:Servers"
        assert table.get_cell("agt-2", "host") == "SRV-01"
        assert "1 of 3 agents" in str(screen.query_one("#status", Static).render())
        # Enter goes back to the table.
        await pilot.press("enter")
        assert screen.focused is table
        # The filter holds across polls.
        await pilot.press("f5")
        await pilot.pause(0.1)
        assert table.row_count == 1

        groups.highlighted = 1
        await wait_for(pilot, lambda: set(map(str, (k.value for k in table.rows))) == {"agt-3"})
        groups.highlighted = 0
        await wait_for(pilot, lambda: table.row_count == 3)

        # A group that disappears resets the filter to all.
        groups.highlighted = 3
        await wait_for(pilot, lambda: table.row_count == 1)
        server.on("GET", "/api/groups", body=GROUPS[:1])
        server.on(
            "GET",
            "/api/agents",
            body=[agent_json("agt-1", hostname="WS-01", groups=["Accounts"])],
        )
        screen.load_groups()
        await pilot.press("f5")
        await wait_for(pilot, lambda: screen.group_filter == "all")
        await wait_for(pilot, lambda: table.row_count == 1)
        assert groups.highlighted == 0
        assert screen.filter_values == ["all", "none", "group:Accounts"]


async def test_search_filters_by_hostname_with_the_group_list(tmp_path) -> None:
    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 40)) as pilot:
        screen = await main_screen(pilot, app)
        table = screen.query_one("#agents", DataTable)
        search = screen.query_one("#search", Input)

        def shown() -> set[str]:
            return {str(k.value) for k in table.rows}

        # "/" focuses the search; letters go to it, not to the shortcuts.
        await pilot.press("slash")
        assert screen.focused is search
        await pilot.press("w", "s")
        await wait_for(pilot, lambda: shown() == {"agt-1"})
        assert "1 of 3 agents" in str(screen.query_one("#status", Static).render())
        assert not isinstance(app.screen, ScriptScreen)

        # Case-insensitive, anywhere in the name.
        search.value = "-0"
        await wait_for(pilot, lambda: shown() == {"agt-1", "agt-2"})
        search.value = "srv"
        await wait_for(pilot, lambda: shown() == {"agt-2"})
        # IP addresses (the agent's own and its public one) and group names too.
        search.value = "10.0.5"
        await wait_for(pilot, lambda: shown() == {"agt-1"})
        search.value = "203.0.113.9"
        await wait_for(pilot, lambda: shown() == {"agt-3"})
        search.value = "accou"
        await wait_for(pilot, lambda: shown() == {"agt-1", "agt-2"})
        search.value = "servers"
        await wait_for(pilot, lambda: shown() == {"agt-2"})
        # Combined with the group list.
        search.value = "-0"
        screen.query_one("#group-list", OptionList).highlighted = 3
        await wait_for(pilot, lambda: shown() == {"agt-2"})
        screen.query_one("#group-list", OptionList).highlighted = 0
        await wait_for(pilot, lambda: shown() == {"agt-1", "agt-2"})
        # Holds across polls.
        screen.refresh_agents()
        await pilot.pause(0.1)
        assert shown() == {"agt-1", "agt-2"}

        # Enter goes to the table; Esc clears the search.
        search.focus()
        await pilot.press("enter")
        assert screen.focused is table
        await pilot.press("escape")
        await wait_for(pilot, lambda: len(shown()) == 3)
        assert search.value == ""


async def test_admins_classify_the_selected_agent(tmp_path) -> None:
    from tetanus_rmm.screens import ClassifyScreen

    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json("agt-1", hostname="WS-01", classification="desktop", os="Windows 11 Pro"),
            agent_json("agt-2", hostname="SRV-01", classification="server"),
            agent_json("agt-3", hostname="LONER", online=False),
        ],
    )
    server.on(
        "PUT",
        "/api/agents/agt-2/classification",
        body={"classification": "other", "classification_override": "other"},
    )
    async with app.run_test(size=(160, 40)) as pilot:
        screen = await main_screen(pilot, app)
        table = screen.query_one("#agents", DataTable)
        assert cell_text(table, "agt-1", "class") == "Desktop"
        assert cell_text(table, "agt-1", "os") == "Windows 11 Pro"
        assert cell_text(table, "agt-2", "os") == "–"

        table.move_cursor(row=table.get_row_index("agt-2"))
        await pilot.press("k")
        await wait_for(pilot, lambda: isinstance(app.screen, ClassifyScreen))
        options = app.screen.query_one("#classify-list", OptionList)
        # Nothing chosen yet: Automatic, which the device kind decides.
        assert options.get_option_at_index(options.highlighted).id == "auto"
        assert str(options.get_option_at_index(3).prompt) == "Automatic (Desktop)"
        options.highlighted = 2
        await pilot.press("enter")
        await wait_for(pilot, lambda: any(r.method == "PUT" for r in server.requests), timeout=5)
        put = [r for r in server.requests if r.method == "PUT"][-1]
        assert json.loads(put.content) == {"classification": "other"}

        # Choosing Automatic clears the admin's choice; cancelling sends nothing.
        server.on(
            "GET",
            "/api/agents",
            body=[
                agent_json("agt-1", hostname="WS-01"),
                agent_json(
                    "agt-2",
                    hostname="SRV-01",
                    classification="other",
                    classification_override="other",
                ),
                agent_json("agt-3", hostname="LONER", online=False),
            ],
        )
        await wait_for(pilot, lambda: app.screen is screen)
        await pilot.press("f5")
        await wait_for(pilot, lambda: cell_text(table, "agt-2", "class") == "Other*")
        await pilot.press("k")
        await wait_for(pilot, lambda: isinstance(app.screen, ClassifyScreen))
        options = app.screen.query_one("#classify-list", OptionList)
        assert options.get_option_at_index(options.highlighted).id == "other"
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is screen)
        puts = sum(r.method == "PUT" for r in server.requests)
        await pilot.press("k")
        await wait_for(pilot, lambda: isinstance(app.screen, ClassifyScreen))
        app.screen.query_one("#classify-list", OptionList).highlighted = 3
        await pilot.press("enter")
        await wait_for(pilot, lambda: sum(r.method == "PUT" for r in server.requests) > puts)
        put = [r for r in server.requests if r.method == "PUT"][-1]
        assert json.loads(put.content) == {"classification": None}


async def test_only_admins_are_offered_classify(tmp_path) -> None:
    server = FakeServer()
    app = signed_in_app(server, tmp_path, user=AUDITOR)
    async with app.run_test(size=(160, 40)) as pilot:
        screen = await main_screen(pilot, app)
        assert not screen.check_action("classify", ())
        await pilot.press("k")
        await pilot.pause(0.1)
        assert app.screen is screen


async def test_new_agent_makes_an_msi_link_in_a_group_and_saves_it(tmp_path) -> None:
    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    server.on(
        "POST",
        "/api/enrollment-links",
        body={
            "token": "t0k",
            "expires_at": LATER.isoformat(),
            "download_url": "https://pub:8443/api/download/windows-x86_64?token=t0k",
            "msi_url": "https://pub:8443/api/download/windows-x86_64/msi?token=t0k",
            "server": "10.0.0.9:4433",
            "server_name": "rmm.test",
        },
    )
    server.on(
        "GET",
        "/api/download/windows-x86_64/msi",
        handler=lambda _req: httpx.Response(200, content=b"MSI-BYTES"),
    )
    async with app.run_test(size=(160, 50)) as pilot:
        await main_screen(pilot, app)
        await pilot.press("n")
        await wait_for(pilot, lambda: type(app.screen).__name__ == "NewAgentScreen")
        screen = app.screen
        groups = screen.query_one("#na-groups", SelectionList)
        await wait_for(pilot, lambda: groups.option_count == 2)
        # Defaults: the server this TUI uses (QUIC address from the config).
        assert screen.query_one("#na-server", Input).value == "127.0.0.1:4433"
        assert screen.query_one("#na-server-name", Input).value == "rmm.test"

        screen.query_one("#na-server", Input).value = "10.0.0.9:4433"
        groups.select(2)
        await pilot.press("ctrl+g")
        await wait_for(pilot, lambda: screen.link is not None)
        body = server.body(
            next(i for i, r in enumerate(server.requests) if r.url.path.endswith("links"))
        )
        assert body == {
            "platform": "windows-x86_64",
            "ttl_secs": 86400,
            "group_ids": [2],
            "server": "10.0.0.9:4433",
            "server_name": "rmm.test",
        }
        assert screen.query_one("#na-msi-url", Input).value.endswith("/msi?token=t0k")
        assert "--token t0k" in screen.query_one("#na-command", Input).value
        assert "It joins: Servers" in str(screen.query_one("#na-summary", Static).render())

        target = tmp_path / "out" / "agent.msi"
        target.parent.mkdir()
        screen.query_one("#na-save-path", Input).value = str(target)
        screen.query_one("#na-save").press()
        await wait_for(pilot, lambda: target.exists())
        assert target.read_bytes() == b"MSI-BYTES"
        # The next save would not overwrite it.
        await wait_for(
            pilot, lambda: screen.query_one("#na-save-path", Input).value.endswith("agent-2.msi")
        )


async def test_engineers_make_links_without_groups_and_auditors_cannot(tmp_path) -> None:
    server = FakeServer()
    app = signed_in_app(server, tmp_path, USER)
    async with app.run_test(size=(160, 50)) as pilot:
        screen = await main_screen(pilot, app)
        assert screen.check_action("new_agent", ()) is True
        await pilot.press("n")
        await wait_for(pilot, lambda: type(app.screen).__name__ == "NewAgentScreen")
        assert not app.screen.query("#na-groups")
        assert "Only admins" in str(app.screen.query_one("#na-groups-note", Static).render())

    server = FakeServer()
    app = signed_in_app(server, tmp_path, AUDITOR)
    async with app.run_test(size=(160, 50)) as pilot:
        screen = await main_screen(pilot, app)
        assert screen.check_action("new_agent", ()) is False
        await pilot.press("n")
        await pilot.pause(0.1)
        assert app.screen is screen


async def test_admins_create_rename_fill_and_delete_groups(tmp_path) -> None:
    from tetanus_rmm.groups import ConfirmScreen, GroupEditScreen, GroupsScreen, MembersScreen

    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    created = {"id": 3, "name": "Branch", "description": "Leeds", "agent_ids": []}
    server.on("POST", "/api/groups", status=201, body=created)
    server.on("PATCH", "/api/groups/1", body={**GROUPS[0], "name": "Finance"})
    server.on("PUT", "/api/groups/1/agents", body={**GROUPS[0], "agent_ids": ["agt-3"]})
    server.on("DELETE", "/api/groups/2", handler=lambda _req: httpx.Response(204))
    last = lambda method: next(r for r in reversed(server.requests) if r.method == method)  # noqa: E731
    async with app.run_test(size=(160, 50)) as pilot:
        main = await main_screen(pilot, app)
        await pilot.press("g")
        await wait_for(pilot, lambda: isinstance(app.screen, GroupsScreen))
        screen = app.screen
        table = screen.query_one("#groups-table", DataTable)
        await wait_for(pilot, lambda: table.row_count == 2)
        assert "WS-01" in str(screen.query_one("#group-members", Static).render())

        await pilot.press("n")
        await wait_for(pilot, lambda: isinstance(app.screen, GroupEditScreen))
        app.screen.query_one("#ge-name", Input).value = "Branch"
        app.screen.query_one("#ge-description", Input).value = "Leeds"
        await pilot.click("#ge-save")
        await wait_for(pilot, lambda: any(r.method == "POST" for r in server.requests))
        assert json.loads(last("POST").content) == {"name": "Branch", "description": "Leeds"}

        await wait_for(pilot, lambda: app.screen is screen)
        table.move_cursor(row=table.get_row_index("1"))
        await pilot.press("e")
        await wait_for(pilot, lambda: isinstance(app.screen, GroupEditScreen))
        assert app.screen.query_one("#ge-name", Input).value == "Accounts"
        app.screen.query_one("#ge-name", Input).value = "Finance"
        await pilot.click("#ge-save")
        await wait_for(pilot, lambda: any(r.method == "PATCH" for r in server.requests))
        assert json.loads(last("PATCH").content)["name"] == "Finance"

        await wait_for(pilot, lambda: app.screen is screen)
        table.move_cursor(row=table.get_row_index("1"))
        await pilot.press("m")
        await wait_for(pilot, lambda: isinstance(app.screen, MembersScreen))
        members = app.screen.query_one("#members-list", SelectionList)
        assert set(members.selected) == {"agt-1", "agt-2"}
        members.deselect_all()
        members.select("agt-3")
        await pilot.click("#members-save")
        await wait_for(pilot, lambda: any(r.method == "PUT" for r in server.requests))
        assert json.loads(last("PUT").content) == {"agent_ids": ["agt-3"]}

        await wait_for(pilot, lambda: app.screen is screen)
        table.move_cursor(row=table.get_row_index("2"))
        await pilot.press("delete")
        await wait_for(pilot, lambda: isinstance(app.screen, ConfirmScreen))
        await pilot.click("#confirm-ok")
        await wait_for(pilot, lambda: any(r.method == "DELETE" for r in server.requests))
        assert last("DELETE").url.path == "/api/groups/2"

        # Back on the agent list, which reloads groups for its filter.
        await wait_for(pilot, lambda: app.screen is screen)
        polls = sum(r.url.path == "/api/groups" for r in server.requests)
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is main)
        await wait_for(
            pilot, lambda: sum(r.url.path == "/api/groups" for r in server.requests) > polls
        )


async def test_non_admins_see_groups_read_only(tmp_path) -> None:
    from tetanus_rmm.groups import GroupsScreen

    server = FakeServer()
    app = signed_in_app(server, tmp_path, USER)
    async with app.run_test(size=(160, 50)) as pilot:
        await main_screen(pilot, app)
        await pilot.press("g")
        await wait_for(pilot, lambda: isinstance(app.screen, GroupsScreen))
        screen = app.screen
        await wait_for(pilot, lambda: screen.query_one("#groups-table", DataTable).row_count == 2)
        for action in ("new", "edit", "members", "delete"):
            assert screen.check_action(action, ()) is False
        for key in ("n", "e", "m", "delete"):
            await pilot.press(key)
        await pilot.pause(0.1)
        assert app.screen is screen
        assert not any(r.method in ("POST", "PATCH", "PUT", "DELETE") for r in server.requests)
        assert "Read only" in str(screen.query_one("#groups-status", Static).render())


USERS = [
    ADMIN,
    {"id": 7, "username": "jane", "role": "support_engineer"},
    AUDITOR,
]
GRANT = {
    "id": 5,
    "user_id": 7,
    "username": "jane",
    "agent_id": None,
    "group_id": 1,
    "group_name": "Accounts",
    "all_agents": False,
    "capabilities": ["desktop", "shell"],
}


async def test_admins_manage_users(tmp_path) -> None:
    from tetanus_rmm.groups import ConfirmScreen
    from tetanus_rmm.users import (
        GrantEditScreen,
        GrantsScreen,
        NewUserScreen,
        PasswordScreen,
        RoleScreen,
        TotpScreen,
        UsersScreen,
    )

    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    sam = {"id": 9, "username": "sam", "role": "auditor"}
    enrollment = {"user": sam, "totp_secret": "NEWSECRET", "otpauth_url": "otpauth://totp/sam"}
    server.on("GET", "/api/users", body=USERS)
    server.on("GET", "/api/grants", body=[GRANT])
    server.on("POST", "/api/users", status=201, body=enrollment)
    server.on("PUT", "/api/users/7/role", body={**USERS[1], "role": "admin"})
    server.on("PUT", "/api/users/7/password", body=USERS[1])
    server.on("POST", "/api/users/7/totp", body={**enrollment, "user": USERS[1]})
    server.on("POST", "/api/grants", status=201, body=GRANT)
    server.on("DELETE", "/api/grants/5", handler=lambda _req: httpx.Response(204))
    server.on("DELETE", "/api/users/2", handler=lambda _req: httpx.Response(204))

    def sent(method: str, path: str) -> list[httpx.Request]:
        return [r for r in server.requests if r.method == method and r.url.path == path]

    async with app.run_test(size=(160, 50)) as pilot:
        main = await main_screen(pilot, app)
        assert main.check_action("users", ()) is True
        await pilot.press("u")
        await wait_for(pilot, lambda: isinstance(app.screen, UsersScreen))
        screen = app.screen
        table = screen.query_one("#users-table", DataTable)
        await wait_for(pilot, lambda: table.row_count == 3)
        rows = {str(table.get_row(str(u["id"]))[0]): table.get_row(str(u["id"])) for u in USERS}
        assert rows["root (you)"][1:] == ["Admin", "Everything"]
        assert rows["jane"][1:] == ["Support engineer", "Group Accounts"]
        assert rows["carol"][1:] == ["Auditor", "Read only"]

        # Not yourself; and grants are only for support engineers.
        table.move_cursor(row=table.get_row_index("1"))
        await pilot.pause()
        assert screen.check_action("delete", ()) is None
        assert screen.check_action("access", ()) is None
        assert screen.check_action("password", ()) is True

        # New user: the password is checked before anything is sent.
        await pilot.press("n")
        await wait_for(pilot, lambda: isinstance(app.screen, NewUserScreen))
        dialog = app.screen
        dialog.query_one("#nu-username", Input).value = "sam"
        dialog.query_one("#nu-password", Input).value = "short"
        dialog.query_one("#nu-repeat", Input).value = "short"
        status = dialog.query_one("#nu-status", Static)
        await pilot.click("#nu-save")
        await wait_for(pilot, lambda: "at least 12" in str(status.render()))
        dialog.query_one("#nu-password", Input).value = "a long password"
        dialog.query_one("#nu-repeat", Input).value = "a long passwerd"
        # A button ignores clicks while it still shows the last press.
        await pilot.pause(0.3)
        await pilot.click("#nu-save")
        await wait_for(pilot, lambda: "do not match" in str(status.render()))
        assert not sent("POST", "/api/users")
        dialog.query_one("#nu-repeat", Input).value = "a long password"
        await pilot.pause(0.3)
        roles = dialog.query_one("#nu-role", OptionList)
        roles.highlighted = roles.get_option_index("auditor")
        await pilot.click("#nu-save")
        # The new TOTP secret is shown, once.
        await wait_for(pilot, lambda: isinstance(app.screen, TotpScreen))
        assert json.loads(sent("POST", "/api/users")[-1].content) == {
            "username": "sam",
            "password": "a long password",
            "role": "auditor",
        }
        assert app.screen.query_one("#totp-secret", Input).value == "NEWSECRET"
        assert app.screen.query_one("#totp-url", Input).value == "otpauth://totp/sam"
        await pilot.click("#totp-close")
        await wait_for(pilot, lambda: app.screen is screen)

        # Role.
        table.move_cursor(row=table.get_row_index("7"))
        await pilot.press("r")
        await wait_for(pilot, lambda: isinstance(app.screen, RoleScreen))
        roles = app.screen.query_one("#role-list", OptionList)
        assert roles.highlighted == roles.get_option_index("support_engineer")
        roles.highlighted = roles.get_option_index("admin")
        await pilot.press("enter")
        await wait_for(pilot, lambda: bool(sent("PUT", "/api/users/7/role")))
        assert json.loads(sent("PUT", "/api/users/7/role")[-1].content) == {"role": "admin"}

        # Access: remove a grant, add another.
        await wait_for(pilot, lambda: app.screen is screen)
        table.move_cursor(row=table.get_row_index("7"))
        await pilot.press("a")
        await wait_for(pilot, lambda: isinstance(app.screen, GrantsScreen))
        grants = app.screen
        listed = grants.query_one("#grants-list", OptionList)
        await wait_for(pilot, lambda: listed.option_count == 1)
        assert sent("GET", "/api/grants")[-1].url.params["user_id"] == "7"
        assert str(listed.get_option_at_index(0).prompt) == "Group Accounts: desktop, shell"
        await pilot.press("delete")
        await wait_for(pilot, lambda: bool(sent("DELETE", "/api/grants/5")))
        await pilot.press("n")
        await wait_for(pilot, lambda: isinstance(app.screen, GrantEditScreen))
        scope = app.screen.query_one("#gr-scope", OptionList)
        prompts = [str(scope.get_option_at_index(i).prompt) for i in range(scope.option_count)]
        assert prompts[:3] == [
            "All agents (also those enrolled later)",
            "Group: Accounts",
            "Group: Servers",
        ]
        assert len(prompts) == 6
        scope.highlighted = scope.get_option_index("group:2")
        capabilities = app.screen.query_one("#gr-capabilities", SelectionList)
        capabilities.deselect_all()
        await pilot.click("#gr-save")
        await pilot.pause(0.3)
        assert not sent("POST", "/api/grants")
        capabilities.select("script")
        await pilot.click("#gr-save")
        await wait_for(pilot, lambda: bool(sent("POST", "/api/grants")))
        assert json.loads(sent("POST", "/api/grants")[-1].content) == {
            "user_id": 7,
            "capabilities": ["script"],
            "group_id": 2,
        }
        await wait_for(pilot, lambda: app.screen is grants)
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is screen)

        # Password.
        await pilot.press("p")
        await wait_for(pilot, lambda: isinstance(app.screen, PasswordScreen))
        app.screen.query_one("#pw-password", Input).value = "the next password"
        app.screen.query_one("#pw-repeat", Input).value = "the next password"
        await pilot.click("#pw-save")
        await wait_for(pilot, lambda: bool(sent("PUT", "/api/users/7/password")))
        assert json.loads(sent("PUT", "/api/users/7/password")[-1].content) == {
            "password": "the next password"
        }

        # TOTP reset, after confirming.
        await wait_for(pilot, lambda: app.screen is screen)
        await pilot.press("t")
        await wait_for(pilot, lambda: isinstance(app.screen, ConfirmScreen))
        await pilot.click("#confirm-ok")
        await wait_for(pilot, lambda: isinstance(app.screen, TotpScreen))
        assert sent("POST", "/api/users/7/totp")
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is screen)

        # Delete, after confirming.
        table.move_cursor(row=table.get_row_index("2"))
        await pilot.press("delete")
        await wait_for(pilot, lambda: isinstance(app.screen, ConfirmScreen))
        await pilot.click("#confirm-ok")
        await wait_for(pilot, lambda: bool(sent("DELETE", "/api/users/2")))

        await wait_for(pilot, lambda: app.screen is screen)
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is main)


async def test_the_users_menu_is_for_admins_only(tmp_path) -> None:
    from textual.widgets import Footer

    for user in (USER, AUDITOR):
        server = FakeServer()
        app = signed_in_app(server, tmp_path, user)
        async with app.run_test(size=(160, 50)) as pilot:
            screen = await main_screen(pilot, app)
            assert screen.check_action("users", ()) is False
            shown = {b.action for _, b, enabled, _ in screen.active_bindings.values() if enabled}
            assert "users" not in shown and "groups" in shown
            assert screen.query_one(Footer)
            await pilot.press("u")
            await pilot.pause(0.1)
            assert app.screen is screen
            assert not any(r.url.path == "/api/users" for r in server.requests)


async def test_an_admin_who_demotes_themselves_leaves_the_users_screen(tmp_path) -> None:
    from tetanus_rmm.users import RoleScreen, UsersScreen

    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    server.on("GET", "/api/users", body=[ADMIN, {"id": 3, "username": "zed", "role": "admin"}])
    server.on("GET", "/api/grants", body=[])
    server.on("PUT", "/api/users/1/role", body={**ADMIN, "role": "auditor"})
    async with app.run_test(size=(160, 50)) as pilot:
        await main_screen(pilot, app)
        await pilot.press("u")
        await wait_for(pilot, lambda: isinstance(app.screen, UsersScreen))
        table = app.screen.query_one("#users-table", DataTable)
        await wait_for(pilot, lambda: table.row_count == 2)
        table.move_cursor(row=table.get_row_index("1"))
        await pilot.press("r")
        await wait_for(pilot, lambda: isinstance(app.screen, RoleScreen))
        roles = app.screen.query_one("#role-list", OptionList)
        roles.highlighted = roles.get_option_index("auditor")
        await pilot.press("enter")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        assert app.session.user.role == "auditor"
        assert app.screen.check_action("users", ()) is False
        assert "auditor" in str(app.screen.query_one("#whoami", Static).render())


async def test_viewer_buttons_are_edited_saved_and_passed_to_the_viewer(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    server.on(
        "POST",
        "/api/agents/agt-1/viewer-sessions",
        body={
            "token": "vtok",
            "expires_at": LATER.isoformat(),
            "agent_id": "agt-1",
            "online": True,
        },
    )
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 45)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        main = app.screen
        await wait_for(pilot, lambda: main.query_one("#agents", DataTable).row_count == 3)
        await pilot.press("v")
        await wait_for(pilot, lambda: isinstance(app.screen, CommandsScreen))
        dialog = app.screen
        assert dialog.commands == list(DEFAULT_COMMANDS)

        # A label with "=" is refused; a good one is added at the end.
        dialog.query_one("#command-label", Input).value = "a=b"
        dialog.query_one("#command-text", Input).value = "x"
        await pilot.click("#command-add")
        await pilot.pause(0.05)
        assert "cannot contain" in str(dialog.query_one("#commands-status", Static).render())
        dialog.query_one("#command-label", Input).value = "Printers"
        dialog.query_one("#command-text", Input).value = "control printers"
        await pilot.pause(0.3)  # the button ignores clicks while it animates
        await pilot.click("#command-add")
        await pilot.pause(0.05)
        assert dialog.commands[-1] == QuickCommand("Printers", "control printers")

        # Remove the first two; move the new one to the top.
        options = dialog.query_one("#commands-list", OptionList)
        options.focus()
        options.highlighted = 0
        await pilot.press("delete", "delete")
        options.highlighted = len(dialog.commands) - 1
        for _ in range(len(dialog.commands)):
            await pilot.press("shift+up")
        assert dialog.commands[0].label == "Printers"
        assert "Command prompt" not in [c.label for c in dialog.commands]
        await pilot.click("#command-save")
        await wait_for(pilot, lambda: app.screen is main)
        saved = list(dialog.commands)

        # The next viewer gets exactly those buttons.
        await pilot.press("d")
        await wait_for(pilot, lambda: launched)
    argv = launched[0].argv
    assert [argv[i + 1] for i, a in enumerate(argv) if a == "--command"] == [
        c.argument for c in saved
    ]
    assert UiState.load(tmp_path / "state.json").viewer_commands == saved

    # Cancelling changes nothing; an emptied list means no buttons at all.
    app = make_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 45)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await pilot.press("v")
        await wait_for(pilot, lambda: isinstance(app.screen, CommandsScreen))
        assert app.screen.commands == saved
        app.screen.commands.clear()
        await pilot.press("escape")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        assert app.state.commands == saved
        app.state.viewer_commands = []
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        launched.clear()
        await pilot.press("d")
        await wait_for(pilot, lambda: launched)
    assert "--no-default-commands" in launched[0].argv
    assert "--command" not in launched[0].argv


async def test_the_menu_bar_runs_the_agent_tables_actions(tmp_path) -> None:
    from tetanus_rmm.menu import MenuBar, MenuScreen

    server = FakeServer()
    app = signed_in_app(server, tmp_path, USER)
    async with app.run_test(size=(160, 50)) as pilot:
        screen = await main_screen(pilot, app)
        # Every action in a menu has a binding to take its label and key from.
        actions = {b.action for b in MainScreen.BINDINGS}
        assert all(a in actions for menu in MainScreen.MENUS.values() for a in menu)

        # The key opens the first menu; right walks to the others.
        await pilot.press("m")
        await wait_for(pilot, lambda: isinstance(app.screen, MenuScreen))
        items = app.screen.query_one("#menu-list", OptionList)
        assert [items.get_option_at_index(i).id for i in range(items.option_count)] == [
            "desktop",
            "shell",
            "scripts",
            "classify",
            "new_agent",
        ]
        assert str(items.get_option_at_index(0).prompt).split() == ["Remote", "desktop", "d"]
        # A support engineer may not classify.
        assert items.get_option("classify").disabled
        assert not items.get_option("desktop").disabled
        await pilot.press("right")
        assert items.get_option_at_index(0).id == "search"
        assert not items.get_option("app.change_theme").disabled
        await pilot.press("down", "down", "enter")
        await wait_for(pilot, lambda: isinstance(app.screen, ColumnsScreen))
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is screen)

        # A click on a title opens that menu; Esc closes it and runs nothing.
        titles = screen.query_one(MenuBar).titles
        await pilot.click(titles[2])
        await wait_for(pilot, lambda: isinstance(app.screen, MenuScreen))
        assert titles[2].has_class("-open")
        assert app.screen.query_one("#menu-list", OptionList).get_option("users").disabled
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is screen)
        assert not titles[2].has_class("-open")


async def test_the_theme_is_kept_between_runs(tmp_path) -> None:
    from tetanus_rmm.themes import THEMES

    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 50)) as pilot:
        await main_screen(pilot, app)
        assert all(theme.name in app.available_themes for theme in THEMES)
        # Ours is the default, and starting with it saves nothing.
        assert app.theme == "tetanus"
        assert UiState.load(tmp_path / "state.json").theme is None
        app.theme = "nord"
        await wait_for(pilot, lambda: UiState.load(tmp_path / "state.json").theme == "nord")

    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 50)) as pilot:
        await main_screen(pilot, app)
        assert app.theme == "nord"

    # A saved theme that no longer exists is ignored.
    state = UiState.load(tmp_path / "state.json")
    state.theme = "gone"
    state.save()
    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 50)) as pilot:
        await main_screen(pilot, app)
        assert app.theme == "tetanus"


async def test_the_theme_editor_previews_saves_and_deletes_themes(tmp_path) -> None:
    from textual.widgets import Button

    from tetanus_rmm.groups import ConfirmScreen
    from tetanus_rmm.theme_editor import ThemeEditorScreen
    from tetanus_rmm.themes import PREVIEW

    def saved() -> UiState:
        return UiState.load(tmp_path / "state.json")

    server = FakeServer()
    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 50)) as pilot:
        main = await main_screen(pilot, app)
        await pilot.press("e")
        await wait_for(pilot, lambda: isinstance(app.screen, ThemeEditorScreen))
        editor = app.screen
        name = editor.query_one("#te-name", Input)
        primary = editor.query_one("#te-primary", Input)
        # It opens on the theme in use, as the start of a new one.
        assert name.value == "tetanus-custom" and primary.value == "#FF9000"
        assert editor.query_one("#te-delete", Button).disabled

        # A change shows at once, on the app itself, and is not saved.
        primary.value = "#00FF00"
        await wait_for(pilot, lambda: app.current_theme.primary == "#00FF00")
        assert app.theme == PREVIEW
        assert str(editor.query_one("#te-swatch-primary").styles.background.hex) == "#00FF00"
        # Half-typed colours leave the preview as it was.
        primary.value = "#00F"
        await pilot.pause(0.1)
        assert app.current_theme.primary == "#00FF00"
        assert "Primary must be" in str(editor.query_one("#te-status", Static).render())
        primary.value = "#00FF00"

        # A built-in theme's name is refused; one of your own is saved and used.
        name.value = "nord"
        await pilot.press("ctrl+s")
        await pilot.pause(0.1)
        assert saved().custom_themes == {}
        name.value = "Lime"
        await pilot.press("ctrl+s")
        await wait_for(pilot, lambda: "lime" in saved().custom_themes)
        assert saved().theme == "lime" and saved().custom_themes["lime"].primary == "#00FF00"
        assert saved().custom_themes["lime"].text_alpha == 1.0  # from tetanus
        assert not editor.query_one("#te-delete", Button).disabled

        # Looking at another theme and leaving puts the saved one on.
        themes = editor.query_one("#te-themes", OptionList)
        themes.highlighted = themes.get_option_index("nord")
        await wait_for(pilot, lambda: app.current_theme.primary == "#88C0D0")
        assert name.value == "nord-custom"
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is main)
        assert app.theme == "lime" and PREVIEW not in app.available_themes
        await pilot.pause(0.1)
        assert saved().theme == "lime"

    # It is there on the next run, to edit or delete.
    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 50)) as pilot:
        main = await main_screen(pilot, app)
        assert app.theme == "lime" and app.current_theme.primary == "#00FF00"
        await pilot.press("e")
        await wait_for(pilot, lambda: isinstance(app.screen, ThemeEditorScreen))
        editor = app.screen
        assert editor.query_one("#te-name", Input).value == "lime"
        editor.query_one("#te-accent", Input).value = "#123456"
        await pilot.press("ctrl+s")
        await wait_for(pilot, lambda: saved().custom_themes["lime"].accent == "#123456")

        await pilot.click("#te-delete")
        await wait_for(pilot, lambda: isinstance(app.screen, ConfirmScreen))
        await pilot.click("#confirm-ok")
        await wait_for(pilot, lambda: saved().custom_themes == {})
        assert saved().theme is None and "lime" not in app.available_themes
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is main)
        assert app.theme == "tetanus"

    # A cancelled visit changes nothing.
    app = signed_in_app(server, tmp_path)
    async with app.run_test(size=(160, 50)) as pilot:
        main = await main_screen(pilot, app)
        app.theme = "nord"
        await pilot.press("e")
        await wait_for(pilot, lambda: isinstance(app.screen, ThemeEditorScreen))
        app.screen.query_one("#te-primary", Input).value = "#ABCDEF"
        await wait_for(pilot, lambda: app.current_theme.primary == "#ABCDEF")
        await pilot.press("escape")
        await wait_for(pilot, lambda: app.screen is main)
        assert app.theme == "nord"
        await pilot.pause(0.1)
        assert saved().theme == "nord" and saved().custom_themes == {}
