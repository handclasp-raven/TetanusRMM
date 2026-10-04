"""Configuration: a TOML file, overridden by command-line flags.

Example ``config.toml``::

    server_url = "https://rmm.example.com:8443"
    ca_path = "/etc/rmm/ca.crt"          # optional; see below
    viewer_path = "/opt/rmm/viewer"      # optional; default: the server's build
    quic_addr = "rmm.example.com:4433"   # optional; default: API host, port 4433
    poll_interval = 5
    viewer_font_size = 9                 # optional; the viewer's default is 10

None of it is required: the server is typed on the login screen, its CA is
accepted there the first time (see ``trust``), and the viewer is downloaded
from the server (see ``provision``).
"""

from __future__ import annotations

import os
import sys
import tomllib
from dataclasses import dataclass, field, fields, replace
from pathlib import Path
from urllib.parse import urlsplit

APP_NAME = "tetanus-rmm"
#: The app's name up to 0.1.1; its directories are moved over on first run.
LEGACY_APP_NAME = "rmm-tui"
DEFAULT_QUIC_PORT = 4433
#: Text sizes the viewer accepts (``--font-size``).
VIEWER_FONT_SIZES = range(6, 33)


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


def migrate_legacy_dirs() -> None:
    """Move the config and data directories kept under the former name to
    the current ones, unless those already exist."""
    for new in {config_dir(), data_dir()}:
        old = new.with_name(LEGACY_APP_NAME)
        if old.is_dir() and not new.exists():
            try:
                old.rename(new)
            except OSError:
                pass  # start afresh; the old directory is left as it was


def check_server_url(url: str) -> None:
    parts = urlsplit(url)
    if parts.scheme != "https" or not parts.hostname:
        raise ConfigError(f"server_url must be an https:// URL, got {url!r}")


def normalize_server_url(value: str) -> str:
    """A server URL as typed on the login screen: ``https://`` is assumed
    without a scheme, and a trailing slash is dropped. Raises
    :class:`ConfigError` if it is not an ``https://`` URL."""
    url = value.strip()
    if not url:
        raise ConfigError("enter the server URL")
    if "://" not in url:
        url = f"https://{url}"
    url = url.rstrip("/")
    try:
        check_server_url(url)
    except ValueError as e:  # e.g. a malformed IPv6 host
        raise ConfigError(f"invalid server URL {value!r}: {e}") from e
    return url


def default_config_path() -> Path:
    env = os.environ.get("TETANUS_RMM_CONFIG") or os.environ.get("RMM_TUI_CONFIG")  # former name
    return Path(env or config_dir() / "config.toml")


@dataclass(frozen=True)
class Config:
    #: Base URL of the server's HTTPS API.
    server_url: str = "https://localhost:8443"
    #: PEM CA certificate to trust for the server. ``None``: the CA accepted
    #: for the server at first sign-in, else the system trust store.
    ca_path: Path | None = None
    #: The native viewer binary: a path, or a name looked up on PATH.
    #: ``None``: the build the server publishes, else ``viewer`` on PATH.
    viewer_path: str | None = None
    #: ``host:port`` of the server's QUIC listener, for the viewer. Default:
    #: the API's host on port 4433.
    quic_addr: str | None = None
    #: Seconds between agent-list refreshes.
    poll_interval: float = 5.0
    #: Saved-script library.
    scripts_path: Path = field(default_factory=lambda: data_dir() / "scripts.json")
    #: Text size of the viewer's toolbar and panel, in pixels. ``None``: the
    #: viewer's own default.
    viewer_font_size: int | None = None
    #: Bitwarden's command line client, for the viewer's Vault button: a
    #: path, or a name looked up on PATH. ``None``: ``bw`` on PATH.
    bw_path: str | None = None

    def __post_init__(self) -> None:
        check_server_url(self.server_url)
        if self.poll_interval < 1:
            raise ConfigError("poll_interval must be at least 1 second")
        size = self.viewer_font_size
        if size is not None and (
            isinstance(size, bool) or not isinstance(size, int) or size not in VIEWER_FONT_SIZES
        ):
            raise ConfigError(
                f"viewer_font_size must be a whole number of pixels from "
                f"{VIEWER_FONT_SIZES.start} to {VIEWER_FONT_SIZES.stop - 1}, got {size!r}"
            )

    @property
    def api_host(self) -> str:
        return urlsplit(self.server_url).hostname or ""

    def for_server(self, server_url: str) -> Config:
        """A copy for another server. ``quic_addr`` is kept only if the
        host is the same: it named the old server's QUIC listener."""
        same_host = urlsplit(server_url).hostname == self.api_host
        return replace(self, server_url=server_url, quic_addr=self.quic_addr if same_host else None)

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
