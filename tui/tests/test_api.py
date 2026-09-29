"""The HTTPS API client against a mocked server."""

from __future__ import annotations

from datetime import UTC, datetime
from urllib.parse import parse_qs, urlsplit

import httpx
import pytest

from rmm_tui.api import ApiClient, ApiError, Forbidden, Unauthorized

from .conftest import BASE, USER, FakeServer, agent_json


async def test_login_is_password_then_totp(api: ApiClient, server: FakeServer) -> None:
    server.on("POST", "/api/auth/login", body={"challenge_token": "chal", "expires_in_secs": 300})
    server.on(
        "POST",
        "/api/auth/totp",
        body={"session_token": "sess", "expires_at": "2026-10-01T12:00:00Z", "user": USER},
    )
    result = await api.login("jane", "hunter2hunter2", "123456")

    assert server.body(0) == {"username": "jane", "password": "hunter2hunter2"}
    assert server.body(1) == {"challenge_token": "chal", "code": "123456"}
    # Neither step sends a bearer token.
    assert all("authorization" not in r.headers for r in server.requests)
    assert result.token == "sess"
    assert result.expires_at == datetime(2026, 10, 1, 12, tzinfo=UTC)
    assert result.user.username == "jane" and result.user.can_control
    assert api.token is None, "login does not sign the client in by itself"


async def test_login_errors_carry_the_servers_message(api: ApiClient, server: FakeServer) -> None:
    server.on("POST", "/api/auth/login", body={"challenge_token": "c", "expires_in_secs": 1})
    server.on("POST", "/api/auth/totp", status=401, body={"error": "invalid TOTP code"})
    with pytest.raises(Unauthorized, match="invalid TOTP code") as err:
        await api.login("jane", "pw", "000000")
    assert err.value.status == 401


async def test_authenticated_calls_send_the_bearer_token(
    api: ApiClient, server: FakeServer
) -> None:
    server.on("GET", "/api/me", body=USER)
    with pytest.raises(Unauthorized):
        await api.me()  # not signed in: nothing is sent
    assert server.requests == []

    api.token = "sess"
    user = await api.me()
    assert server.requests[-1].headers["authorization"] == "Bearer sess"
    assert user.role == "support_engineer"


async def test_agent_list_is_parsed(api: ApiClient, server: FakeServer) -> None:
    api.token = "t"
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json("agt-1", viewer_sessions=1, shell_sessions=2),
            agent_json(
                "agt-2",
                hostname=None,
                online=False,
                last_seen=None,
                cpu_percent=None,
                mem_used_bytes=None,
            ),
        ],
    )
    first, second = await api.list_agents()
    assert first.label == "host-agt-1"
    assert first.long_label == "host-agt-1 [1]"
    assert first.online and first.cpu_percent == 12.5
    assert (first.viewer_sessions, first.shell_sessions) == (1, 2)
    assert first.last_seen == datetime(2026, 9, 30, 10, tzinfo=UTC)
    # No hostname yet (older agent): fall back to the id.
    assert second.label == second.long_label == "agt-2"
    assert not second.online and second.last_seen is None and second.cpu_percent is None


async def test_older_server_without_live_fields_still_parses(
    api: ApiClient, server: FakeServer
) -> None:
    api.token = "t"
    old = agent_json("agt-1")
    for key in ("hostname", "online", "viewer_sessions", "shell_sessions"):
        del old[key]
    server.on("GET", "/api/agents", body=[old])
    (agent,) = await api.list_agents()
    assert agent.label == "agt-1" and not agent.online and agent.shell_sessions == 0


async def test_viewer_session_and_forbidden(api: ApiClient, server: FakeServer) -> None:
    api.token = "t"
    server.on(
        "POST",
        "/api/agents/agt-1/viewer-sessions",
        body={
            "token": "view",
            "expires_at": "2026-09-30T10:01:00Z",
            "agent_id": "agt-1",
            "online": True,
        },
    )
    session = await api.create_viewer_session("agt-1")
    assert session.token == "view" and session.online

    server.on("POST", "/api/agents/agt-2/viewer-sessions", status=403, body={"error": "forbidden"})
    with pytest.raises(Forbidden):
        await api.create_viewer_session("agt-2")


async def test_run_script_posts_targets_and_waits_long_enough(
    api: ApiClient, server: FakeServer
) -> None:
    api.token = "t"
    report = {"run_id": 3, "summary": {}, "results": []}
    server.on("POST", "/api/script-runs", body=report)

    assert await api.run_script(["a", "b"], "hostname", 60) == report
    assert server.body() == {"agent_ids": ["a", "b"], "script": "hostname", "timeout_secs": 60}
    # The HTTP timeout outlasts the script's own.
    assert server.requests[-1].extensions["timeout"]["read"] > 60

    await api.run_script(["a"], "hostname")
    assert "timeout_secs" not in server.body()
    assert server.requests[-1].extensions["timeout"]["read"] > 300


async def test_audit_endpoints(api: ApiClient, server: FakeServer) -> None:
    api.token = "t"
    server.on(
        "GET",
        "/api/audit",
        body=[
            {
                "id": 9,
                "ts": "2026-09-30T10:00:00.123456Z",
                "actor": "jane",
                "action": "script.run",
                "target": None,
                "detail": {"agents": ["a"]},
                "prev_hash": "00",
                "hash": "11",
            }
        ],
    )
    server.on("GET", "/api/audit/verify", body={"status": "valid", "entries": 9})
    (entry,) = await api.audit(limit=5)
    assert parse_qs(urlsplit(str(server.requests[-1].url)).query) == {"limit": ["5"]}
    assert entry.action == "script.run" and entry.detail == {"agents": ["a"]}
    assert await api.verify_audit() == {"status": "valid", "entries": 9}


async def test_unreachable_server_is_an_api_error_with_status_0() -> None:
    def refuse(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("connection refused", request=request)

    api = ApiClient(BASE, transport=httpx.MockTransport(refuse), token="t")
    with pytest.raises(ApiError) as err:
        await api.list_agents()
    assert err.value.status == 0 and "cannot reach server" in err.value.message


async def test_non_json_error_body_falls_back_to_the_status(
    api: ApiClient, server: FakeServer
) -> None:
    api.token = "t"
    server.on("GET", "/api/agents", handler=lambda r: httpx.Response(502, text="bad gateway"))
    with pytest.raises(ApiError) as err:
        await api.list_agents()
    assert err.value.status == 502 and err.value.message == "Bad Gateway"


def test_shell_url_is_a_websocket_url_with_the_size(api: ApiClient) -> None:
    assert (
        api.shell_url("agt-1", 132, 43)
        == "wss://rmm.test:8443/api/agents/agt-1/shell?cols=132&rows=43"
    )
    # Ids are path-escaped.
    assert "/api/agents/a%2Fb/shell" in api.shell_url("a/b", 80, 24)


def test_bad_ca_path_is_reported(tmp_path) -> None:
    with pytest.raises(ApiError, match="cannot load CA certificate"):
        ApiClient(BASE, tmp_path / "missing.pem")


def test_roles_decide_what_the_ui_offers() -> None:
    from rmm_tui.api import User

    admin = User(1, "a", "admin")
    engineer = User(2, "e", "support_engineer")
    auditor = User(3, "r", "auditor")
    assert admin.can_control and admin.can_read_audit
    assert engineer.can_control and not engineer.can_read_audit
    assert not auditor.can_control and auditor.can_read_audit
