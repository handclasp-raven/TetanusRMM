"""The HTTPS API client against a mocked server."""

from __future__ import annotations

from datetime import UTC, datetime
from urllib.parse import parse_qs, urlsplit

import httpx
import pytest

from tetanus_rmm.api import ApiClient, ApiError, Forbidden, Unauthorized

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


def test_only_admins_edit_groups_and_auditors_make_no_links() -> None:
    from tetanus_rmm.api import User

    admin, engineer, auditor = (
        User(1, n, r) for n, r in (("a", "admin"), ("e", "support_engineer"), ("c", "auditor"))
    )
    assert [u.is_admin for u in (admin, engineer, auditor)] == [True, False, False]
    assert [u.can_enroll for u in (admin, engineer, auditor)] == [True, True, False]


def test_roles_decide_what_the_ui_offers() -> None:
    from tetanus_rmm.api import User

    admin = User(1, "a", "admin")
    engineer = User(2, "e", "support_engineer")
    auditor = User(3, "r", "auditor")
    assert admin.can_control and admin.can_read_audit
    assert engineer.can_control and not engineer.can_read_audit
    assert not auditor.can_control and auditor.can_read_audit


async def test_enrollment_link_sends_only_what_was_chosen(
    api: ApiClient, server: FakeServer
) -> None:
    api.token = "sess"
    server.on(
        "POST",
        "/api/enrollment-links",
        body={
            "token": "t0k",
            "expires_at": "2026-10-01T10:00:00Z",
            "download_url": "https://rmm:8443/api/download/windows-x86_64?token=t0k",
            "msi_url": "https://rmm:8443/api/download/windows-x86_64/msi?token=t0k",
            "server": "rmm:4433",
            "server_name": "rmm",
        },
    )
    link = await api.create_enrollment_link(
        ttl_secs=3600, group_ids=[2], server="rmm:4433", server_name="rmm"
    )
    assert server.body() == {
        "platform": "windows-x86_64",
        "ttl_secs": 3600,
        "group_ids": [2],
        "server": "rmm:4433",
        "server_name": "rmm",
    }
    assert link.msi_url and link.msi_url.endswith("/msi?token=t0k")
    assert (link.server, link.server_name) == ("rmm:4433", "rmm")
    await api.create_enrollment_link(platform="linux-x86_64")
    assert server.body() == {"platform": "linux-x86_64"}


async def test_deployment_keys_are_made_listed_and_revoked(
    api: ApiClient, server: FakeServer
) -> None:
    api.token = "sess"
    server.on(
        "POST",
        "/api/deployment-keys",
        body={
            "id": 7,
            "name": "Acme",
            "token": "k3y",
            "expires_at": None,
            "msi_url": "https://rmm:8443/api/download/windows-x86_64/msi?token=k3y",
            "server": "rmm:4433",
            "server_name": "rmm",
        },
    )
    made = await api.create_deployment_key(name="Acme", group_ids=[2], server="rmm:4433")
    # No lifetime: the key never expires.
    assert server.body() == {"name": "Acme", "group_ids": [2], "server": "rmm:4433"}
    assert made.expires_at is None and made.msi_url.endswith("/msi?token=k3y")
    await api.create_deployment_key(name="Acme", ttl_secs=86400)
    assert server.body() == {"name": "Acme", "ttl_secs": 86400}

    server.on(
        "GET",
        "/api/deployment-keys",
        body=[
            {
                "id": 7,
                "name": "Acme",
                "created_by": "alice",
                "created_at": "2026-10-01T10:00:00Z",
                "expires_at": None,
                "revoked_at": "2026-10-02T10:00:00Z",
                "revoked_by": "sam",
                "group_ids": [2],
                "server": "rmm:4433",
                "server_name": "rmm",
                "enrolled_count": 12,
                "last_enrolled_at": "2026-10-01T11:00:00Z",
            }
        ],
    )
    (key,) = await api.list_deployment_keys()
    assert (key.name, key.enrolled_count, key.group_ids) == ("Acme", 12, (2,))
    assert key.expires_at is None and key.revoked_by == "sam"
    assert key.last_enrolled_at is not None

    server.on("DELETE", "/api/deployment-keys/7", body={})
    await api.revoke_deployment_key(7)
    assert server.requests[-1].method == "DELETE"


