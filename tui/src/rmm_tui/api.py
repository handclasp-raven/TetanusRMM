"""Client for the server's HTTPS API (see ``crates/server/src/api.rs``).

Everything goes through the server: the agent list, viewer-session tokens,
the script runner and the interactive shell WebSocket. The TUI never talks
to an agent directly and never needs the viewer for shells or scripts.
"""

from __future__ import annotations

import json
import os
import ssl
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any
from urllib.parse import quote, urlencode, urlsplit

import httpx

#: Roles that may control agents: remote desktop, shell, scripts. Support
#: engineers only where their grants allow (see :meth:`Agent.allows`).
CONTROL_ROLES = frozenset({"admin", "support_engineer"})

#: Capability names, as the server grants them per agent.
DESKTOP = "desktop"
SHELL = "shell"
SCRIPT = "script"
FILE_TRANSFER = "file_transfer"
#: Every capability, in the order the TUI offers them.
CAPABILITIES = (DESKTOP, SHELL, SCRIPT, FILE_TRANSFER)
#: Every role, in the order the TUI offers them.
ROLES = ("admin", "support_engineer", "auditor")
#: Shortest password the server accepts.
MIN_PASSWORD_LEN = 12
#: Roles that may read the audit log.
AUDIT_ROLES = frozenset({"admin", "auditor"})
#: Roles that may make download links for new agents.
ENROLL_ROLES = frozenset({"admin", "support_engineer"})

#: What an agent is, in the order the TUI offers them.
CLASSIFICATIONS = ("server", "desktop", "other")

#: Extra seconds allowed for a script run beyond its own timeout (the
#: server waits up to 30 s past it for the agent's answer).
SCRIPT_REPLY_GRACE = 45.0
DEFAULT_TIMEOUT = 15.0


class ApiError(Exception):
    """The server refused a request, or could not be reached (status 0)."""

    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


class Unauthorized(ApiError):
    """401: bad credentials, or the session is invalid or expired."""


class Forbidden(ApiError):
    """403: the user's role does not allow this (the server audits it)."""


def parse_time(value: str | None) -> datetime | None:
    if not value:
        return None
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


@dataclass(frozen=True)
class User:
    id: int
    username: str
    role: str

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> User:
        return cls(id=d["id"], username=d["username"], role=d["role"])

    @property
    def can_control(self) -> bool:
        """Launch remote desktop, open shells, run scripts."""
        return self.role in CONTROL_ROLES

    @property
    def can_read_audit(self) -> bool:
        return self.role in AUDIT_ROLES

    @property
    def can_enroll(self) -> bool:
        """Make download links (and MSIs) for new agents."""
        return self.role in ENROLL_ROLES

    @property
    def is_admin(self) -> bool:
        """Edit groups, put new agents straight into them, and manage users."""
        return self.role == "admin"


@dataclass(frozen=True)
class TotpEnrollment:
    """A user's new TOTP secret, for their authenticator app. The server
    shows it once: on creating the user, or resetting their TOTP."""

    user: User
    totp_secret: str
    otpauth_url: str

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> TotpEnrollment:
        return cls(
            user=User.from_json(d["user"]),
            totp_secret=d["totp_secret"],
            otpauth_url=d["otpauth_url"],
        )


@dataclass(frozen=True)
class Grant:
    """What a support engineer may do, and where: on one agent, a group's
    members, or every agent."""

    id: int
    user_id: int
    agent_id: str | None
    group_id: int | None
    group_name: str | None
    all_agents: bool
    capabilities: tuple[str, ...]

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> Grant:
        return cls(
            id=int(d["id"]),
            user_id=int(d["user_id"]),
            agent_id=d.get("agent_id"),
            group_id=d.get("group_id"),
            group_name=d.get("group_name"),
            all_agents=bool(d.get("all_agents", False)),
            capabilities=tuple(d.get("capabilities") or ()),
        )


@dataclass(frozen=True)
class LoginResult:
    token: str
    expires_at: datetime
    user: User


