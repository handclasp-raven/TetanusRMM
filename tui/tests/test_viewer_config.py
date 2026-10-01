"""Viewer launch command, config loading and table formatting."""

from __future__ import annotations

import signal
import socket
import subprocess
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from rmm_tui import formatting
from rmm_tui.__main__ import parse_args, resolve_config
from rmm_tui.api import Agent, ViewerSession
from rmm_tui.commands import DEFAULT_COMMANDS, CommandError, QuickCommand, from_json
from rmm_tui.config import Config, ConfigError, load, normalize_server_url
from rmm_tui.state import UiState
from rmm_tui.viewer import (
    ViewerCommand,
    ViewerError,
    build_command,
    close_all,
    launch,
    pick_address,
    split_host_port,
)

SESSION = ViewerSession("secret-token", datetime(2026, 9, 30, tzinfo=UTC), "agt-1", True)


def fake_resolve(answers: dict[str, str]):
    calls = []

    def resolve(host: str, port: int) -> str:
        calls.append((host, port))
        return answers[host]

    resolve.calls = calls  # type: ignore[attr-defined]
    return resolve


def test_viewer_gets_the_resolved_quic_address_and_the_token_in_env() -> None:
    config = Config(
        server_url="https://rmm.example.com:8443",
        ca_path=Path("/ca.pem"),
        viewer_path="/opt/viewer",
    )
    resolve = fake_resolve({"rmm.example.com": "203.0.113.5"})
    cmd = build_command(config, SESSION, resolve)
    assert cmd.argv == [
        "/opt/viewer",
        "--server",
        "203.0.113.5:4433",
        "--server-name",
        "rmm.example.com",
        "--ca",
        "/ca.pem",
    ]
    assert resolve.calls == [("rmm.example.com", 4433)]
    # Not visible in the process list.
    assert cmd.env == {"RMM_VIEWER_TOKEN": "secret-token"}
    assert "secret-token" not in " ".join(cmd.argv)


def test_the_side_panel_gets_the_api_and_the_buttons() -> None:
    config = Config(
        server_url="https://rmm.example.com:8443",
        ca_path=Path("/ca.pem"),
        viewer_path="/opt/viewer",
    )
    resolve = fake_resolve({"rmm.example.com": "203.0.113.5"})
    buttons = [QuickCommand("Network connections", "ncpa.cpl"), QuickCommand("RDP", "mstsc /v:a=b")]
    cmd = build_command(config, SESSION, resolve, api_token="api-sess", commands=buttons)
    assert cmd.argv[7:] == [
        "--api-url",
        "https://rmm.example.com:8443",
        "--command",
        "Network connections=ncpa.cpl",
        "--command",
        "RDP=mstsc /v:a=b",
    ]
    assert cmd.env == {"RMM_VIEWER_TOKEN": "secret-token", "RMM_API_TOKEN": "api-sess"}
    assert "api-sess" not in " ".join(cmd.argv)
    # No buttons at all: not the viewer's defaults either.
    cmd = build_command(config, SESSION, resolve, api_token="api-sess", commands=[])
    assert cmd.argv[-1] == "--no-default-commands"
    # Without a session there is no panel to feed.
    assert build_command(config, SESSION, resolve, commands=buttons).argv[7:] == []


def test_quick_commands_are_checked() -> None:
    assert QuickCommand.create("  ", " cmd ") == QuickCommand("cmd", "cmd")
    for label, command, error in [
        ("x", "", "Enter the command"),
        ("a=b", "cmd", "cannot contain"),
        ("x" * 41, "cmd", "40 characters"),
        ("x", "cmd\ncalc", "single line"),
        ("x", "c" * 1025, "longer than"),
    ]:
        with pytest.raises(CommandError, match=error):
            QuickCommand.create(label, command)
    # From state.json: bad entries are skipped; not a list means defaults.
    raw = [{"label": "Services", "command": "services.msc"}, {"label": "x"}, "junk"]
    assert from_json(raw) == [QuickCommand("Services", "services.msc")]
    assert from_json(None) is None
    assert from_json([]) == []


