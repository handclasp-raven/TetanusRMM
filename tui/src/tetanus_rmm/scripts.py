"""Script runner support: the local library of saved scripts, and turning a
run's per-agent results into what the TUI shows.

A run is one ``POST /api/script-runs`` for all chosen agents, so it is a
single audited run on the server (one ``run_id``). Each target shows as
*running* until the report arrives, then gets its own status, exit code and
output.
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

MAX_TIMEOUT_SECS = 3600

#: Offered until the user saves scripts of their own.
DEFAULT_SCRIPTS = [
    {
        "name": "Hostname and uptime",
        "body": "hostname\n(Get-Date) - (Get-CimInstance Win32_OperatingSystem).LastBootUpTime",
    },
    {
        "name": "Disk space",
        "body": "Get-PSDrive -PSProvider FileSystem | Format-Table -AutoSize",
    },
    {
        "name": "Stopped automatic services",
        "body": "Get-Service |\n"
        "  Where-Object { $_.StartType -eq 'Automatic' -and $_.Status -ne 'Running' }",
    },
]


class ScriptError(Exception):
    pass


@dataclass(frozen=True)
class SavedScript:
    name: str
    body: str
    timeout_secs: int | None = None


def validate_timeout(value: str) -> int | None:
    """Parse the timeout field: blank means the server default."""
    value = value.strip()
    if not value:
        return None
    try:
        secs = int(value)
    except ValueError:
        raise ScriptError("timeout must be a whole number of seconds") from None
    if not 1 <= secs <= MAX_TIMEOUT_SECS:
        raise ScriptError(f"timeout must be between 1 and {MAX_TIMEOUT_SECS} seconds")
    return secs


class ScriptLibrary:
    """Saved scripts in a JSON file, keyed by name."""

    def __init__(self, path: Path) -> None:
        self.path = path

    def load(self) -> list[SavedScript]:
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            raw = DEFAULT_SCRIPTS
        except (OSError, ValueError) as e:
            raise ScriptError(f"cannot read {self.path}: {e}") from e
        try:
            scripts = [
                SavedScript(
                    name=str(s["name"]),
                    body=str(s["body"]),
                    timeout_secs=s.get("timeout_secs"),
                )
                for s in raw
            ]
        except (TypeError, KeyError) as e:
            raise ScriptError(f"{self.path} is not a script library: {e}") from e
        return sorted(scripts, key=lambda s: s.name.lower())

    def _write(self, scripts: list[SavedScript]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".tmp")
        tmp.write_text(json.dumps([asdict(s) for s in scripts], indent=2), encoding="utf-8")
        os.replace(tmp, self.path)

    def save(self, script: SavedScript) -> None:
        """Add ``script``, replacing one with the same name."""
        name = script.name.strip()
        if not name:
            raise ScriptError("a saved script needs a name")
        if not script.body.strip():
            raise ScriptError("the script is empty")
        script = SavedScript(name, script.body, script.timeout_secs)
        others = [s for s in self.load() if s.name != name]
        self._write(others + [script])

    def delete(self, name: str) -> None:
        self._write([s for s in self.load() if s.name != name])


# --- results -----------------------------------------------------------------


@dataclass(frozen=True)
class AgentResult:
    """One agent's part of a run (mirrors the server's ``AgentResult``)."""

    agent_id: str
    status: str  # running | completed | timed_out | offline | unsupported | failed
    exit_code: int | None = None
    stdout: str = ""
    stderr: str = ""
    stdout_truncated: bool = False
    stderr_truncated: bool = False
    duration_ms: int | None = None
    error: str | None = None

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> AgentResult:
        return cls(
            agent_id=d["agent_id"],
            status=d["status"],
            exit_code=d.get("exit_code"),
            stdout=d.get("stdout") or "",
            stderr=d.get("stderr") or "",
            stdout_truncated=bool(d.get("stdout_truncated")),
            stderr_truncated=bool(d.get("stderr_truncated")),
            duration_ms=d.get("duration_ms"),
            error=d.get("error"),
        )

    @property
    def succeeded(self) -> bool:
        return self.status == "completed" and self.exit_code == 0

    @property
    def status_label(self) -> str:
        match self.status:
            case "running":
                return "running…"
            case "completed":
                return "ok" if self.exit_code == 0 else "nonzero exit"
            case "timed_out":
                return "timed out"
            case other:
                return other.replace("_", " ")

    @property
    def exit_label(self) -> str:
        return "" if self.exit_code is None else str(self.exit_code)

    @property
    def duration_label(self) -> str:
        if self.duration_ms is None:
            return ""
        if self.duration_ms < 1000:
            return f"{self.duration_ms}ms"
        return f"{self.duration_ms / 1000:.1f}s"

    def output_text(self) -> str:
        """Everything worth showing for this agent, as plain text."""
        parts = []
        if self.error:
            parts.append(f"error: {self.error}")
        if self.stdout:
            parts.append("── stdout ──\n" + self.stdout.rstrip("\n"))
            if self.stdout_truncated:
                parts.append("[stdout truncated]")
        if self.stderr:
            parts.append("── stderr ──\n" + self.stderr.rstrip("\n"))
            if self.stderr_truncated:
                parts.append("[stderr truncated]")
        if not parts:
            parts.append("(still running)" if self.status == "running" else "(no output)")
        return "\n".join(parts)


@dataclass
class ScriptRun:
    """A run from the TUI's point of view: every target, in order.

    ``group_ids`` are sent to the server, which resolves their members when
    the run starts; ``agent_ids`` should already include the members the
    TUI knows of, so they show as running meanwhile. Any other agent the
    report mentions is added at the end."""

    agent_ids: list[str]
    script: str
    group_ids: list[int] = field(default_factory=list)
    run_id: int | None = None
    results: dict[str, AgentResult] = field(default_factory=dict)
    #: Set if the whole request failed (e.g. forbidden, server unreachable).
    error: str | None = None

    def __post_init__(self) -> None:
        if not self.script.strip():
            raise ScriptError("enter a script or command")
        # Same normalization as the server: trimmed, de-duplicated, ordered.
        seen: dict[str, None] = {}
        for agent_id in self.agent_ids:
            if agent_id.strip():
                seen.setdefault(agent_id.strip())
        if not seen and not self.group_ids:
            raise ScriptError("choose at least one agent or group")
        self.agent_ids = list(seen)
        self.results = {a: AgentResult(a, "running") for a in self.agent_ids}

    @property
    def done(self) -> bool:
        return all(r.status != "running" for r in self.results.values())

    def apply_report(self, report: dict[str, Any]) -> None:
        """Fill in results from the server's run report. A target the
        report does not mention is marked failed."""
        self.run_id = report.get("run_id")
        by_agent = {r["agent_id"]: AgentResult.from_json(r) for r in report.get("results", [])}
        # Group members the TUI did not know of (joined meanwhile).
        self.agent_ids += [a for a in by_agent if a not in self.results]
        for agent_id in self.agent_ids:
            self.results[agent_id] = by_agent.get(
                agent_id, AgentResult(agent_id, "failed", error="no result in the report")
            )

    def fail(self, message: str) -> None:
        """The request itself failed: every pending target failed with it."""
        self.error = message
        for agent_id, result in self.results.items():
            if result.status == "running":
                self.results[agent_id] = AgentResult(agent_id, "failed", error=message)

    def ordered(self) -> list[AgentResult]:
        return [self.results[a] for a in self.agent_ids]

    def summary(self) -> str:
        results = self.ordered()
        if not self.done:
            pending = sum(r.status == "running" for r in results)
            return f"running on {pending} agent(s)…"
        ok = sum(r.succeeded for r in results)
        ran = sum(r.status in ("completed", "timed_out") for r in results)
        text = f"{ok}/{len(results)} succeeded"
        if ran - ok:
            text += f", {ran - ok} failed"
        if len(results) - ran:
            text += f", {len(results) - ran} not run"
        if self.run_id is not None:
            text += f" (run {self.run_id})"
        return text