@dataclass(frozen=True)
class Agent:
    id: str
    hostname: str | None
    online: bool
    enrollment_state: str
    last_seen: datetime | None
    cpu_percent: float | None
    mem_used_bytes: int | None
    mem_total_bytes: int | None
    disk_used_bytes: int | None
    disk_total_bytes: int | None
    uptime_secs: int | None
    viewer_sessions: int
    shell_sessions: int
    #: ``"quic"`` or ``"websocket"`` (UDP blocked) while online.
    transport: str | None = None
    groups: tuple[str, ...] = ()
    #: What the signed-in user may do on this agent. ``None`` from servers
    #: older than per-agent grants: then the role decides.
    capabilities: frozenset[str] | None = None
    #: ``"workstation"`` or ``"server"``, once reported.
    device_kind: str | None = None
    #: Users signed in to the machine, as last reported. ``None`` if the
    #: agent (or server) is too old to report them.
    logged_in_users: tuple[str, ...] | None = None
    #: The agent's own address (its LAN address behind NAT).
    local_ip: str | None = None
    #: The address the server sees it connect from.
    remote_ip: str | None = None
    #: One of :data:`CLASSIFICATIONS`: an admin's choice, else from the
    #: device kind.
    classification: str = "other"
    #: What an admin chose, if anything (``None``: derived).
    classification_override: str | None = None
    #: Operating system and version, as last reported.
    os: str | None = None

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> Agent:
        caps = d.get("capabilities")
        users = d.get("logged_in_users")
        return cls(
            id=d["id"],
            hostname=d.get("hostname"),
            online=bool(d.get("online", False)),
            enrollment_state=d.get("enrollment_state", "enrolled"),
            last_seen=parse_time(d.get("last_seen")),
            cpu_percent=d.get("cpu_percent"),
            mem_used_bytes=d.get("mem_used_bytes"),
            mem_total_bytes=d.get("mem_total_bytes"),
            disk_used_bytes=d.get("disk_used_bytes"),
            disk_total_bytes=d.get("disk_total_bytes"),
            uptime_secs=d.get("uptime_secs"),
            viewer_sessions=int(d.get("viewer_sessions", 0)),
            shell_sessions=int(d.get("shell_sessions", 0)),
            transport=d.get("transport"),
            groups=tuple(d.get("groups") or ()),
            capabilities=frozenset(caps) if caps is not None else None,
            device_kind=d.get("device_kind"),
            logged_in_users=tuple(users) if users is not None else None,
            local_ip=d.get("local_ip"),
            remote_ip=d.get("remote_ip"),
            classification=d.get("classification") or derived_classification(d.get("device_kind")),
            classification_override=d.get("classification_override"),
            os=d.get("os"),
        )

    def allows(self, capability: str, user: User | None) -> bool:
        """Whether ``user`` may use ``capability`` here."""
        if self.capabilities is not None:
            return capability in self.capabilities
        return bool(user and user.can_control)

    @property
    def label(self) -> str:
        """Hostname if known, else the agent id."""
        return self.hostname or self.id

    @property
    def long_label(self) -> str:
        """Hostname plus a short id, to tell apart machines that share a
        hostname (e.g. cloned VMs)."""
        if not self.hostname:
            return self.id
        return f"{self.hostname} [{self.id.removeprefix('agt-')[:6]}]"


def derived_classification(device_kind: str | None) -> str:
    """What the server derives from a device kind (for older servers that
    send no classification)."""
    return {"server": "server", "workstation": "desktop"}.get(device_kind or "", "other")


@dataclass(frozen=True)
class Group:
    """An agent group (members limited to agents the user can see)."""

    id: int
    name: str
    description: str
    agent_ids: tuple[str, ...]

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> Group:
        return cls(
            id=int(d["id"]),
            name=d["name"],
            description=d.get("description", ""),
            agent_ids=tuple(d.get("agent_ids") or ()),
        )


@dataclass(frozen=True)
class EnrollmentLink:
    """A single-use download link for a new agent."""

    token: str
    expires_at: datetime
    download_url: str
    #: Windows only: an MSI that installs and enrolls the agent unattended.
    msi_url: str | None
    #: Where the agent connects (``host:port``), and the TLS name it checks.
    server: str
    server_name: str

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> EnrollmentLink:
        return cls(
            token=d["token"],
            expires_at=parse_time(d["expires_at"]),  # type: ignore[arg-type]
            download_url=d["download_url"],
            msi_url=d.get("msi_url"),
            server=d.get("server", ""),
            server_name=d.get("server_name", ""),
        )