def test_viewer_buttons_are_remembered(tmp_path) -> None:
    path = tmp_path / "state.json"
    state = UiState.load(path)
    assert state.viewer_commands is None and state.commands == list(DEFAULT_COMMANDS)
    state.viewer_commands = [QuickCommand("Printers", "control printers")]
    state.save()
    again = UiState.load(path)
    assert again.commands == [QuickCommand("Printers", "control printers")]
    again.viewer_commands = []
    again.save()
    assert UiState.load(path).commands == []


def test_viewer_font_size_comes_from_the_config(tmp_path) -> None:
    base = dict(server_url="https://rmm.example.com:8443", ca_path=Path("/ca.pem"))
    resolve = fake_resolve({"rmm.example.com": "203.0.113.5"})
    cmd = build_command(Config(**base, viewer_font_size=9), SESSION, resolve)
    assert cmd.argv[7:] == ["--font-size", "9"]
    # Unset: the viewer's own default.
    assert "--font-size" not in build_command(Config(**base), SESSION, resolve).argv
    for bad in (5, 33, 9.5, True, "9"):
        with pytest.raises(ConfigError, match="viewer_font_size"):
            Config(**base, viewer_font_size=bad)
    path = tmp_path / "config.toml"
    path.write_text("viewer_font_size = 11\n")
    assert load(path).viewer_font_size == 11


def test_the_size_picked_in_a_viewer_becomes_the_default(tmp_path) -> None:
    base = dict(server_url="https://rmm.example.com:8443", ca_path=Path("/ca.pem"))
    resolve = fake_resolve({"rmm.example.com": "203.0.113.5"})
    font_file = tmp_path / "viewer.json"
    config = Config(**base, viewer_font_size=11)

    # Nothing picked yet: the config's size, and the file to remember in.
    cmd = build_command(config, SESSION, resolve, font_file=font_file)
    assert cmd.argv[7:] == ["--font-size", "11", "--remember-font-size", str(font_file)]
    # A viewer saved a size: it wins over the config.
    font_file.write_text('{"font_size": 8}\n')
    cmd = build_command(config, SESSION, resolve, font_file=font_file)
    assert cmd.argv[7:9] == ["--font-size", "8"]
    # Junk or out-of-range values are ignored.
    for junk in ("not json", '{"font_size": 99}', '{"font_size": true}', "[1]"):
        font_file.write_text(junk)
        assert build_command(config, SESSION, resolve, font_file=font_file).argv[8] == "11"
    font_file.write_text('{"font_size": 9}')
    assert build_command(Config(**base), SESSION, resolve, font_file=font_file).argv[8] == "9"


def test_quic_address_override_and_ipv6() -> None:
    config = Config(
        server_url="https://api.example.com",
        ca_path=Path("/ca.pem"),
        quic_addr="quic.example.com:5000",
    )
    cmd = build_command(config, SESSION, fake_resolve({"quic.example.com": "2001:db8::1"}))
    assert cmd.argv[2:5] == ["[2001:db8::1]:5000", "--server-name", "quic.example.com"]


def test_ipv4_is_preferred_when_a_name_has_both() -> None:
    v6 = (socket.AF_INET6, socket.SOCK_DGRAM, 17, "", ("::1", 4433, 0, 0))
    v4 = (socket.AF_INET, socket.SOCK_DGRAM, 17, "", ("127.0.0.1", 4433))
    assert pick_address([v6, v4]) == "127.0.0.1"
    assert pick_address([v6]) == "::1"
    with pytest.raises(ViewerError):
        pick_address([])


def test_viewer_requires_a_ca() -> None:
    with pytest.raises(ViewerError, match="CA certificate"):
        build_command(Config(), SESSION, fake_resolve({}))


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        ("host", ("host", 4433)),
        ("host:1", ("host", 1)),
        ("[::1]:9", ("::1", 9)),
        ("::1", ("::1", 4433)),
        ("[::1]", ("::1", 4433)),
    ],
)
def test_split_host_port(value, expected) -> None:
    assert split_host_port(value, 4433) == expected


def test_missing_viewer_binary_is_reported(tmp_path) -> None:
    with pytest.raises(ViewerError, match="not found"):
        launch(ViewerCommand(["/nonexistent/viewer"], {}), tmp_path / "v.log")


