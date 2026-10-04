"""The TUI offering to update itself from the server."""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

import httpx
import pytest
from textual.widgets import Static

from tetanus_rmm import __version__, selfupdate
from tetanus_rmm.api import ApiClient, ApiError, TuiBuild
from tetanus_rmm.auth import StoredSession, TokenStore
from tetanus_rmm.screens import MainScreen
from tetanus_rmm.selfupdate import PendingUpdate, UpdateScreen

from .conftest import BASE, FakeServer, MemoryKeyring
from .test_app import LATER, make_app, serve_agents, wait_for

WHEEL = b"a newer wheel"
NAME = "tetanus_rmm-99.0.0-py3-none-any.whl"


def publish(server: FakeServer, version: str = "99.0.0", content: bytes = WHEEL) -> None:
    name = f"tetanus_rmm-{version}-py3-none-any.whl"
    server.on(
        "GET",
        "/api/tui/manifest",
        body={
            "file": name,
            "version": version,
            "sha256": hashlib.sha256(WHEEL).hexdigest(),
            "size": len(WHEEL),
        },
    )
    server.on("GET", f"/install/{name}", handler=lambda _r: httpx.Response(200, content=content))


def test_only_a_later_release_counts_as_newer() -> None:
    assert selfupdate.is_newer("0.1.7", "0.1.6")
    assert selfupdate.is_newer("0.2", "0.1.9")
    assert selfupdate.is_newer("0.10.0", "0.9.0")
    assert selfupdate.is_newer("1.0.0.1", "1.0")
    assert not selfupdate.is_newer("0.1.6", "0.1.6")
    assert not selfupdate.is_newer("0.1.6.0", "0.1.6")
    assert not selfupdate.is_newer("0.1.5", "0.1.6")
    # Pre-releases and anything unreadable are never offered.
    assert not selfupdate.is_newer("0.2.0-rc1", "0.1.6")
    assert not selfupdate.is_newer("0.2.0rc1", "0.1.6")
    assert not selfupdate.is_newer("", "0.1.6")
    assert not selfupdate.is_newer("0.2.0", "0.1.6.dev1")
    assert not selfupdate.is_newer(__version__)


def test_the_install_command_follows_how_the_tui_was_installed(monkeypatch, tmp_path) -> None:
    wheel = tmp_path / NAME
    monkeypatch.setattr(selfupdate.shutil, "which", lambda name: f"/usr/bin/{name}")
    uv = tmp_path / "share" / "uv" / "tools" / "tetanus-rmm"
    assert selfupdate.install_command(wheel, uv) == ["uv", "tool", "install", "--force", str(wheel)]
    pipx = tmp_path / "pipx" / "venvs" / "tetanus-rmm"
    assert selfupdate.install_command(wheel, pipx) == ["pipx", "install", "--force", str(wheel)]
    # Any other environment: pip in it, if it has one.
    venv = tmp_path / "venv"
    monkeypatch.setattr(selfupdate.importlib.util, "find_spec", lambda name: object())
    pip = [sys.executable, "-m", "pip", "install", "--upgrade", str(wheel)]
    assert selfupdate.install_command(wheel, venv) == pip
    # uv's layout without uv on PATH falls back to pip too.
    monkeypatch.setattr(selfupdate.shutil, "which", lambda name: None)
    assert selfupdate.install_command(wheel, uv) == pip
    monkeypatch.setattr(selfupdate.importlib.util, "find_spec", lambda name: None)
    assert selfupdate.install_command(wheel, venv) is None
    # The tests run from the source tree, which is updated with git.
    assert selfupdate.is_source_checkout()


async def test_the_api_reports_the_published_tui_or_none(server: FakeServer, api: ApiClient):
    assert await api.tui_build() is None
    publish(server)
    build = await api.tui_build()
    assert build == TuiBuild(NAME, "99.0.0", hashlib.sha256(WHEEL).hexdigest(), len(WHEEL))
    assert "authorization" not in server.requests[-1].headers
    server.on("GET", "/api/tui/manifest", body={"file": NAME})
    with pytest.raises(ApiError, match="malformed"):
        await api.tui_build()


async def test_a_download_is_checked_against_the_manifest(
    server: FakeServer, api: ApiClient, tmp_path: Path
) -> None:
    publish(server)
    build = await api.tui_build()
    stale = tmp_path / "updates" / "tetanus_rmm-0.0.1-py3-none-any.whl"
    stale.parent.mkdir()
    stale.write_bytes(b"old")
    wheel = await selfupdate.download(api, build, tmp_path / "updates")
    assert wheel.name == NAME and wheel.read_bytes() == WHEEL
    assert not stale.exists()

    publish(server, content=b"tampered with")
    with pytest.raises(ApiError, match="does not match"):
        await selfupdate.download(api, build, tmp_path / "updates")
    assert not wheel.exists()
    # A manifest cannot name a file outside the directory.
    bad = TuiBuild("../evil.whl", "99.0.0", build.sha256, 1)
    with pytest.raises(ApiError, match="malformed"):
        await selfupdate.download(api, bad, tmp_path / "updates")


