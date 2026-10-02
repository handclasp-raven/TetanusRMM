"""The stats panel under the agent table: the selected agent's CPU, memory
and disk now and over the last hour, and a cache of the history behind it.

The current values come with the agent list, so the panel fills at once.
Only the history is fetched, and :class:`TelemetryCache` keeps it per agent
so that moving back to an agent, or the list refreshing, asks the server
for nothing until a new sample can exist.
"""

from __future__ import annotations

import time
from collections import OrderedDict
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta

from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.widget import Widget
from textual.widgets import ProgressBar, Static

from . import formatting
from .api import Agent, TelemetryHistory, TelemetrySample

#: How much history the panel shows.
WINDOW = timedelta(hours=1)

BARS = "▁▂▃▄▅▆▇█"

Point = tuple[datetime, float]


def percent(used: int, total: int) -> float:
    return used * 100 / total if total else 0.0


def sparkline(
    points: Iterable[Point], start: datetime, end: datetime, width: int, step: float = 0
) -> str:
    """Percentages (0-100) between ``start`` and ``end`` as ``width`` bars,
    each the highest value of its share of the time; a space where there
    was no sample (the agent was offline). With samples ``step`` seconds
    apart, no more bars are drawn than there can be samples."""
    span = (end - start).total_seconds()
    if step > 0:
        width = min(width, int(span // step))
    if width <= 0 or span <= 0:
        return ""
    buckets: list[float | None] = [None] * width
    for when, value in points:
        offset = (when - start).total_seconds()
        if not 0 <= offset <= span:
            continue
        index = min(width - 1, int(offset / span * width))
        held = buckets[index]
        buckets[index] = value if held is None else max(held, value)
    last = len(BARS) - 1
    return "".join(
        " " if v is None else BARS[max(0, min(last, int(v / 100 * len(BARS))))] for v in buckets
    )


@dataclass
class _Series:
    samples: list[TelemetrySample]
    fetched: float
    step: float


class TelemetryCache:
    """Telemetry history by agent id, for the agents looked at most
    recently. An entry is fresh until a new sample can exist (one step
    after it was fetched); after that only the samples since its last one
    need fetching (see :meth:`last`)."""

    def __init__(
        self,
        window: timedelta = WINDOW,
        capacity: int = 64,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.window = window
        self.capacity = capacity
        self._clock = clock
        self._entries: OrderedDict[str, _Series] = OrderedDict()

    def get(self, agent_id: str) -> list[TelemetrySample] | None:
        """The samples held for the agent; ``None`` if never fetched."""
        entry = self._entries.get(agent_id)
        if entry is None:
            return None
        self._entries.move_to_end(agent_id)
        return entry.samples

    def step(self, agent_id: str) -> float:
        entry = self._entries.get(agent_id)
        return entry.step if entry else 0

    def fresh(self, agent_id: str) -> bool:
        """Whether asking the server again could not bring anything new."""
        entry = self._entries.get(agent_id)
        return entry is not None and self._clock() - entry.fetched < entry.step

    def last(self, agent_id: str) -> datetime | None:
        """When the newest sample held was taken: what to fetch since."""
        entry = self._entries.get(agent_id)
        return entry.samples[-1].ts if entry and entry.samples else None

    def store(
        self, agent_id: str, history: TelemetryHistory, now: datetime | None = None
    ) -> list[TelemetrySample]:
        """Add what was fetched to what is held, dropping samples older
        than the window. Returns the agent's samples."""
        oldest = (now or datetime.now(UTC)) - self.window
        held = self._entries.get(agent_id)
        merged = {s.ts: s for s in (*(held.samples if held else ()), *history.samples)}
        samples = [merged[ts] for ts in sorted(merged) if ts >= oldest]
        self._entries[agent_id] = _Series(samples, self._clock(), history.step_secs)
        self._entries.move_to_end(agent_id)
        while len(self._entries) > self.capacity:
            self._entries.popitem(last=False)
        return samples

    def failed(self, agent_id: str, retry_after: float = 30) -> None:
        """A fetch failed: keep what is held, and leave the server alone
        for ``retry_after`` seconds."""
        held = self._entries.get(agent_id)
        self._entries[agent_id] = _Series(held.samples if held else [], self._clock(), retry_after)


class Spark(Widget):
    """A sparkline as wide as the widget."""

    def __init__(self, id: str) -> None:  # noqa: A002
        super().__init__(id=id)
        self.points: Sequence[Point] = ()
        self.end = datetime.now(UTC)
        self.step = 0.0

    def show(self, points: Sequence[Point], end: datetime, step: float) -> None:
        self.points, self.end, self.step = points, end, step
        self.refresh()

    def render(self) -> Text:
        return Text(sparkline(self.points, self.end - WINDOW, self.end, self.size.width, self.step))


#: The panel's graphs: key, label, and the percentage a sample gives.
GRAPHS: tuple[tuple[str, str, Callable[[TelemetrySample], float]], ...] = (
    ("cpu", "CPU", lambda s: s.cpu_percent),
    ("ram", "RAM", lambda s: percent(s.mem_used_bytes, s.mem_total_bytes)),
    ("disk", "Disk", lambda s: percent(s.disk_used_bytes, s.disk_total_bytes)),
)


def details(agent: Agent) -> Text:
    """Uptime and sessions, who is signed in, and every fixed disk."""
    disks = "  ".join(
        f"{d.name} {formatting.usage(d.used_bytes, d.total_bytes)}" for d in agent.disks or ()
    )
    return Text(
        f"Uptime {formatting.uptime(agent)} · Sessions {formatting.sessions(agent)}\n"
        f"Users {formatting.users(agent)}\n"
        f"{disks}",
        no_wrap=True,
        overflow="ellipsis",
    )


class StatsPanel(Vertical):
    """The selected agent's health: now, and over the last hour."""

    def compose(self) -> ComposeResult:
        with Horizontal(id="stats-head"):
            yield Static(id="stats-title")
            yield ProgressBar(show_percentage=False, show_eta=False, id="stats-loading")
        with Horizontal(id="stats-body"):
            for key, _, _ in GRAPHS:
                with Vertical(classes="stat"):
                    yield Static(id=f"stat-{key}")
                    yield Spark(id=f"spark-{key}")
            yield Static(id="stats-details")

    def on_mount(self) -> None:
        self.loading_history = False

    @property
    def loading_history(self) -> bool:
        """Whether the loading bar is showing."""
        return bool(self.query_one("#stats-loading").display)

    @loading_history.setter
    def loading_history(self, loading: bool) -> None:
        self.query_one("#stats-loading").display = loading

    def show(
        self,
        agent: Agent | None,
        samples: Sequence[TelemetrySample] | None = None,
        *,
        step: float = 0,
        note: str = "",
        now: datetime | None = None,
    ) -> None:
        """Show ``agent`` as the agent list has it, over ``samples`` (its
        history, if held). ``note`` says why there is no history."""
        now = now or datetime.now(UTC)
        title = self.query_one("#stats-title", Static)
        if agent is None:
            title.update(Text("No agent selected", style="dim"))
            for key, label, _ in GRAPHS:
                self.query_one(f"#stat-{key}", Static).update(Text(label, style="bold"))
                self.query_one(f"#spark-{key}", Spark).show((), now, step)
            self.query_one("#stats-details", Static).update("")
            return
        if not note:
            note = "last hour" if agent.online else "offline: last known values"
        # Text, not markup: hostnames are the machines' own.
        title.update(Text.assemble((agent.label, "bold"), ("  " + note, "dim")))
        values = {
            "cpu": formatting.cpu(agent.cpu_percent),
            "ram": formatting.usage(agent.mem_used_bytes, agent.mem_total_bytes),
            "disk": formatting.usage(agent.disk_used_bytes, agent.disk_total_bytes),
        }
        for key, label, value_of in GRAPHS:
            self.query_one(f"#stat-{key}", Static).update(
                Text.assemble((label, "bold"), "  ", values[key])
            )
            points = [(s.ts, value_of(s)) for s in samples or ()]
            self.query_one(f"#spark-{key}", Spark).show(points, now, step)
        self.query_one("#stats-details", Static).update(details(agent))
