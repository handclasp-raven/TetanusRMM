from __future__ import annotations

import json
from collections.abc import Callable
from typing import Any

import httpx
import keyring.errors
import pytest

from tetanus_rmm.api import ApiClient

BASE = "https://rmm.test:8443"


class MemoryKeyring:
    """Stand-in for the OS keyring."""

    def __init__(self) -> None:
        self.entries: dict[tuple[str, str], str] = {}
        self.broken = False

    def _check(self) -> None:
        if self.broken:
            raise keyring.errors.NoKeyringError("no backend")

    def get_password(self, service: str, username: str) -> str | None:
        self._check()
        return self.entries.get((service, username))

    def set_password(self, service: str, username: str, password: str) -> None:
        self._check()
        self.entries[(service, username)] = password

    def delete_password(self, service: str, username: str) -> None:
        self._check()
        if (service, username) not in self.entries:
            raise keyring.errors.PasswordDeleteError("not found")
        del self.entries[(service, username)]


class FakeServer:
    """Routes requests to handlers and records them."""

    def __init__(self) -> None:
        self.routes: dict[tuple[str, str], Callable[[httpx.Request], httpx.Response]] = {}
        self.requests: list[httpx.Request] = []

    def on(
        self,
        method: str,
        path: str,
        status: int = 200,
        body: Any = None,
        handler: Callable[[httpx.Request], httpx.Response] | None = None,
    ) -> None:
        self.routes[(method, path)] = handler or (lambda _req: httpx.Response(status, json=body))

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        route = self.routes.get((request.method, request.url.path))
        if route is None:
            return httpx.Response(404, json={"error": "not found"})
        return route(request)

    def body(self, index: int = -1) -> Any:
        return json.loads(self.requests[index].content)


@pytest.fixture
def server() -> FakeServer:
    return FakeServer()


@pytest.fixture
def api(server: FakeServer) -> ApiClient:
    return ApiClient(BASE, transport=httpx.MockTransport(server))


@pytest.fixture
def memory_keyring() -> MemoryKeyring:
    return MemoryKeyring()


USER = {"id": 7, "username": "jane", "role": "support_engineer"}


def agent_json(agent_id: str, **overrides: Any) -> dict[str, Any]:
    base = {
        "id": agent_id,
        "enrollment_state": "enrolled",
        "cert_fingerprint": "ab" * 32,
        "last_seen": "2026-09-30T10:00:00Z",
        "cpu_percent": 12.5,
        "mem_used_bytes": 4 << 30,
        "mem_total_bytes": 16 << 30,
        "disk_used_bytes": 100 << 30,
        "disk_total_bytes": 400 << 30,
        "uptime_secs": 3600,
        "telemetry_at": "2026-09-30T10:00:00Z",
        "created_at": "2026-09-01T00:00:00Z",
        "device_kind": "workstation",
        "hostname": f"host-{agent_id}",
        "online": True,
        "viewer_sessions": 0,
        "shell_sessions": 0,
    }
    base.update(overrides)
    return base