def test_launch_runs_the_viewer_detached_with_its_output_logged(tmp_path) -> None:
    script = tmp_path / "fake-viewer.py"
    script.write_text(
        "import os, sys\nprint(' '.join(sys.argv[1:]), os.environ['RMM_VIEWER_TOKEN'])\n"
    )
    log = tmp_path / "logs" / "viewer.log"
    process = launch(
        ViewerCommand([sys.executable, str(script), "--server", "x"], {"RMM_VIEWER_TOKEN": "tok"}),
        log,
    )
    assert process.wait(timeout=10) == 0
    assert log.read_text().strip() == "--server x tok"


# --- config ---------------------------------------------------------------------


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX signals")
def test_close_all_terminates_viewers_and_kills_stubborn_ones() -> None:
    sleep = "import time; time.sleep(60)"
    stubborn = (
        "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN);"
        " print(flush=True); time.sleep(60)"
    )
    polite = subprocess.Popen([sys.executable, "-c", sleep])
    ignores_term = subprocess.Popen([sys.executable, "-c", stubborn], stdout=subprocess.PIPE)
    ignores_term.stdout.readline()  # its handler is installed
    done = subprocess.Popen([sys.executable, "-c", "pass"])
    done.wait()

    close_all([polite, ignores_term, done], timeout=0.5)

    assert polite.returncode == -signal.SIGTERM
    assert ignores_term.returncode == -signal.SIGKILL
    assert done.returncode == 0
    ignores_term.stdout.close()


def test_config_file_with_relative_paths(tmp_path) -> None:
    path = tmp_path / "config.toml"
    path.write_text(
        'server_url = "https://rmm.lan:8443"\nca_path = "certs/ca.crt"\npoll_interval = 2\n'
    )
    config = load(path)
    assert config.server_url == "https://rmm.lan:8443"
    assert config.ca_path == (tmp_path / "certs" / "ca.crt").resolve()
    assert config.poll_interval == 2
    assert config.with_overrides(viewer_path="/v", ca_path=None).viewer_path == "/v"
    assert config.with_overrides(ca_path=None).ca_path == config.ca_path


@pytest.mark.parametrize(
    ("typed", "url"),
    [
        ("rmm.example.com:8443", "https://rmm.example.com:8443"),
        ("  https://rmm.example.com:8443/ ", "https://rmm.example.com:8443"),
        ("https://[::1]:8443", "https://[::1]:8443"),
    ],
)
def test_server_urls_typed_at_login_are_normalized(typed, url) -> None:
    assert normalize_server_url(typed) == url


@pytest.mark.parametrize("typed", ["", "   ", "http://rmm:8443", "https://", "ftp://x"])
def test_bad_server_urls_are_refused(typed) -> None:
    with pytest.raises(ConfigError):
        normalize_server_url(typed)


def test_switching_server_drops_a_quic_addr_for_another_host() -> None:
    config = Config(server_url="https://a.lan:8443", quic_addr="a.lan:5000")
    assert config.for_server("https://a.lan:9443").quic_addr == "a.lan:5000"
    other = config.for_server("https://b.lan:8443")
    assert (other.server_url, other.quic_addr) == ("https://b.lan:8443", None)


def test_the_last_server_is_used_unless_a_flag_names_one(tmp_path) -> None:
    path = tmp_path / "config.toml"
    path.write_text('server_url = "https://file.lan:8443"')
    state = UiState(tmp_path / "state.json", last_server="https://last.lan:8443")
    assert resolve_config(parse_args(["--config", str(path)]), state).server_url == (
        "https://last.lan:8443"
    )
    flagged = parse_args(["--config", str(path), "--server-url", "https://flag.lan:8443"])
    assert resolve_config(flagged, state).server_url == "https://flag.lan:8443"
    assert resolve_config(parse_args(["--config", str(path)]), UiState(state.path)).server_url == (
        "https://file.lan:8443"
    )
    # A broken entry (hand-edited file) is ignored.
    broken = UiState(state.path, last_server="http://nope")
    assert resolve_config(parse_args(["--config", str(path)]), broken).server_url == (
        "https://file.lan:8443"
    )


