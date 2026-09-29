"""Script runner: result handling and the saved-script library."""

from __future__ import annotations

import json

import pytest

from rmm_tui.scripts import (
    DEFAULT_SCRIPTS,
    AgentResult,
    SavedScript,
    ScriptError,
    ScriptLibrary,
    ScriptRun,
    validate_timeout,
)


def result(agent_id: str, status: str, exit_code=None, **kw) -> dict:
    return {
        "agent_id": agent_id,
        "status": status,
        "exit_code": exit_code,
        "stdout": kw.get("stdout", ""),
        "stderr": kw.get("stderr", ""),
        "stdout_truncated": kw.get("stdout_truncated", False),
        "stderr_truncated": False,
        "duration_ms": kw.get("duration_ms"),
        "error": kw.get("error"),
    }


def test_a_new_run_normalizes_targets_and_starts_running() -> None:
    run = ScriptRun([" b ", "a", "b", ""], "hostname")
    assert run.agent_ids == ["b", "a"]
    assert [r.status for r in run.ordered()] == ["running", "running"]
    assert not run.done
    assert run.summary() == "running on 2 agent(s)…"
    assert run.ordered()[0].output_text() == "(still running)"


@pytest.mark.parametrize(
    ("agents", "script", "message"),
    [([], "hostname", "choose at least one agent"), (["a"], "   \n", "enter a script")],
)
def test_a_run_needs_targets_and_a_script(agents, script, message) -> None:
    with pytest.raises(ScriptError, match=message):
        ScriptRun(agents, script)


def test_report_fills_every_agent_in_request_order() -> None:
    run = ScriptRun(["ws1", "ws2", "srv", "old", "gone", "missing"], "Get-Date")
    run.apply_report(
        {
            "run_id": 41,
            "summary": {"total": 5, "succeeded": 1, "failed": 2, "not_run": 2},
            # Report order differs from request order; "missing" is absent.
            "results": [
                result("srv", "timed_out", None, stdout="partial", duration_ms=60000),
                result("ws2", "completed", 3, stderr="boom", duration_ms=250),
                result(
                    "ws1", "completed", 0, stdout="ok\n", stdout_truncated=True, duration_ms=1234
                ),
                result("gone", "offline", error="agent is not connected"),
                result(
                    "old", "unsupported", error="agent version does not support remote operations"
                ),
            ],
        }
    )
    assert run.done and run.run_id == 41
    rows = [(r.agent_id, r.status_label, r.exit_label, r.duration_label) for r in run.ordered()]
    assert rows == [
        ("ws1", "ok", "0", "1.2s"),
        ("ws2", "nonzero exit", "3", "250ms"),
        ("srv", "timed out", "", "60.0s"),
        ("old", "unsupported", "", ""),
        ("gone", "offline", "", ""),
        ("missing", "failed", "", ""),
    ]
    assert run.results["missing"].error == "no result in the report"
    assert run.summary() == "1/6 succeeded, 2 failed, 3 not run (run 41)"

    ws1, ws2 = run.results["ws1"], run.results["ws2"]
    assert ws1.succeeded and not ws2.succeeded
    assert ws1.output_text() == "── stdout ──\nok\n[stdout truncated]"
    assert ws2.output_text() == "── stderr ──\nboom"
    assert run.results["gone"].output_text() == "error: agent is not connected"


def test_two_agents_both_succeeding() -> None:
    run = ScriptRun(["a", "b"], "hostname")
    run.apply_report(
        {
            "run_id": 1,
            "results": [
                result("a", "completed", 0, stdout="A"),
                result("b", "completed", 0, stdout="B"),
            ],
        }
    )
    assert run.summary() == "2/2 succeeded (run 1)"
    assert [r.stdout for r in run.ordered()] == ["A", "B"]


def test_a_failed_request_fails_every_target() -> None:
    run = ScriptRun(["a", "b"], "hostname")
    run.fail("forbidden")
    assert run.done and run.error == "forbidden"
    assert {r.status for r in run.ordered()} == {"failed"}
    assert run.results["a"].output_text() == "error: forbidden"
    assert run.summary() == "0/2 succeeded, 2 not run"


def test_completed_with_no_output() -> None:
    r = AgentResult.from_json(result("a", "completed", 0))
    assert r.output_text() == "(no output)"


@pytest.mark.parametrize(("raw", "expected"), [("", None), (" 90 ", 90), ("3600", 3600)])
def test_timeout_field(raw, expected) -> None:
    assert validate_timeout(raw) == expected


@pytest.mark.parametrize("raw", ["0", "3601", "ten", "-5"])
def test_bad_timeouts(raw) -> None:
    with pytest.raises(ScriptError):
        validate_timeout(raw)


# --- library -------------------------------------------------------------------


def test_library_starts_with_examples_and_saves_replaces_deletes(tmp_path) -> None:
    lib = ScriptLibrary(tmp_path / "sub" / "scripts.json")
    assert [s.name for s in lib.load()] == sorted(s["name"] for s in DEFAULT_SCRIPTS)

    lib.save(SavedScript("  Restart spooler ", "Restart-Service Spooler", 60))
    lib.save(SavedScript("Restart spooler", "Restart-Service -Force Spooler"))
    names = [s.name for s in lib.load()]
    assert names.count("Restart spooler") == 1
    spooler = next(s for s in lib.load() if s.name == "Restart spooler")
    assert spooler.body == "Restart-Service -Force Spooler" and spooler.timeout_secs is None

    lib.delete("Restart spooler")
    assert "Restart spooler" not in [s.name for s in lib.load()]
    # Stored as plain JSON, so it can be edited or shared by hand.
    assert isinstance(json.loads(lib.path.read_text()), list)


def test_library_rejects_unnamed_or_empty_scripts(tmp_path) -> None:
    lib = ScriptLibrary(tmp_path / "s.json")
    with pytest.raises(ScriptError, match="name"):
        lib.save(SavedScript(" ", "x"))
    with pytest.raises(ScriptError, match="empty"):
        lib.save(SavedScript("x", "  "))


def test_corrupt_library_is_reported(tmp_path) -> None:
    path = tmp_path / "s.json"
    path.write_text("{not json")
    with pytest.raises(ScriptError, match="cannot read"):
        ScriptLibrary(path).load()
    path.write_text('[{"body": "no name"}]')
    with pytest.raises(ScriptError, match="not a script library"):
        ScriptLibrary(path).load()
