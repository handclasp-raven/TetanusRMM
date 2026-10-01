"""Display formatting for the agent table, and the columns it can show."""

from __future__ import annotations

from collections.abc import Callable, Iterable
from dataclasses import dataclass
from datetime import UTC, datetime

from rich.text import Text

from .api import Agent

Cell = str | Text


def bytes_short(n: int) -> str:
    value = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024 or unit == "TiB":
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024
    raise AssertionError("unreachable")


def usage(used: int | None, total: int | None) -> str:
    """``38% of 16.0 GiB``, or ``–`` without telemetry."""
    if used is None or not total:
        return "–"
    return f"{used * 100 / total:.0f}% of {bytes_short(total)}"


def cpu(percent: float | None) -> str:
    return "–" if percent is None else f"{percent:.0f}%"


def ago(when: datetime | None, now: datetime | None = None) -> str:
    """``12s ago``, ``5m ago``, ``3h ago``, ``2d ago``, or ``never``."""
    if when is None:
        return "never"
    secs = max(0, int(((now or datetime.now(UTC)) - when).total_seconds()))
    for size, unit in ((86400, "d"), (3600, "h"), (60, "m")):
        if secs >= size:
            return f"{secs // size}{unit} ago"
    return f"{secs}s ago"


def duration(secs: int | None) -> str:
    """``3d 4h``, ``5h 12m`` or ``7m``; ``–`` if unknown."""
    if secs is None:
        return "–"
    days, rest = divmod(max(0, secs), 86400)
    hours, rest = divmod(rest, 3600)
    minutes = rest // 60
    if days:
        return f"{days}d {hours}h"
    if hours:
        return f"{hours}h {minutes}m"
    return f"{minutes}m"


def status(agent: Agent) -> str:
    if agent.enrollment_state == "revoked":
        return "revoked"
    if not agent.online:
        return "offline"
    # On the WebSocket fallback: worth knowing when a session feels slow.
    return "online (ws)" if agent.transport == "websocket" else "online"


def status_cell(agent: Agent) -> Text:
    """The status behind a green (online), red (offline) or grey
    (revoked) dot."""
    if agent.enrollment_state == "revoked":
        colour = "bright_black"
    else:
        colour = "green" if agent.online else "red"
    return Text.assemble(("●", colour), " ", status(agent))


def groups(agent: Agent) -> str:
    return ", ".join(agent.groups) or "–"


def sessions(agent: Agent) -> str:
    parts = []
    if agent.viewer_sessions:
        parts.append(f"{agent.viewer_sessions} desktop")
    if agent.shell_sessions:
        parts.append(f"{agent.shell_sessions} shell")
    return ", ".join(parts) or "–"


def users(agent: Agent) -> str:
    """Who is signed in right now: ``–`` while offline or not reported."""
    if not agent.online or agent.logged_in_users is None:
        return "–"
    return ", ".join(agent.logged_in_users) or "none"


def classification_label(classification: str) -> str:
    """``Server``, ``Desktop`` or ``Other``."""
    return classification.capitalize()


def classification(agent: Agent) -> str:
    """The classification; marked with ``*`` when an admin chose it."""
    label = classification_label(agent.classification)
    return f"{label}*" if agent.classification_override else label


def uptime(agent: Agent) -> str:
    # The last sample of an offline agent is stale: it may have rebooted.
    return duration(agent.uptime_secs) if agent.online else "–"


@dataclass(frozen=True)
class Column:
    key: str
    label: str
    render: Callable[[Agent, datetime], Cell]


#: Every column the agent table can show, in the column menu's order.
COLUMNS: dict[str, Column] = {
    c.key: c
    for c in (
        Column("host", "Hostname", lambda a, _: a.label),
        Column("status", "Status", lambda a, _: status_cell(a)),
        Column("class", "Classification", lambda a, _: classification(a)),
        Column("os", "OS", lambda a, _: a.os or "–"),
        Column("user", "Logged-in user", lambda a, _: users(a)),
        Column("ip", "IP address", lambda a, _: a.local_ip or "–"),
        Column("public_ip", "Public IP", lambda a, _: a.remote_ip or "–"),
        Column("uptime", "Uptime", lambda a, _: uptime(a)),
        Column("groups", "Groups", lambda a, _: groups(a)),
        Column("last_seen", "Last seen", lambda a, now: ago(a.last_seen, now)),
        Column("cpu", "CPU", lambda a, _: cpu(a.cpu_percent)),
        Column("ram", "RAM", lambda a, _: usage(a.mem_used_bytes, a.mem_total_bytes)),
        Column("disk", "Disk", lambda a, _: usage(a.disk_used_bytes, a.disk_total_bytes)),
        Column("sessions", "Active sessions", lambda a, _: sessions(a)),
        Column("id", "Agent ID", lambda a, _: a.id),
    )
}

#: Shown until the user picks their own.
DEFAULT_COLUMNS: tuple[str, ...] = (
    "host",
    "status",
    "class",
    "os",
    "user",
    "ip",
    "uptime",
    "groups",
    "last_seen",
    "cpu",
    "ram",
    "disk",
    "sessions",
)


#: Columns since replaced: old key -> new key.
RENAMED_COLUMNS = {"kind": "class"}


def valid_columns(keys: Iterable[str] | None) -> list[str]:
    """``keys`` without unknown or repeated ones (e.g. from an older or
    hand-edited state file); the defaults if nothing is left."""
    seen: dict[str, None] = {}
    for key in keys or ():
        key = RENAMED_COLUMNS.get(key, key)
        if key in COLUMNS:
            seen.setdefault(key)
    return list(seen) or list(DEFAULT_COLUMNS)


def agent_row(
    agent: Agent, now: datetime | None = None, columns: Iterable[str] = DEFAULT_COLUMNS
) -> tuple[Cell, ...]:
    """Cells for ``columns`` (keys of :data:`COLUMNS`), in that order."""
    now = now or datetime.now(UTC)
    return tuple(COLUMNS[key].render(agent, now) for key in columns)