def test_ui_state_round_trips_and_tolerates_bad_files(tmp_path) -> None:
    path = tmp_path / "sub" / "state.json"
    assert UiState.load(path) == UiState(path)
    UiState(path, last_server="https://a:8443", agent_columns=["host", "ip"]).save()
    assert UiState.load(path) == UiState(path, "https://a:8443", ["host", "ip"])
    for text in ("{broken", "[1, 2]", '{"last_server": 5, "agent_columns": "host"}'):
        path.write_text(text)
        assert UiState.load(path) == UiState(path)
    # Unwritable: logged, not raised.
    blocker = tmp_path / "file"
    blocker.write_text("")
    UiState(blocker / "state.json", last_server="https://a:8443").save()


def test_missing_config_file_gives_defaults(tmp_path) -> None:
    assert load(tmp_path / "nope.toml") == Config()


@pytest.mark.parametrize(
    "text",
    ['server_url = "http://plain"', "poll_interval = 0", 'sevrer_url = "https://x"', "=bad"],
)
def test_bad_config_is_an_error(tmp_path, text) -> None:
    path = tmp_path / "config.toml"
    path.write_text(text)
    with pytest.raises(ConfigError):
        load(path)


# --- formatting -------------------------------------------------------------------

NOW = datetime(2026, 9, 30, 12, tzinfo=UTC)


def make_agent(**kw) -> Agent:
    base = dict(
        id="agt-1",
        hostname="WS-01",
        online=True,
        enrollment_state="enrolled",
        last_seen=NOW - timedelta(seconds=5),
        cpu_percent=37.4,
        mem_used_bytes=6 << 30,
        mem_total_bytes=16 << 30,
        disk_used_bytes=100 << 30,
        disk_total_bytes=400 << 30,
        uptime_secs=1,
        viewer_sessions=0,
        shell_sessions=0,
    )
    base.update(kw)
    return Agent(**base)


def plain_row(agent: Agent, columns=formatting.DEFAULT_COLUMNS) -> tuple[str, ...]:
    return tuple(str(c) for c in formatting.agent_row(agent, NOW, columns))


def test_agent_row() -> None:
    full = make_agent(
        viewer_sessions=2,
        shell_sessions=1,
        logged_in_users=("CORP\\alice", "bob"),
        local_ip="192.168.1.20",
        uptime_secs=5 * 3600 + 12 * 60,
        classification="desktop",
        os="Windows 11 Pro (build 26100)",
    )
    assert plain_row(full) == (
        "WS-01",
        "● online",
        "Desktop",
        "Windows 11 Pro (build 26100)",
        "CORP\\alice, bob",
        "192.168.1.20",
        "5h 12m",
        "–",
        "5s ago",
        "37%",
        "38% of 16.0 GiB",
        "25% of 400.0 GiB",
        "2 desktop, 1 shell",
    )
    empty = make_agent(
        hostname=None,
        online=False,
        last_seen=None,
        cpu_percent=None,
        mem_used_bytes=None,
        disk_total_bytes=0,
        logged_in_users=("stale",),
    )
    assert plain_row(empty) == (
        "agt-1",
        "● offline",
        "Other",
        "–",
        "–",  # users and uptime of an offline agent are stale
        "–",
        "–",
        "–",
        "never",
        "–",
        "–",
        "–",
        "–",
    )
    assert formatting.status(make_agent(enrollment_state="revoked")) == "revoked"
    # The WebSocket fallback is worth a mention; groups are listed.
    fallback = make_agent(transport="websocket", groups=("Branch", "Servers"))
    assert plain_row(fallback, ["status", "groups"]) == ("● online (ws)", "Branch, Servers")
    # Reported, but nobody signed in; an agent too old to report.
    assert plain_row(make_agent(logged_in_users=()), ["user"]) == ("none",)
    assert plain_row(make_agent(), ["user", "ip", "public_ip"]) == ("–", "–", "–")
    chosen = make_agent(
        classification="server", classification_override="server", remote_ip="203.0.113.9"
    )
    # An admin's choice is marked.
    assert plain_row(chosen, ["class", "id", "public_ip"]) == ("Server*", "agt-1", "203.0.113.9")


