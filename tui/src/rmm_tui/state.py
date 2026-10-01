"""What the TUI remembers between launches: the server last signed in to
(the login screen's default), the agent-table columns and the remote
viewer's command buttons.

A small JSON file in the data directory. It is only a convenience: if it is
missing or unreadable the defaults apply, and failing to write it is logged,
never shown as an error.
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import dataclass
from pathlib import Path

from . import commands
from .commands import QuickCommand

log = logging.getLogger(__name__)


@dataclass
class UiState:
    path: Path
    #: Base URL of the server last signed in to.
    last_server: str | None = None
    #: Agent-table column keys, in order. ``None``: the defaults.
    agent_columns: list[str] | None = None
    #: The viewer's command buttons. ``None``: the defaults.
    viewer_commands: list[QuickCommand] | None = None

    @property
    def commands(self) -> list[QuickCommand]:
        """The viewer's command buttons, defaults included."""
        if self.viewer_commands is None:
            return list(commands.DEFAULT_COMMANDS)
        return list(self.viewer_commands)

    @classmethod
    def load(cls, path: Path) -> UiState:
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return cls(path)
        except (OSError, ValueError) as e:
            log.warning("ignoring unreadable %s: %s", path, e)
            return cls(path)
        if not isinstance(raw, dict):
            return cls(path)
        server = raw.get("last_server")
        columns = raw.get("agent_columns")
        return cls(
            path,
            last_server=server if isinstance(server, str) else None,
            agent_columns=[str(c) for c in columns] if isinstance(columns, list) else None,
            viewer_commands=commands.from_json(raw.get("viewer_commands")),
        )

    def save(self) -> None:
        data = {
            "last_server": self.last_server,
            "agent_columns": self.agent_columns,
            "viewer_commands": (
                None
                if self.viewer_commands is None
                else [c.to_json() for c in self.viewer_commands]
            ),
        }
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.path.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
            os.replace(tmp, self.path)
        except OSError as e:
            log.warning("saving %s failed: %s", self.path, e)
