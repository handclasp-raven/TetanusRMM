"""Display formatting for the agent table."""

from __future__ import annotations

from datetime import UTC, datetime

from .api import Agent


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


def status(agent: Agent) -> str:
    if agent.enrollment_state == "revoked":
        return "revoked"
    if not agent.online:
        return "offline"
    # On the WebSocket fallback: worth knowing when a session feels slow.
    return "online (ws)" if agent.transport == "websocket" else "online"


def groups(agent: Agent) -> str:
    return ", ".join(agent.groups) or "–"


def sessions(agent: Agent) -> str:
    parts = []
    if agent.viewer_sessions:
        parts.append(f"{agent.viewer_sessions} desktop")
    if agent.shell_sessions:
        parts.append(f"{agent.shell_sessions} shell")
    return ", ".join(parts) or "–"


def agent_row(agent: Agent, now: datetime | None = None) -> tuple[str, ...]:
    """Cells in column order: host, status, groups, last seen, CPU, RAM,
    disk, sessions."""
    return (
        agent.label,
        status(agent),
        groups(agent),
        ago(agent.last_seen, now),
        cpu(agent.cpu_percent),
        usage(agent.mem_used_bytes, agent.mem_total_bytes),
        usage(agent.disk_used_bytes, agent.disk_total_bytes),
        sessions(agent),
    )