def test_status_dot_is_green_online_red_offline_grey_revoked() -> None:
    def dot(agent: Agent) -> str:
        cell = formatting.status_cell(agent)
        assert cell.plain.startswith("●")
        return str(cell.spans[0].style)

    assert dot(make_agent()) == "green"
    assert dot(make_agent(transport="websocket")) == "green"
    assert dot(make_agent(online=False)) == "red"
    assert dot(make_agent(online=False, enrollment_state="revoked")) == "bright_black"


@pytest.mark.parametrize(
    ("secs", "text"),
    [(None, "–"), (0, "0m"), (59, "0m"), (61, "1m"), (3600, "1h 0m"), (90061, "1d 1h")],
)
def test_duration(secs, text) -> None:
    assert formatting.duration(secs) == text


def test_valid_columns_drops_unknown_and_repeated_keys() -> None:
    assert formatting.valid_columns(["ip", "nope", "host", "ip"]) == ["ip", "host"]
    assert formatting.valid_columns(["nope"]) == list(formatting.DEFAULT_COLUMNS)
    assert formatting.valid_columns(None) == list(formatting.DEFAULT_COLUMNS)
    # The old Type column became Classification.
    assert formatting.valid_columns(["host", "kind"]) == ["host", "class"]
    assert set(formatting.DEFAULT_COLUMNS) <= set(formatting.COLUMNS)


def test_capabilities_decide_per_agent_and_the_role_is_the_fallback() -> None:
    from rmm_tui.api import User

    engineer = User(id=1, username="jane", role="support_engineer")
    auditor = User(id=2, username="carol", role="auditor")
    granted = make_agent(capabilities=frozenset({"desktop"}))
    assert granted.allows("desktop", engineer)
    assert not granted.allows("shell", engineer)
    # An older server sends no capabilities: the role decides.
    legacy = make_agent()
    assert legacy.allows("shell", engineer)
    assert not legacy.allows("shell", auditor)
    parsed = Agent.from_json(
        {
            "id": "agt-9",
            "enrollment_state": "enrolled",
            "online": True,
            "transport": "websocket",
            "groups": ["Branch"],
            "capabilities": ["script"],
            "logged_in_users": ["CORP\\alice"],
            "local_ip": "10.0.0.5",
            "remote_ip": "203.0.113.9",
            "device_kind": "server",
            "classification": "desktop",
            "classification_override": "desktop",
            "os": "Windows Server 2022 Datacenter (build 20348)",
        }
    )
    assert (parsed.classification, parsed.classification_override, parsed.os) == (
        "desktop",
        "desktop",
        "Windows Server 2022 Datacenter (build 20348)",
    )
    # An older server sends no classification: derived from the device kind.
    for kind, derived in (("server", "server"), ("workstation", "desktop"), (None, "other")):
        legacy_json = {"id": "agt-7", "enrollment_state": "enrolled", "device_kind": kind}
        assert Agent.from_json(legacy_json).classification == derived
    assert (parsed.logged_in_users, parsed.local_ip, parsed.remote_ip, parsed.device_kind) == (
        ("CORP\\alice",),
        "10.0.0.5",
        "203.0.113.9",
        "server",
    )
    # From an older server: unknown, not "nobody".
    assert Agent.from_json({"id": "agt-8", "enrollment_state": "enrolled"}).logged_in_users is None
    assert (parsed.transport, parsed.groups, parsed.capabilities) == (
        "websocket",
        ("Branch",),
        frozenset({"script"}),
    )


@pytest.mark.parametrize(
    ("secs", "text"),
    [
        (0, "0s ago"),
        (59, "59s ago"),
        (60, "1m ago"),
        (7200, "2h ago"),
        (3 * 86400, "3d ago"),
        (-5, "0s ago"),
    ],
)
def test_ago(secs, text) -> None:
    assert formatting.ago(NOW - timedelta(seconds=secs), NOW) == text


def test_bytes_short() -> None:
    assert formatting.bytes_short(512) == "512 B"
    assert formatting.bytes_short(1536) == "1.5 KiB"
    assert formatting.bytes_short(5 << 40) == "5.0 TiB"