@dataclass(frozen=True)
class ViewerSession:
    """A short-lived, single-use token for the viewer."""

    token: str
    expires_at: datetime
    agent_id: str
    online: bool

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> ViewerSession:
        return cls(
            token=d["token"],
            expires_at=parse_time(d["expires_at"]),  # type: ignore[arg-type]
            agent_id=d["agent_id"],
            online=bool(d["online"]),
        )


@dataclass(frozen=True)
class AuditEntry:
    id: int
    ts: datetime
    actor: str
    action: str
    target: str | None
    detail: Any

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> AuditEntry:
        return cls(
            id=d["id"],
            ts=parse_time(d["ts"]),  # type: ignore[arg-type]
            actor=d["actor"],
            action=d["action"],
            target=d.get("target"),
            detail=d.get("detail"),
        )


def ssl_context(ca_path: Path | None) -> ssl.SSLContext:
    """Verify the server: against ``ca_path`` if given, else the system store."""
    if ca_path is None:
        return ssl.create_default_context()
    try:
        return ssl.create_default_context(cafile=str(ca_path))
    except (OSError, ssl.SSLError) as e:
        raise ApiError(0, f"cannot load CA certificate {ca_path}: {e}") from e


def _error_message(response: httpx.Response) -> str:
    try:
        body = response.json()
        if isinstance(body, dict) and isinstance(body.get("error"), str):
            return body["error"]
    except ValueError:
        pass
    return response.reason_phrase or f"HTTP {response.status_code}"


def error_for(status: int, message: str) -> ApiError:
    if status == 401:
        return Unauthorized(status, message)
    if status == 403:
        return Forbidden(status, message or "forbidden")
    return ApiError(status, message)