def signed_in(tmp_path: Path, server: FakeServer, **config):
    kr, launched = MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    app = make_app(server, kr, tmp_path, launched)
    app.config = app.config.with_overrides(**config)
    return app


async def test_a_newer_tui_is_offered_at_sign_in_and_taken(tmp_path: Path) -> None:
    server = FakeServer()
    publish(server)
    app = signed_in(tmp_path, server)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, UpdateScreen))
        text = " ".join(str(s.render()) for s in app.screen.query(Static))
        assert "99.0.0 is available" in text and f"You have {__version__}" in text
        await pilot.click("#update-now")
        await wait_for(pilot, lambda: app.return_value is not None)
    wheel = tmp_path / "updates" / NAME
    assert app.return_value == PendingUpdate("99.0.0", wheel)
    assert wheel.read_bytes() == WHEEL


async def test_later_keeps_working_and_a_bad_download_says_so(tmp_path: Path) -> None:
    server = FakeServer()
    publish(server, content=b"tampered with")
    app = signed_in(tmp_path, server)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, UpdateScreen))
        await pilot.click("#update-now")
        status = app.screen.query_one("#update-status", Static)
        await wait_for(pilot, lambda: "does not match" in str(status.render()))
        assert app.return_value is None
        await pilot.press("escape")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
    assert app.return_value is None


@pytest.mark.parametrize("published", [None, "0.0.1", __version__])
async def test_nothing_newer_is_silent_until_asked(tmp_path: Path, published: str | None) -> None:
    server = FakeServer()
    if published:
        publish(server, version=published)
    app = signed_in(tmp_path, server)
    notes: list[str] = []
    app.notify = lambda message, **_kw: notes.append(message)  # type: ignore[method-assign]
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(
            pilot, lambda: any(r.url.path == "/api/tui/manifest" for r in server.requests)
        )
        await pilot.pause(0.1)
        assert isinstance(app.screen, MainScreen) and not notes
        await pilot.press("U")
        await wait_for(pilot, lambda: notes)
        assert notes == [f"You are up to date ({__version__})."]


async def test_the_automatic_check_can_be_turned_off(tmp_path: Path) -> None:
    server = FakeServer()
    publish(server)
    app = signed_in(tmp_path, server, check_updates=False)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await pilot.pause(0.2)
        assert isinstance(app.screen, MainScreen)
        assert all(r.url.path != "/api/tui/manifest" for r in server.requests)
        # By hand it still works.
        await pilot.press("U")
        await wait_for(pilot, lambda: isinstance(app.screen, UpdateScreen))


def test_finishing_installs_then_restarts_or_says_how(monkeypatch, tmp_path, capsys) -> None:
    update = PendingUpdate("99.0.0", tmp_path / NAME)
    ran: list[list[str]] = []
    execs: list[tuple[str, list[str]]] = []
    monkeypatch.setattr(selfupdate, "is_source_checkout", lambda: False)
    monkeypatch.setattr(selfupdate, "install_command", lambda wheel: ["uv", "tool", str(wheel)])
    monkeypatch.setattr(selfupdate.shutil, "which", lambda name: f"/bin/{name}")
    monkeypatch.setattr(selfupdate.os, "execv", lambda path, argv: execs.append((path, argv)))
    code = 0

    def run(command, check):
        ran.append(command)
        return type("Done", (), {"returncode": code})()

    monkeypatch.setattr(selfupdate.subprocess, "run", run)
    monkeypatch.setattr(selfupdate.sys, "platform", "linux")
    selfupdate.finish(update, ["tetanus-rmm", "--server-url", BASE])
    assert ran == [["uv", "tool", str(update.wheel)]]
    assert execs[0] == ("/bin/tetanus-rmm", ["/bin/tetanus-rmm", "--server-url", BASE])

    # A failed install leaves this version running and says what to run.
    code, execs[:] = 1, []
    with pytest.raises(SystemExit):
        selfupdate.finish(update, ["tetanus-rmm"])
    assert not execs and f"uv tool {update.wheel}" in capsys.readouterr().out

    # Windows and source checkouts are never installed over from here.
    ran.clear()
    monkeypatch.setattr(selfupdate.sys, "platform", "win32")
    selfupdate.finish(update, ["tetanus-rmm"])
    monkeypatch.setattr(selfupdate.sys, "platform", "linux")
    monkeypatch.setattr(selfupdate, "is_source_checkout", lambda: True)
    selfupdate.finish(update, ["tetanus-rmm"])
    assert not ran and not execs
    assert capsys.readouterr().out.count("To install it, run:") == 2
