"""Staying signed in: the session token lives in the OS keyring (Windows
Credential Manager, macOS Keychain, Secret Service on Linux), one entry per
server URL, and is validated against ``/api/me`` on start."""

from __future__ import annotations

import json
import logging
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any, Protocol

import keyring
import keyring.errors

from .api import ApiClient, ApiError, Unauthorized, User, parse_time

log = logging.getLogger(__name__)

KEYRING_SERVICE = "rmm-tui"


class KeyringBackend(Protocol):
    def get_password(self, service: str, username: str) -> str | None: ...
    def set_password(self, service: str, username: str, password: str) -> None: ...
    def delete_password(self, service: str, username: str) -> None: ...


@dataclass(frozen=True)
class StoredSession:
    token: str
    expires_at: datetime
    username: str


class TokenStore:
    """The saved session for one server."""

    def __init__(self, server_url: str, backend: KeyringBackend | Any = keyring) -> None:
        self.key = server_url.rstrip("/")
        self._backend = backend

    def load(self) -> StoredSession | None:
        """The saved session, or ``None`` if there is none, it is unreadable,
        or the keyring is unavailable."""
        try:
            raw = self._backend.get_password(KEYRING_SERVICE, self.key)
        except keyring.errors.KeyringError as e:
            log.warning("reading the keyring failed: %s", e)
            return None
        if not raw:
            return None
        try:
            d = json.loads(raw)
            expires_at = parse_time(d["expires_at"])
            if expires_at is None or not d["token"]:
                raise ValueError("incomplete")
            return StoredSession(token=d["token"], expires_at=expires_at, username=d["username"])
        except (ValueError, KeyError, TypeError) as e:
            log.warning("ignoring a malformed saved session: %s", e)
            return None

    def save(self, session: StoredSession) -> bool:
        """Save; ``False`` if the keyring is unavailable (the user then has
        to sign in again next time, nothing else breaks)."""
        raw = json.dumps(
            {
                "token": session.token,
                "expires_at": session.expires_at.isoformat(),
                "username": session.username,
            }
        )
        try:
            self._backend.set_password(KEYRING_SERVICE, self.key, raw)
            return True
        except keyring.errors.KeyringError as e:
            log.warning("saving to the keyring failed: %s", e)
            return False

    def clear(self) -> None:
        try:
            self._backend.delete_password(KEYRING_SERVICE, self.key)
        except keyring.errors.PasswordDeleteError:
            pass  # nothing saved
        except keyring.errors.KeyringError as e:
            log.warning("clearing the keyring failed: %s", e)


def _now() -> datetime:
    return datetime.now(UTC)


class SessionManager:
    """Sign in, stay signed in, sign out."""

    def __init__(
        self,
        api: ApiClient,
        store: TokenStore,
        clock: Callable[[], datetime] = _now,
    ) -> None:
        self.api = api
        self.store = store
        self._clock = clock
        self.user: User | None = None
        #: False if the last login could not be saved to the keyring.
        self.persisted = True

    async def restore(self) -> User | None:
        """Resume the saved session if the server still accepts it.

        An expired or rejected token is removed from the keyring. If the
        server cannot be reached the token is kept and the error raised, so
        a network blip does not sign the user out.
        """
        saved = self.store.load()
        if saved is None:
            return None
        if saved.expires_at <= self._clock():
            log.info("saved session expired")
            self.store.clear()
            return None
        self.api.token = saved.token
        try:
            user = await self.api.me()
        except Unauthorized:
            log.info("saved session rejected by the server")
            self.api.token = None
            self.store.clear()
            return None
        except ApiError:
            self.api.token = None
            raise
        self.user = user
        return user

    async def login(self, username: str, password: str, code: str) -> User:
        result = await self.api.login(username, password, code)
        self.api.token = result.token
        self.user = result.user
        self.persisted = self.store.save(
            StoredSession(
                token=result.token, expires_at=result.expires_at, username=result.user.username
            )
        )
        return result.user

    async def logout(self) -> None:
        """Revoke the session on the server (best effort) and forget it."""
        if self.api.token:
            try:
                await self.api.logout()
            except ApiError as e:
                log.warning("server logout failed: %s", e)
        self.api.token = None
        self.user = None
        self.store.clear()

    def expired(self) -> None:
        """The server rejected the session mid-use: forget it."""
        self.api.token = None
        self.user = None
        self.store.clear()