class ApiClient:
    """Async client for one server. Set :attr:`token` once logged in."""

    def __init__(
        self,
        base_url: str,
        ca_path: Path | None = None,
        *,
        token: str | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.ca_path = ca_path
        self.token = token
        # With a test transport there is no TLS to set up.
        verify: ssl.SSLContext | bool = True if transport else ssl_context(ca_path)
        self._http = httpx.AsyncClient(
            base_url=self.base_url,
            verify=verify,
            transport=transport,
            timeout=DEFAULT_TIMEOUT,
        )

    async def aclose(self) -> None:
        await self._http.aclose()

    async def _request(
        self,
        method: str,
        path: str,
        *,
        auth: bool = True,
        timeout: float | None = None,
        **kwargs: Any,
    ) -> httpx.Response:
        headers = kwargs.pop("headers", {})
        if auth:
            if not self.token:
                raise Unauthorized(401, "not signed in")
            headers["Authorization"] = f"Bearer {self.token}"
        if timeout is not None:
            kwargs["timeout"] = timeout
        try:
            response = await self._http.request(method, path, headers=headers, **kwargs)
        except httpx.HTTPError as e:
            raise ApiError(0, f"cannot reach server: {e}") from e
        if response.is_error:
            raise error_for(response.status_code, _error_message(response))
        return response

    # --- auth -------------------------------------------------------------

    async def login(self, username: str, password: str, code: str) -> LoginResult:
        """Password, then TOTP. Does not set :attr:`token`."""
        challenge = (
            await self._request(
                "POST",
                "/api/auth/login",
                auth=False,
                json={"username": username, "password": password},
            )
        ).json()["challenge_token"]
        grant = (
            await self._request(
                "POST",
                "/api/auth/totp",
                auth=False,
                json={"challenge_token": challenge, "code": code},
            )
        ).json()
        return LoginResult(
            token=grant["session_token"],
            expires_at=parse_time(grant["expires_at"]),  # type: ignore[arg-type]
            user=User.from_json(grant["user"]),
        )

    async def me(self) -> User:
        return User.from_json((await self._request("GET", "/api/me")).json())

    async def logout(self) -> None:
        await self._request("POST", "/api/auth/logout")

    # --- agents -----------------------------------------------------------

    async def list_agents(self) -> list[Agent]:
        return [Agent.from_json(a) for a in (await self._request("GET", "/api/agents")).json()]

    async def set_classification(self, agent_id: str, classification: str | None) -> str:
        """Classify an agent (admins only); ``None`` returns it to the
        derived classification. Returns the one now in effect."""
        response = await self._request(
            "PUT",
            f"/api/agents/{quote(agent_id, safe='')}/classification",
            json={"classification": classification},
        )
        return response.json()["classification"]

    async def list_groups(self) -> list[Group]:
        return [Group.from_json(g) for g in (await self._request("GET", "/api/groups")).json()]

    # --- groups -----------------------------------------------------------

    async def create_group(self, name: str, description: str = "") -> Group:
        response = await self._request(
            "POST", "/api/groups", json={"name": name, "description": description}
        )
        return Group.from_json(response.json())

    async def update_group(self, group_id: int, name: str, description: str) -> Group:
        response = await self._request(
            "PATCH", f"/api/groups/{group_id}", json={"name": name, "description": description}
        )
        return Group.from_json(response.json())

    async def delete_group(self, group_id: int) -> None:
        await self._request("DELETE", f"/api/groups/{group_id}")

    async def set_group_members(self, group_id: int, agent_ids: list[str]) -> Group:
        response = await self._request(
            "PUT", f"/api/groups/{group_id}/agents", json={"agent_ids": agent_ids}
        )
        return Group.from_json(response.json())

    # --- users (admins only) ----------------------------------------------

    async def list_users(self) -> list[User]:
        return [User.from_json(u) for u in (await self._request("GET", "/api/users")).json()]

    async def create_user(self, username: str, password: str, role: str) -> TotpEnrollment:
        response = await self._request(
            "POST", "/api/users", json={"username": username, "password": password, "role": role}
        )
        return TotpEnrollment.from_json(response.json())

    async def set_user_role(self, user_id: int, role: str) -> User:
        response = await self._request("PUT", f"/api/users/{user_id}/role", json={"role": role})
        return User.from_json(response.json())

    async def set_user_password(self, user_id: int, password: str) -> User:
        """Also ends the user's sessions (not the one making the change)."""
        response = await self._request(
            "PUT", f"/api/users/{user_id}/password", json={"password": password}
        )
        return User.from_json(response.json())

    async def reset_user_totp(self, user_id: int) -> TotpEnrollment:
        """A new secret: the user's old authenticator entry stops working."""
        response = await self._request("POST", f"/api/users/{user_id}/totp")
        return TotpEnrollment.from_json(response.json())

    async def delete_user(self, user_id: int) -> None:
        await self._request("DELETE", f"/api/users/{user_id}")

    async def list_grants(self, user_id: int | None = None) -> list[Grant]:
        """Everyone's grants, or one user's."""
        params = {} if user_id is None else {"user_id": user_id}
        response = await self._request("GET", "/api/grants", params=params)
        return [Grant.from_json(g) for g in response.json()]

    async def create_grant(
        self,
        user_id: int,
        capabilities: list[str],
        *,
        agent_id: str | None = None,
        group_id: int | None = None,
    ) -> Grant:
        """Grant ``capabilities`` on one agent, on a group's members, or
        (given neither) on every agent."""
        body: dict[str, Any] = {"user_id": user_id, "capabilities": capabilities}
        if agent_id is not None:
            body["agent_id"] = agent_id
        elif group_id is not None:
            body["group_id"] = group_id
        else:
            body["all_agents"] = True
        return Grant.from_json((await self._request("POST", "/api/grants", json=body)).json())

    async def delete_grant(self, grant_id: int) -> None:
        await self._request("DELETE", f"/api/grants/{grant_id}")

    # --- new agents -------------------------------------------------------

    async def create_enrollment_link(
        self,
        *,
        platform: str = "windows-x86_64",
        ttl_secs: int | None = None,
        group_ids: list[int] | None = None,
        server: str | None = None,
        server_name: str | None = None,
    ) -> EnrollmentLink:
        body: dict[str, Any] = {"platform": platform}
        if ttl_secs is not None:
            body["ttl_secs"] = ttl_secs
        if group_ids:
            body["group_ids"] = group_ids
        if server:
            body["server"] = server
        if server_name:
            body["server_name"] = server_name
        response = await self._request("POST", "/api/enrollment-links", json=body)
        return EnrollmentLink.from_json(response.json())

    async def download(self, url: str, dest: Path) -> int:
        """Save a download link (its token is in the URL) to ``dest`` via
        this client's server, whatever host the link names. Returns the
        size. Written to a temporary file first, so a failed download
        leaves nothing behind."""
        parts = urlsplit(url)
        path = parts.path + (f"?{parts.query}" if parts.query else "")
        tmp = dest.with_name(dest.name + ".part")
        try:
            async with self._http.stream("GET", path, timeout=120) as response:
                if response.is_error:
                    await response.aread()
                    raise error_for(response.status_code, _error_message(response))
                size = 0
                with tmp.open("wb") as f:
                    async for chunk in response.aiter_bytes():
                        f.write(chunk)
                        size += len(chunk)
            os.replace(tmp, dest)
            return size
        except httpx.HTTPError as e:
            raise ApiError(0, f"download failed: {e}") from e
        except OSError as e:
            raise ApiError(0, f"cannot write {dest}: {e}") from e
        finally:
            tmp.unlink(missing_ok=True)

    async def create_viewer_session(self, agent_id: str) -> ViewerSession:
        response = await self._request(
            "POST", f"/api/agents/{quote(agent_id, safe='')}/viewer-sessions"
        )
        return ViewerSession.from_json(response.json())

    # --- script runner ----------------------------------------------------

    async def run_script(
        self,
        agent_ids: list[str],
        script: str,
        timeout_secs: int | None = None,
        group_ids: list[int] | None = None,
    ) -> dict[str, Any]:
        """Run ``script`` on every agent (and every member of ``group_ids``,
        resolved by the server at run time) and wait for all results (the
        raw report; see :mod:`rmm_tui.scripts`)."""
        body: dict[str, Any] = {"agent_ids": agent_ids, "script": script}
        if group_ids:
            body["group_ids"] = group_ids
        if timeout_secs is not None:
            body["timeout_secs"] = timeout_secs
        wait = (timeout_secs or 300) + SCRIPT_REPLY_GRACE
        response = await self._request("POST", "/api/script-runs", json=body, timeout=wait)
        return response.json()

    # --- audit ------------------------------------------------------------

    async def audit(self, limit: int = 100) -> list[AuditEntry]:
        response = await self._request("GET", "/api/audit", params={"limit": limit})
        return [AuditEntry.from_json(e) for e in response.json()]

    async def verify_audit(self) -> dict[str, Any]:
        """``{"status": "valid", "entries": n}`` or ``{"status": "broken", ...}``."""
        return (await self._request("GET", "/api/audit/verify", timeout=120)).json()

    # --- shell ------------------------------------------------------------

    def shell_url(self, agent_id: str, cols: int, rows: int) -> str:
        base = self.base_url.replace("https://", "wss://", 1)
        query = urlencode({"cols": cols, "rows": rows})
        return f"{base}/api/agents/{quote(agent_id, safe='')}/shell?{query}"

    async def open_shell(self, agent_id: str, cols: int, rows: int) -> ShellConnection:
        """Start PowerShell on the agent and attach to it."""
        from websockets.asyncio.client import connect
        from websockets.exceptions import InvalidStatus, WebSocketException

        if not self.token:
            raise Unauthorized(401, "not signed in")
        try:
            ws = await connect(
                self.shell_url(agent_id, cols, rows),
                ssl=ssl_context(self.ca_path),
                additional_headers={"Authorization": f"Bearer {self.token}"},
                max_size=2**22,
                open_timeout=30,
            )
        except InvalidStatus as e:
            # The server starts the shell before upgrading, so refusals
            # (offline agent, forbidden role) arrive as plain HTTP errors.
            message = f"HTTP {e.response.status_code}"
            try:
                message = json.loads(e.response.body or b"{}").get("error", message)
            except ValueError:
                pass
            raise error_for(e.response.status_code, message) from e
        except (OSError, WebSocketException, TimeoutError) as e:
            raise ApiError(0, f"cannot open shell: {e}") from e
        return ShellConnection(ws)


class ShellConnection:
    """An attached shell: raw bytes in, :mod:`rmm_tui.shell` events out."""

    def __init__(self, ws: Any) -> None:
        self._ws = ws

    async def send_input(self, data: bytes) -> None:
        await self._ws.send(data)

    async def send_control(self, text: str) -> None:
        await self._ws.send(text)

    async def messages(self):  # -> AsyncIterator[bytes | str]
        from websockets.exceptions import ConnectionClosed

        try:
            async for message in self._ws:
                yield message
        except ConnectionClosed:
            return

    async def close(self) -> None:
        await self._ws.close()
