"""Unlocking the Bitwarden vault for the viewers, against a stand-in ``bw``."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from textual.widgets import Input, Static

from tetanus_rmm import bitwarden
from tetanus_rmm.auth import StoredSession, TokenStore
from tetanus_rmm.bitwarden import BitwardenError, UnlockScreen, WrongPassword
from tetanus_rmm.screens import LoginScreen, MainScreen

from .conftest import BASE, FakeServer, MemoryKeyring
from .test_app import LATER, make_app, serve_agents, wait_for

pytestmark = pytest.mark.skipif(sys.platform == "win32", reason="the stand-in bw is a shell script")

#: Records how it was called and answers like the real one.
FAKE_BW = r"""#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/argv"
[ "$BW_NOINTERACTION" = true ] || { echo "would prompt" >&2; exit 1; }
[ -e "$dir/logged-out" ] && { echo "You are not logged in." >&2; exit 1; }
case "$1" in
  status)
    if [ "$BW_SESSION" = KEY ]; then echo '{"status":"unlocked"}'
    else echo '{"status":"locked"}'; fi ;;
  unlock)
    [ "$2 $3 $4" = "--raw --passwordenv RMM_BW_PASSWORD" ] || exit 2
    if [ "$RMM_BW_PASSWORD" = "correct horse" ]; then printf KEY
    else echo "Invalid master password." >&2; exit 1; fi ;;
  lock) echo "Your vault is locked." ;;
  *) echo "Not found." >&2; exit 1 ;;
esac
"""


@pytest.fixture
def bw(tmp_path: Path) -> str:
    path = tmp_path / "bw"
    path.write_text(FAKE_BW)
    path.chmod(0o700)
    return str(path)


def argv(bw: str) -> str:
    return Path(bw).with_name("argv").read_text()


def test_unlock_gives_a_session_key_and_keeps_secrets_off_the_command_line(bw: str) -> None:
    assert bitwarden.status(bw) == "locked"
    with pytest.raises(WrongPassword):
        bitwarden.unlock(bw, "wrong")
    session = bitwarden.unlock(bw, "correct horse")
    assert session == "KEY"
    assert bitwarden.status(bw, session) == "unlocked"
    bitwarden.lock(bw)
    calls = argv(bw)
    assert "unlock --raw --passwordenv RMM_BW_PASSWORD" in calls
    assert "correct horse" not in calls and "KEY" not in calls


def test_a_missing_or_signed_out_client_says_what_to_do(bw: str, tmp_path: Path) -> None:
    with pytest.raises(BitwardenError, match="install bw"):
        bitwarden.status(str(tmp_path / "nowhere"))
    (tmp_path / "logged-out").touch()
    with pytest.raises(BitwardenError, match="bw login"):
        bitwarden.unlock(bw, "correct horse")


async def test_unlocking_in_the_tui_hands_viewers_the_key_and_quitting_locks(
    tmp_path: Path, bw: str
) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    server.on(
        "POST",
        "/api/agents/agt-1/viewer-sessions",
        body={
            "token": "vtok",
            "expires_at": LATER.isoformat(),
            "agent_id": "agt-1",
            "online": True,
        },
    )
    app = make_app(server, kr, tmp_path, launched)
    app.config = app.config.with_overrides(bw_path=bw)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        main = app.screen
        await pilot.press("b")
        await wait_for(pilot, lambda: isinstance(app.screen, UnlockScreen))

        # A wrong password says so and asks again; the field is emptied.
        app.screen.query_one("#bw-password", Input).value = "wrong"
        await pilot.press("enter")
        status = app.screen.query_one("#bw-status", Static)
        await wait_for(pilot, lambda: "Wrong master password" in str(status.render()))
        assert app.screen.query_one("#bw-password", Input).value == ""
        assert app.bw_session is None

        app.screen.query_one("#bw-password", Input).value = "correct horse"
        await pilot.press("enter")
        await wait_for(pilot, lambda: app.bw_session == "KEY")
        await wait_for(pilot, lambda: app.screen is main)
        assert "vault unlocked" in str(main.query_one("#whoami", Static).render())

        await pilot.press("d")
        await wait_for(pilot, lambda: launched)
        assert launched[0].env["RMM_BW_SESSION"] == "KEY"
        assert launched[0].env["RMM_BW"] == bw
        assert "KEY" not in launched[0].argv
        assert "\nlock" not in argv(bw)
    # Quitting locks the vault again.
    assert argv(bw).splitlines()[-1] == "lock"
    assert app.bw_session is None
    # Nothing of it was written to the TUI's files.
    for path in tmp_path.rglob("*"):
        if path.is_file() and path.name not in ("bw", "argv"):
            text = path.read_text(errors="replace")
            assert "correct horse" not in text and "KEY" not in text, path


async def test_b_locks_an_unlocked_vault_and_signing_out_locks_too(tmp_path: Path, bw: str) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    server.on("POST", "/api/auth/logout", status=204)
    app = make_app(server, kr, tmp_path, launched)
    app.config = app.config.with_overrides(bw_path=bw)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        app.bw_session = "KEY"
        await pilot.press("b")
        await wait_for(pilot, lambda: app.bw_session is None)
        await wait_for(pilot, lambda: Path(bw).with_name("argv").exists())
        assert argv(bw).splitlines() == ["lock"]

        app.bw_session = "KEY"
        await pilot.press("l")
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        assert app.bw_session is None
        assert argv(bw).splitlines() == ["lock", "lock"]