async def test_quick_assist_codes_and_their_status(api: ApiClient, server: FakeServer) -> None:
    api.token = "sess"
    server.on(
        "POST",
        "/api/assist-sessions",
        body={
            "id": 7,
            "code": "482913",
            "expires_at": "2026-10-03T10:10:00Z",
            "url": "https://rmm:8443/assist",
        },
    )
    code = await api.create_assist_code()
    assert (code.id, code.code, code.spaced) == (7, "482913", "482 913")
    assert code.url == "https://rmm:8443/assist"

    server.on("GET", "/api/assist-sessions/7", body={"id": 7, "status": "waiting"})
    waiting = await api.assist_status(7)
    assert (waiting.status, waiting.agent_id) == ("waiting", None)
    assert waiting.label == "the user's computer"
    server.on(
        "GET",
        "/api/assist-sessions/7",
        body={"id": 7, "status": "connected", "agent_id": "qa-1", "hostname": "HOME-PC"},
    )
    connected = await api.assist_status(7)
    assert (connected.status, connected.agent_id, connected.label) == (
        "connected",
        "qa-1",
        "HOME-PC",
    )
    # Quick assist agents are marked as such in the agent list.
    server.on(
        "GET",
        "/api/agents",
        body=[agent_json("qa-1", assist_session_id=7), agent_json("agt-1")],
    )
    assert [a.quick_assist for a in await api.list_agents()] == [True, False]


async def test_download_goes_to_this_server_and_leaves_nothing_on_failure(
    api: ApiClient, server: FakeServer, tmp_path
) -> None:
    server.on(
        "GET",
        "/api/download/windows-x86_64/msi",
        handler=lambda req: httpx.Response(200, content=b"MSI" * 1000),
    )
    dest = tmp_path / "agent.msi"
    # The link names the server's public URL; the TUI's own server is used.
    size = await api.download(
        "https://public.example:8443/api/download/windows-x86_64/msi?token=t", dest
    )
    assert size == 3000 and dest.read_bytes() == b"MSI" * 1000
    request = server.requests[-1]
    assert request.url.host == "rmm.test" and request.url.params["token"] == "t"
    assert "authorization" not in request.headers

    server.on("GET", "/api/download/windows-x86_64/msi", status=401, body={"error": "expired"})
    other = tmp_path / "other.msi"
    with pytest.raises(Unauthorized, match="expired"):
        await api.download("https://x/api/download/windows-x86_64/msi?token=t", other)
    assert list(tmp_path.iterdir()) == [dest]


async def test_group_changes(api: ApiClient, server: FakeServer) -> None:
    api.token = "sess"
    group = {"id": 4, "name": "Branch", "description": "Leeds", "agent_ids": ["agt-1"]}
    server.on("POST", "/api/groups", status=201, body=group)
    server.on("PATCH", "/api/groups/4", body=group)
    server.on("PUT", "/api/groups/4/agents", body=group)
    server.on("DELETE", "/api/groups/4", handler=lambda _req: httpx.Response(204))
    assert (await api.create_group("Branch", "Leeds")).id == 4
    assert server.body() == {"name": "Branch", "description": "Leeds"}
    await api.update_group(4, "Branch", "Leeds office")
    assert server.body() == {"name": "Branch", "description": "Leeds office"}
    assert (await api.set_group_members(4, ["agt-1"])).agent_ids == ("agt-1",)
    assert server.body() == {"agent_ids": ["agt-1"]}
    await api.delete_group(4)
    assert server.requests[-1].method == "DELETE"


