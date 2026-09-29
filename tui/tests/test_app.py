"""The Textual app end to end against a mocked server (headless)."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime, timedelta
from pathlib import Path

import httpx
from textual.widgets import DataTable, Input, SelectionList, Static, TextArea

from rmm_tui.api import ApiClient
from rmm_tui.app import RmmApp
from rmm_tui.auth import SessionManager, StoredSession, TokenStore
from rmm_tui.config import Config
from rmm_tui.console import ConsoleScreen
from rmm_tui.runner import ScriptScreen
from rmm_tui.screens import LoginScreen, MainScreen
from rmm_tui.scripts import ScriptLibrary

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


def make_app(server: FakeServer, kr: MemoryKeyring, tmp_path: Path, launched: list) -> RmmApp:
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
        assert table.get_row("agt-2")[0] == "WS-02"
        assert table.get_row("agt-2")[7] == "1 shell"
        assert table.get_row("agt-3")[1] == "offline"
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
        assert table.get_row("agt-1")[1] == "offline"
        assert table.get_row("agt-9")[0] == "NEW"


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
    assert command.argv == [
        "/opt/viewer",
        "--server",
        "127.0.0.1:4433",
        "--server-name",
        "127.0.0.1",
        "--ca",
        "/ca.pem",
    ]
    assert command.env == {"RMM_VIEWER_TOKEN": "vtok"}


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
        assert table.get_row("agt-2")[1] == "online (ws)"

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
