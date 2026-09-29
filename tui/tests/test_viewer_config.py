"""Viewer launch command, config loading and table formatting."""

from __future__ import annotations

import socket
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from rmm_tui import formatting
from rmm_tui.api import Agent, ViewerSession
from rmm_tui.config import Config, ConfigError, load
from rmm_tui.viewer import (
    ViewerCommand,
    ViewerError,
    build_command,
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


def test_agent_row() -> None:
    assert formatting.agent_row(make_agent(viewer_sessions=2, shell_sessions=1), NOW) == (
        "WS-01",
        "online",
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
    )
    assert formatting.agent_row(empty, NOW) == (
        "agt-1",
        "offline",
        "never",
        "–",
        "–",
        "–",
        "–",
    )
    assert formatting.status(make_agent(enrollment_state="revoked")) == "revoked"


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
