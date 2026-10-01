"""Getting the native viewer without installing it by hand.

Unless ``viewer_path`` is set, the TUI uses the viewer build the server
publishes for this platform (``GET /api/viewer/<platform>/…``): it is
downloaded into the data directory on first use, checked against the
SHA-256 in the server's manifest, and replaced whenever the server
publishes another. The viewer therefore always matches the server.
"""

from __future__ import annotations

import hashlib
import logging
import platform as platform_mod
import re
import sys
from collections.abc import Callable
from pathlib import Path

from .api import ApiClient, ApiError, ViewerBuild

log = logging.getLogger(__name__)

_OS = {"linux": "linux", "win32": "windows", "darwin": "macos"}
_ARCH = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}


def current_platform() -> str:
    """This machine as the server names platforms: ``<os>-<arch>``, e.g.
    ``linux-x86_64`` or ``windows-x86_64``."""
    machine = platform_mod.machine().lower()
    return f"{_OS.get(sys.platform, sys.platform)}-{_ARCH.get(machine, machine)}"


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        while chunk := f.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def viewer_file(directory: Path, build: ViewerBuild) -> Path:
    """Where ``build`` is kept. The name carries the hash, so a new build
    never has to overwrite one that is running (Windows would refuse)."""
    suffix = ".exe" if build.platform.startswith("windows") else ""
    return directory / f"viewer-{build.sha256[:16]}{suffix}"


async def ensure_viewer(
    api: ApiClient,
    directory: Path,
    platform: str | None = None,
    on_download: Callable[[ViewerBuild], None] | None = None,
) -> Path | None:
    """The server's viewer build for this platform, downloading it into
    ``directory`` if it is not there yet. ``None`` if the server publishes
    none. ``on_download`` is called before a download starts."""
    build = await api.viewer_build(platform or current_platform())
    if build is None:
        return None
    if not re.fullmatch(r"[0-9a-f]{64}", build.sha256):
        raise ApiError(0, "the server sent a malformed viewer manifest")
    dest = viewer_file(directory, build)
    if dest.is_file():
        return dest
    if on_download:
        on_download(build)
    directory.mkdir(parents=True, exist_ok=True)
    part = dest.with_name(dest.name + ".download")
    try:
        await api.download(f"/api/viewer/{build.platform}/binary", part)
        if _sha256(part) != build.sha256:
            raise ApiError(0, "the downloaded viewer does not match the server's manifest")
        part.chmod(0o755)
        part.replace(dest)
    except OSError as e:
        raise ApiError(0, f"cannot install the viewer in {directory}: {e}") from e
    finally:
        part.unlink(missing_ok=True)
    log.info("installed viewer %s (%s) at %s", build.version, build.platform, dest)
    for old in directory.glob("viewer-*"):
        if old != dest:
            try:
                old.unlink()
            except OSError:  # still running: the next download removes it
                pass
    return dest
