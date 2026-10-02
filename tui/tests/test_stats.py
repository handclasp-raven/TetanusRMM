"""The stats panel: its history cache, its graphs, and how the agent table
drives it against a mocked server (headless)."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta

from textual.widgets import Static

from tetanus_rmm.api import TelemetryHistory, TelemetrySample
from tetanus_rmm.auth import StoredSession, TokenStore
from tetanus_rmm.stats import Spark, StatsPanel, TelemetryCache, sparkline

from .conftest import BASE, FakeServer, MemoryKeyring, agent_json
from .test_app import LATER, make_app, serve_agents, wait_for

NOW = datetime(2026, 10, 3, 12, tzinfo=UTC)


def sample(minutes_ago: float, cpu: float = 50.0) -> TelemetrySample:
    return TelemetrySample(
        ts=NOW - timedelta(minutes=minutes_ago),
        cpu_percent=cpu,
        mem_used_bytes=4 << 30,
        mem_total_bytes=16 << 30,
        disk_used_bytes=100 << 30,
        disk_total_bytes=400 << 30,
    )


def history(*samples: TelemetrySample) -> TelemetryHistory:
    return TelemetryHistory(step_secs=30, samples=samples)


def test_sparkline_scales_to_the_width_and_leaves_gaps() -> None:
    start, end = NOW - timedelta(hours=1), NOW
    points = [(NOW - timedelta(minutes=59), 0.0), (NOW - timedelta(minutes=1), 100.0)]
    assert sparkline(points, start, end, 4) == "▁  █"
    # The highest value of each bar's time; out-of-range values stay in range.
    points = [(NOW - timedelta(minutes=50), 10.0), (NOW - timedelta(minutes=40), 250.0)]
    assert sparkline(points, start, end, 2) == "█ "
    # Samples outside the window are not drawn.
    assert sparkline([(NOW - timedelta(hours=2), 50.0)], start, end, 3) == "   "
    # No more bars than there can be samples.
    assert len(sparkline([], start, end, 500, step=30)) == 120
    assert sparkline(points, start, end, 0) == ""


def test_cache_is_fresh_for_a_step_then_merges_what_is_newer() -> None:
    clock = [0.0]
    cache = TelemetryCache(clock=lambda: clock[0])
    assert cache.get("a") is None and not cache.fresh("a") and cache.last("a") is None

    cache.store("a", history(sample(90), sample(10, 10.0), sample(5, 20.0)), now=NOW)
    # Older than the window: dropped.
    assert [s.cpu_percent for s in cache.get("a")] == [10.0, 20.0]
    assert cache.fresh("a") and cache.step("a") == 30
    assert cache.last("a") == NOW - timedelta(minutes=5)

    clock[0] = 31
    assert not cache.fresh("a")
    # The same sample again and a newer one: no duplicates, in order.
    cache.store("a", history(sample(1, 30.0), sample(5, 20.0)), now=NOW)
    assert [s.cpu_percent for s in cache.get("a")] == [10.0, 20.0, 30.0]
    assert cache.fresh("a")

    # A failed fetch keeps the samples and waits before the next try.
    clock[0] = 100
    cache.failed("a", retry_after=30)
    assert cache.fresh("a") and len(cache.get("a")) == 3
    cache.failed("b")
    assert cache.get("b") == [] and cache.last("b") is None


def test_cache_forgets_the_agents_looked_at_longest_ago() -> None:
    cache = TelemetryCache(capacity=2)
    cache.store("a", history(), now=NOW)
    cache.store("b", history(), now=NOW)
    cache.get("a")
    cache.store("c", history(), now=NOW)
    assert cache.get("b") is None
    assert cache.get("a") is not None and cache.get("c") is not None


# --- in the app -----------------------------------------------------------------


def serve_telemetry(server: FakeServer, *agent_ids: str) -> None:
    now = datetime.now(UTC)
    for agent_id in agent_ids:
        server.on(
            "GET",
            f"/api/agents/{agent_id}/telemetry",
            body={
                "step_secs": 30,
                "samples": [
                    {
                        "ts": (now - timedelta(minutes=m)).isoformat(),
                        "cpu_percent": 80.0,
                        "mem_used_bytes": 4 << 30,
                        "mem_total_bytes": 16 << 30,
                        "disk_used_bytes": 100 << 30,
                        "disk_total_bytes": 400 << 30,
                    }
                    for m in (30, 20, 10)
                ],
            },
        )


def telemetry_requests(server: FakeServer) -> list[str]:
    """The agents whose history was asked for, in order."""
    return [r.url.path.split("/")[3] for r in server.requests if r.url.path.endswith("/telemetry")]


def text(panel: StatsPanel, selector: str) -> str:
    return str(panel.query_one(selector, Static).render())


def stats_app(server: FakeServer, tmp_path):
    kr = MemoryKeyring()
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    return make_app(server, kr, tmp_path, [])


async def test_panel_shows_the_selected_agent_at_once_and_then_its_history(tmp_path) -> None:
    server = FakeServer()
    serve_agents(server)
    server.on(
        "GET",
        "/api/agents",
        body=[
            agent_json(
                "agt-1",
                hostname="WS-01",
                logged_in_users=["CORP\\alice"],
                disks=[{"name": "C:\\", "total_bytes": 400 << 30, "used_bytes": 100 << 30}],
            )
        ],
    )
    serve_telemetry(server, "agt-1")
    app = stats_app(server, tmp_path)
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: app.screen.query("#stats"))
        screen = app.screen
        panel = screen.query_one("#stats", StatsPanel)
        # From the agent list alone, before any history arrives.
        await wait_for(pilot, lambda: "WS-01" in text(panel, "#stats-title"))
        assert "25% of 16.0 GiB" in text(panel, "#stat-ram")
        assert "25% of 400.0 GiB" in text(panel, "#stat-disk")
        details = text(panel, "#stats-details")
        assert "Uptime 1h 0m" in details and "CORP\\alice" in details and "C:\\ 25%" in details

        await wait_for(pilot, lambda: screen.stats_cache.get("agt-1"))
        await wait_for(pilot, lambda: not panel.loading_history)
        cpu = str(panel.query_one("#spark-cpu", Spark).render())
        assert "▇" in cpu and cpu.startswith(" "), repr(cpu)
        assert "last hour" in text(panel, "#stats-title")
        assert telemetry_requests(server) == ["agt-1"]


async def test_moving_through_the_list_fetches_only_where_the_selection_rests(tmp_path) -> None:
    server = FakeServer()
    serve_agents(server)
    serve_telemetry(server, "agt-1", "agt-2", "agt-3")
    app = stats_app(server, tmp_path)
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: app.screen.query("#stats"))
        screen = app.screen
        screen.STATS_DEBOUNCE = 0.6
        panel = screen.query_one("#stats", StatsPanel)
        await wait_for(pilot, lambda: screen.stats_cache.get("agt-1"))

        # Past WS-02 to OLD without stopping: WS-02 is never asked for.
        await pilot.press("down", "down")
        assert "OLD" in text(panel, "#stats-title") and panel.loading_history
        assert "offline" in text(panel, "#stats-title")
        await wait_for(pilot, lambda: screen.stats_cache.get("agt-3"))
        assert telemetry_requests(server) == ["agt-1", "agt-3"]

        # Back to an agent whose history is fresh: shown from the cache.
        await pilot.press("up", "up")
        assert "WS-01" in text(panel, "#stats-title") and not panel.loading_history
        assert "▇" in str(panel.query_one("#spark-cpu", Spark).render())
        await pilot.pause(0.8)
        assert telemetry_requests(server) == ["agt-1", "agt-3"]

        # Once a new sample can exist, only what is newer is asked for.
        screen.stats_cache._clock = lambda: 1e12
        await pilot.press("down", "up")
        await wait_for(pilot, lambda: len(telemetry_requests(server)) == 3)
        assert telemetry_requests(server)[-1] == "agt-1"
        assert server.requests[-1].url.params["since"].endswith("Z")


async def test_hidden_panel_fetches_nothing_and_stays_hidden(tmp_path) -> None:
    server = FakeServer()
    serve_agents(server)
    serve_telemetry(server, "agt-1", "agt-2", "agt-3")
    app = stats_app(server, tmp_path)
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: app.screen.query("#stats"))
        screen = app.screen
        panel = screen.query_one("#stats", StatsPanel)
        await wait_for(pilot, lambda: screen.stats_cache.get("agt-1"))

        await pilot.press("p")
        assert not panel.display
        assert json.loads((tmp_path / "state.json").read_text())["stats_panel"] is False
        await pilot.press("down")
        await pilot.pause(0.6)
        assert telemetry_requests(server) == ["agt-1"]

        # Shown again: it loads the agent now selected.
        await pilot.press("p")
        assert panel.display and "WS-02" in text(panel, "#stats-title")
        await wait_for(pilot, lambda: screen.stats_cache.get("agt-2"))
        assert telemetry_requests(server) == ["agt-1", "agt-2"]

    # The next launch starts with it as it was left.
    app = stats_app(server, tmp_path)
    app.state.stats_panel = False
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: app.screen.query("#stats"))
        assert not app.screen.query_one("#stats", StatsPanel).display


async def test_older_server_and_failures_leave_the_current_values(tmp_path) -> None:
    server = FakeServer()
    serve_agents(server)
    server.on("GET", "/api/agents/agt-2/telemetry", status=500, body={"error": "database down"})
    app = stats_app(server, tmp_path)
    async with app.run_test(size=(160, 40)) as pilot:
        await wait_for(pilot, lambda: app.screen.query("#stats"))
        screen = app.screen
        panel = screen.query_one("#stats", StatsPanel)
        # No such route: said once, and not asked again for any agent.
        await wait_for(pilot, lambda: screen.stats_unavailable)
        assert "no history on this server" in text(panel, "#stats-title")
        assert "25% of 16.0 GiB" in text(panel, "#stat-ram")
        await pilot.press("down")
        await pilot.pause(0.5)
        assert telemetry_requests(server) == ["agt-1"]
        assert not panel.loading_history

        # A failure is shown in the panel, and not retried at once.
        screen.stats_unavailable = False
        await pilot.press("up", "down")
        await wait_for(pilot, lambda: "database down" in text(panel, "#stats-title"))
        assert not panel.loading_history and screen.stats_cache.fresh("agt-2")
