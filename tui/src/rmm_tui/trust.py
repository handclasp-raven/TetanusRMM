"""Trusting a server's private CA without copying files around.

A server set up with ``gen-certs`` presents a certificate signed by its own
CA, which no system trust store knows. The first time the TUI meets such a
server it fetches the CA certificate (``GET /api/ca``, unverified), shows
its SHA-256 fingerprint for the user to accept, and from then on verifies
that server against the accepted CA only, as SSH does with host keys. A
later mismatch is an error, never a new prompt.

Accepted CAs live in the data directory, one directory per server:
``servers/<host>_<port>/ca.crt``. A CA fetched over an already verified
connection (a server with a public certificate on its API), which only the
viewer needs, is kept beside it as ``viewer-ca.crt`` and is never used to
verify the API.
"""

from __future__ import annotations

import hashlib
import os
import re
import ssl
from pathlib import Path
from urllib.parse import urlsplit

import httpx

from .api import DEFAULT_TIMEOUT, ApiError, cert_failure

PINNED_FILE = "ca.crt"
VIEWER_CA_FILE = "viewer-ca.crt"
#: A CA certificate is a few hundred bytes; refuse anything absurd.
MAX_CA_BYTES = 64 * 1024


def fingerprint(pem: str) -> str:
    """SHA-256 of the certificate, as ``openssl x509 -fingerprint -sha256``
    prints it (``AB:CD:…``). Raises :class:`ValueError` if ``pem`` is not a
    certificate."""
    digest = hashlib.sha256(ssl.PEM_cert_to_DER_cert(pem.strip())).hexdigest().upper()
    return ":".join(digest[i : i + 2] for i in range(0, len(digest), 2))


def _context(pem: str) -> ssl.SSLContext:
    try:
        return ssl.create_default_context(cadata=pem)
    except (ssl.SSLError, ValueError) as e:
        raise ApiError(0, f"the server sent an unusable CA certificate: {e}") from e


async def fetch_ca(server_url: str) -> str:
    """The CA certificate ``server_url`` offers, checked to be the one that
    signed the certificate it presents. Nothing here makes it trustworthy:
    that is the user's call, from the fingerprint."""
    base = server_url.rstrip("/")
    try:
        async with httpx.AsyncClient(verify=False, timeout=DEFAULT_TIMEOUT) as http:  # noqa: S501
            response = await http.get(f"{base}/api/ca")
    except httpx.HTTPError as e:
        raise ApiError(0, f"cannot reach server: {e}") from e
    if response.status_code == 404:
        raise ApiError(
            404,
            "the server's certificate is not trusted, and the server is too old to "
            "offer its CA: set ca_path in the config",
        )
    if response.is_error or len(response.content) > MAX_CA_BYTES:
        raise ApiError(response.status_code, "the server did not send its CA certificate")
    pem = response.text
    try:
        fingerprint(pem)
    except ValueError as e:
        raise ApiError(0, "the server did not send its CA certificate") from e
    try:
        async with httpx.AsyncClient(verify=_context(pem), timeout=DEFAULT_TIMEOUT) as http:
            await http.get(f"{base}/api/health")
    except httpx.HTTPError as e:
        failure = cert_failure(e)
        if failure is None:
            raise ApiError(0, f"cannot reach server: {e}") from e
        host = urlsplit(server_url).hostname
        raise ApiError(
            0,
            f"the server's certificate cannot be used for {host}: {failure.rstrip('.')}. "
            "On the server, add this name with `gen-certs --san`.",
        ) from e
    return pem


class TrustStore:
    """The CAs accepted for each server, under ``root``."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def _dir(self, server_url: str) -> Path:
        parts = urlsplit(server_url)
        name = f"{parts.hostname}_{parts.port or 443}"
        return self.root / re.sub(r"[^A-Za-z0-9._-]", "-", name)

    def pinned(self, server_url: str) -> Path | None:
        """The CA accepted for this server, if any."""
        path = self._dir(server_url) / PINNED_FILE
        return path if path.is_file() else None

    def pin(self, server_url: str, pem: str) -> Path:
        return _write(self._dir(server_url) / PINNED_FILE, pem)

    def forget(self, server_url: str) -> bool:
        """Drop the CA accepted for this server; ``False`` if there was none."""
        path = self._dir(server_url) / PINNED_FILE
        try:
            path.unlink()
        except FileNotFoundError:
            return False
        return True

    def save_viewer_ca(self, server_url: str, pem: str) -> Path:
        """Keep a CA fetched over a verified connection, for the viewer."""
        return _write(self._dir(server_url) / VIEWER_CA_FILE, pem)


def _write(path: Path, pem: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(pem, encoding="ascii")
    os.replace(tmp, path)
    return path
