"""Configuration: a TOML file, overridden by command-line flags.

Example ``config.toml``::

    server_url = "https://rmm.example.com:8443"
    ca_path = "/etc/rmm/ca.crt"          # dev CA; omit to use the system trust store
    viewer_path = "/opt/rmm/viewer"      # the native remote-desktop viewer
    quic_addr = "rmm.example.com:4433"   # optional; default: API host, port 4433
    poll_interval = 5
"""

from __future__ import annotations

import os
import sys
import tomllib
from dataclasses import dataclass, field, fields, replace
from pathlib import Path
from urllib.parse import urlsplit

APP_NAME = "rmm-tui"
DEFAULT_QUIC_PORT = 4433


class ConfigError(Exception):
    pass


def config_dir() -> Path:
    """Per-user config directory for this OS."""
    if sys.platform == "win32":
        return Path(os.environ.get("APPDATA", Path.home() / "AppData" / "Roaming")) / APP_NAME
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / APP_NAME
    return Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / APP_NAME


def data_dir() -> Path:
    """Per-user data directory (saved scripts, logs)."""
    if sys.platform == "win32":
        return Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local")) / APP_NAME
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / APP_NAME
    return Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local" / "share") / APP_NAME


def default_config_path() -> Path:
    return Path(os.environ.get("RMM_TUI_CONFIG") or config_dir() / "config.toml")


@dataclass(frozen=True)
class Config:
    #: Base URL of the server's HTTPS API.
    server_url: str = "https://localhost:8443"
    #: PEM CA certificate to trust for the server (dev CA). ``None`` uses the
    #: system trust store. The viewer needs one either way.
    ca_path: Path | None = None
    #: The native viewer binary: a path, or a name looked up on PATH.
    viewer_path: str = "viewer"
    #: ``host:port`` of the server's QUIC listener, for the viewer. Default:
    #: the API's host on port 4433.
    quic_addr: str | None = None
    #: Seconds between agent-list refreshes.
    poll_interval: float = 5.0
    #: Saved-script library.
    scripts_path: Path = field(default_factory=lambda: data_dir() / "scripts.json")

    def __post_init__(self) -> None:
        parts = urlsplit(self.server_url)
        if parts.scheme != "https" or not parts.hostname:
            raise ConfigError(f"server_url must be an https:// URL, got {self.server_url!r}")
        if self.poll_interval < 1:
            raise ConfigError("poll_interval must be at least 1 second")

    @property
    def api_host(self) -> str:
        return urlsplit(self.server_url).hostname or ""

    def with_overrides(self, **overrides: object) -> Config:
        """A copy with every non-``None`` override applied."""
        return replace(self, **{k: v for k, v in overrides.items() if v is not None})


_PATH_FIELDS = {"ca_path", "scripts_path"}


def load(path: Path | None = None) -> Config:
    """Read ``path`` (default: the per-user config file). A missing file
    gives the defaults; unknown keys are an error, so typos are caught."""
    path = path or default_config_path()
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return Config()
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise ConfigError(f"{path}: {e}") from e
    known = {f.name for f in fields(Config)}
    unknown = sorted(set(raw) - known)
    if unknown:
        raise ConfigError(f"{path}: unknown setting(s): {', '.join(unknown)}")
    values: dict[str, object] = {}
    for key, value in raw.items():
        if key in _PATH_FIELDS:
            # Relative paths are relative to the config file.
            value = (path.parent / Path(value).expanduser()).resolve()
        values[key] = value
    try:
        return Config(**values)  # type: ignore[arg-type]
    except TypeError as e:
        raise ConfigError(f"{path}: {e}") from e