async def test_user_administration(api: ApiClient, server: FakeServer) -> None:
    api.token = "sess"
    user = {"id": 9, "username": "sam", "role": "support_engineer"}
    enrollment = {"user": user, "totp_secret": "SECRET", "otpauth_url": "otpauth://totp/x"}
    grant = {
        "id": 3,
        "user_id": 9,
        "username": "sam",
        "agent_id": None,
        "group_id": 2,
        "group_name": "Servers",
        "all_agents": False,
        "capabilities": ["desktop", "shell"],
    }
    server.on("GET", "/api/users", body=[user])
    server.on("POST", "/api/users", status=201, body=enrollment)
    server.on("PUT", "/api/users/9/role", body={**user, "role": "auditor"})
    server.on("PUT", "/api/users/9/password", body=user)
    server.on("POST", "/api/users/9/totp", body=enrollment)
    server.on("DELETE", "/api/users/9", handler=lambda _req: httpx.Response(204))
    server.on("GET", "/api/grants", body=[grant])
    server.on("POST", "/api/grants", status=201, body=grant)
    server.on("DELETE", "/api/grants/3", handler=lambda _req: httpx.Response(204))

    assert [u.username for u in await api.list_users()] == ["sam"]
    created = await api.create_user("sam", "a long password", "support_engineer")
    assert created.totp_secret == "SECRET" and created.user.id == 9
    assert server.body() == {
        "username": "sam",
        "password": "a long password",
        "role": "support_engineer",
    }
    assert (await api.set_user_role(9, "auditor")).role == "auditor"
    assert server.body() == {"role": "auditor"}
    await api.set_user_password(9, "another password")
    assert server.body() == {"password": "another password"}
    assert (await api.reset_user_totp(9)).otpauth_url == "otpauth://totp/x"
    await api.delete_user(9)
    assert server.requests[-1].method == "DELETE"

    grants = await api.list_grants(9)
    assert server.requests[-1].url.params["user_id"] == "9"
    assert grants[0].group_name == "Servers" and grants[0].capabilities == ("desktop", "shell")
    await api.list_grants()
    assert "user_id" not in server.requests[-1].url.params
    await api.create_grant(9, ["shell"], group_id=2)
    assert server.body() == {"user_id": 9, "capabilities": ["shell"], "group_id": 2}
    await api.create_grant(9, ["shell"], agent_id="agt-1")
    assert server.body() == {"user_id": 9, "capabilities": ["shell"], "agent_id": "agt-1"}
    await api.create_grant(9, ["shell"])
    assert server.body() == {"user_id": 9, "capabilities": ["shell"], "all_agents": True}
    await api.delete_grant(3)
    assert server.requests[-1].url.path == "/api/grants/3"


async def test_telemetry_history_is_parsed_and_can_be_incremental(
    api: ApiClient, server: FakeServer
) -> None:
    api.token = "t"
    server.on(
        "GET",
        "/api/agents/agt-1/telemetry",
        body={
            "step_secs": 30,
            "samples": [
                {
                    "ts": "2026-10-03T10:00:00.5Z",
                    "cpu_percent": 12.5,
                    "mem_used_bytes": 4 << 30,
                    "mem_total_bytes": 16 << 30,
                    "disk_used_bytes": 1,
                    "disk_total_bytes": 2,
                }
            ],
        },
    )
    history = await api.agent_telemetry("agt-1")
    assert history.step_secs == 30
    (only,) = history.samples
    assert only.ts == datetime(2026, 10, 3, 10, 0, 0, 500000, tzinfo=UTC)
    assert (only.cpu_percent, only.mem_total_bytes) == (12.5, 16 << 30)
    assert "since" not in server.requests[-1].url.params

    await api.agent_telemetry("agt-1", since=only.ts)
    assert server.requests[-1].url.params["since"] == "2026-10-03T10:00:00.500000Z"

    server.on("GET", "/api/agents/agt-1/telemetry", body={"samples": [{"ts": "x"}]})
    with pytest.raises(ApiError, match="malformed"):
        await api.agent_telemetry("agt-1")


async def test_agent_disks_are_parsed(api: ApiClient, server: FakeServer) -> None:
    api.token = "t"
    disk = {"name": "C:\\", "total_bytes": 100, "used_bytes": 40}
    server.on("GET", "/api/agents", body=[agent_json("agt-1", disks=[disk]), agent_json("agt-2")])
    first, second = await api.list_agents()
    assert [(d.name, d.total_bytes, d.used_bytes) for d in first.disks] == [("C:\\", 100, 40)]
    assert second.disks is None
