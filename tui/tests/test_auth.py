"""Staying signed in: the token in the keyring, validated on start."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta

import httpx
import pytest

from rmm_tui.api import ApiClient, ApiError
from rmm_tui.auth import KEYRING_SERVICE, SessionManager, StoredSession, TokenStore

from .conftest import BASE, USER, FakeServer, MemoryKeyring

NOW = datetime(2026, 9, 30, 12, tzinfo=UTC)
LATER = NOW + timedelta(hours=8)


def manager(api: ApiClient, kr: MemoryKeyring) -> SessionManager:
    return SessionManager(api, TokenStore(BASE, kr), clock=lambda: NOW)


def saved(kr: MemoryKeyring) -> dict | None:
    raw = kr.entries.get((KEYRING_SERVICE, BASE))
    return json.loads(raw) if raw else None


def test_store_round_trips_per_server(memory_keyring: MemoryKeyring) -> None:
    a = TokenStore(BASE + "/", memory_keyring)  # trailing slash is the same server
    b = TokenStore("https://other:8443", memory_keyring)
    assert a.load() is None
    session = StoredSession(token="tok", expires_at=LATER, username="jane")
    assert a.save(session)
    assert TokenStore(BASE, memory_keyring).load() == session
    assert b.load() is None
    a.clear()
    assert a.load() is None
    a.clear()  # clearing twice is fine


@pytest.mark.parametrize(
    "raw",
    [
        "not json",
        "{}",
        json.dumps({"token": "", "expires_at": LATER.isoformat(), "username": "x"}),
        json.dumps({"token": "t", "expires_at": "yesterday", "username": "x"}),
    ],
)
def test_malformed_entries_are_ignored(memory_keyring: MemoryKeyring, raw: str) -> None:
    memory_keyring.entries[(KEYRING_SERVICE, BASE)] = raw
    assert TokenStore(BASE, memory_keyring).load() is None


def test_unavailable_keyring_degrades_gracefully(memory_keyring: MemoryKeyring) -> None:
    memory_keyring.broken = True
    store = TokenStore(BASE, memory_keyring)
    assert store.load() is None
    assert not store.save(StoredSession("t", LATER, "jane"))
    store.clear()


def login_routes(server: FakeServer) -> None:
    server.on("POST", "/api/auth/login", body={"challenge_token": "c", "expires_in_secs": 300})
    server.on(
        "POST",
        "/api/auth/totp",
        body={"session_token": "sess", "expires_at": LATER.isoformat(), "user": USER},
    )


async def test_login_saves_the_token_and_a_restart_resumes_it(
    api: ApiClient, server: FakeServer, memory_keyring: MemoryKeyring
) -> None:
    login_routes(server)
    first = manager(api, memory_keyring)
    user = await first.login("jane", "pw", "123456")
    assert user.username == "jane" and first.persisted
    assert saved(memory_keyring) == {
        "token": "sess",
        "expires_at": LATER.isoformat(),
        "username": "jane",
    }
    assert "sess" not in json.dumps(server.body(0)), "token never sent at login"

    # "Restart": a fresh client and manager, same keyring.
    server.on("GET", "/api/me", body=USER)
    fresh = ApiClient(BASE, transport=httpx.MockTransport(server))
    second = manager(fresh, memory_keyring)
    assert (await second.restore()).username == "jane"
    assert fresh.token == "sess"
    assert server.requests[-1].headers["authorization"] == "Bearer sess"


async def test_restore_without_a_saved_session(
    api: ApiClient, server: FakeServer, memory_keyring
) -> None:
    assert await manager(api, memory_keyring).restore() is None
    assert server.requests == []


async def test_locally_expired_token_is_dropped_without_asking_the_server(
    api: ApiClient, server: FakeServer, memory_keyring: MemoryKeyring
) -> None:
    TokenStore(BASE, memory_keyring).save(StoredSession("old", NOW - timedelta(seconds=1), "jane"))
    assert await manager(api, memory_keyring).restore() is None
    assert server.requests == []
    assert saved(memory_keyring) is None


async def test_token_rejected_by_the_server_is_dropped(
    api: ApiClient, server: FakeServer, memory_keyring: MemoryKeyring
) -> None:
    TokenStore(BASE, memory_keyring).save(StoredSession("revoked", LATER, "jane"))
    server.on("GET", "/api/me", status=401, body={"error": "invalid or expired session"})
    m = manager(api, memory_keyring)
    assert await m.restore() is None
    assert saved(memory_keyring) is None
    assert api.token is None and m.user is None


async def test_unreachable_server_keeps_the_token(memory_keyring: MemoryKeyring) -> None:
    def down(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("down", request=request)

    TokenStore(BASE, memory_keyring).save(StoredSession("sess", LATER, "jane"))
    api = ApiClient(BASE, transport=httpx.MockTransport(down))
    with pytest.raises(ApiError):
        await manager(api, memory_keyring).restore()
    assert saved(memory_keyring)["token"] == "sess", "a network blip does not sign out"


async def test_logout_revokes_and_forgets_even_if_the_server_fails(
    api: ApiClient, server: FakeServer, memory_keyring: MemoryKeyring
) -> None:
    login_routes(server)
    m = manager(api, memory_keyring)
    await m.login("jane", "pw", "123456")
    server.on("POST", "/api/auth/logout", status=500, body={"error": "internal error"})
    await m.logout()
    assert server.requests[-1].url.path == "/api/auth/logout"
    assert server.requests[-1].headers["authorization"] == "Bearer sess"
    assert saved(memory_keyring) is None and api.token is None and m.user is None


async def test_login_when_the_keyring_is_unavailable_still_signs_in(
    api: ApiClient, server: FakeServer, memory_keyring: MemoryKeyring
) -> None:
    login_routes(server)
    memory_keyring.broken = True
    m = manager(api, memory_keyring)
    await m.login("jane", "pw", "123456")
    assert api.token == "sess" and not m.persisted


async def test_expired_mid_session_forgets_the_token(
    api: ApiClient, server: FakeServer, memory_keyring: MemoryKeyring
) -> None:
    login_routes(server)
    m = manager(api, memory_keyring)
    await m.login("jane", "pw", "123456")
    m.expired()
    assert saved(memory_keyring) is None and api.token is None
