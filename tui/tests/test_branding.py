"""Company branding: the API client and the screen admins set it with."""

from __future__ import annotations

import base64
import json
from pathlib import Path

import httpx
import pytest
from textual.widgets import Button, Checkbox, Input, Static

from tetanus_rmm.api import ApiClient, ApiError, Branding
from tetanus_rmm.auth import StoredSession, TokenStore
from tetanus_rmm.branding import (
    MAX_LOGO_BYTES,
    BrandingScreen,
    normalise_colour,
    read_logo,
)
from tetanus_rmm.screens import MainScreen

from .conftest import BASE, FakeServer, MemoryKeyring
from .test_app import LATER, make_app, serve_agents, wait_for

PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 40
ADMIN = {"id": 1, "username": "root", "role": "admin"}


def test_colours_are_normalised_or_refused() -> None:
    assert normalise_colour("b5441c") == "#B5441C"
    assert normalise_colour(" #0b5CAD ") == "#0B5CAD"
    for bad in ["", "blue", "#12345", "#1234567", "#GGGGGG"]:
        assert normalise_colour(bad) is None


def test_a_logo_must_be_a_png_that_is_not_too_large(tmp_path: Path) -> None:
    logo = tmp_path / "logo.png"
    logo.write_bytes(PNG)
    assert read_logo(str(logo)) == PNG
    with pytest.raises(ValueError, match="cannot read"):
        read_logo(str(tmp_path / "missing.png"))
    (tmp_path / "logo.gif").write_bytes(b"GIF89a")
    with pytest.raises(ValueError, match="not a PNG"):
        read_logo(str(tmp_path / "logo.gif"))
    big = tmp_path / "big.png"
    big.write_bytes(PNG + b"\x00" * MAX_LOGO_BYTES)
    with pytest.raises(ValueError, match="may be 128 KiB"):
        read_logo(str(big))


async def test_the_api_reads_sets_and_resets_branding(server: FakeServer, api: ApiClient) -> None:
    # An older server has no such route: nothing set.
    assert await api.branding() is None
    server.on("GET", "/api/branding", body=None)
    assert await api.branding() is None
    assert "authorization" not in server.requests[-1].headers
    stored = {"name": "Contoso IT", "accent": "#0B5CAD", "logo_png": base64.b64encode(PNG).decode()}
    server.on("GET", "/api/branding", body=stored)
    contoso = Branding("Contoso IT", "#0B5CAD", PNG)
    assert await api.branding() == contoso

    api.token = "sess"
    server.on("PUT", "/api/branding", handler=lambda r: httpx.Response(200, content=r.content))
    assert await api.set_branding(contoso) == contoso
    assert server.body() == stored
    plain = Branding("Contoso")
    assert await api.set_branding(plain) == plain
    assert server.body() == {"name": "Contoso", "accent": None, "logo_png": None}
    server.on("DELETE", "/api/branding", status=200)
    await api.reset_branding()
    assert server.requests[-1].method == "DELETE"
    server.on("GET", "/api/branding", body={"accent": "#000000"})
    with pytest.raises(ApiError, match="malformed"):
        await api.branding()


class BrandServer(FakeServer):
    """Keeps the branding it is given, like the real one."""

    def __init__(self) -> None:
        super().__init__()
        self.stored: dict | None = None
        self.on("GET", "/api/branding", handler=lambda _r: httpx.Response(200, json=self.stored))
        self.on("PUT", "/api/branding", handler=self._put)
        self.on("DELETE", "/api/branding", handler=self._delete)

    def _put(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        if body.get("accent") == "#FFEE00":
            reason = "the accent colour is too light: white text must be readable on it"
            return httpx.Response(400, json={"error": reason})
        self.stored = body
        return httpx.Response(200, json=body)

    def _delete(self, _request: httpx.Request) -> httpx.Response:
        self.stored = None
        return httpx.Response(200)


def signed_in(tmp_path: Path, server: FakeServer, user: dict):
    kr = MemoryKeyring()
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, user["username"]))
    serve_agents(server, user=user)
    return make_app(server, kr, tmp_path, [])


async def test_an_admin_sets_and_resets_the_branding(tmp_path: Path) -> None:
    server = BrandServer()
    logo = tmp_path / "logo.png"
    logo.write_bytes(PNG)
    app = signed_in(tmp_path, server, ADMIN)
    async with app.run_test(size=(140, 44)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await pilot.press("B")
        await wait_for(pilot, lambda: isinstance(app.screen, BrandingScreen))
        screen = app.screen
        text = lambda id: str(screen.query_one(id, Static).render())  # noqa: E731

        async def click(id: str) -> None:
            # A status line appearing moves the buttons: let the layout settle.
            await pilot.pause(0.3)
            await pilot.click(id)

        await wait_for(pilot, lambda: "Not set" in text("#br-current"))
        assert screen.query_one("#br-reset", Button).disabled

        # A name is needed, and a colour must be one.
        await click("#br-save")
        await wait_for(pilot, lambda: "company's name" in text("#br-status"))
        screen.query_one("#br-name", Input).value = "Contoso IT"
        screen.query_one("#br-accent", Input).value = "blue"
        await click("#br-save")
        await wait_for(pilot, lambda: "#B5441C" in text("#br-status"))
        assert server.stored is None

        # The server's refusal is shown as it gives it.
        screen.query_one("#br-accent", Input).value = "#ffee00"
        await click("#br-save")
        await wait_for(pilot, lambda: "too light" in text("#br-status"))
        assert server.stored is None

        screen.query_one("#br-accent", Input).value = "0b5cad"
        screen.query_one("#br-logo", Input).value = str(logo)
        await click("#br-save")
        await wait_for(pilot, lambda: server.stored is not None)
        assert server.stored == {
            "name": "Contoso IT",
            "accent": "#0B5CAD",
            "logo_png": base64.b64encode(PNG).decode(),
        }
        await wait_for(pilot, lambda: "with a logo" in text("#br-current"))
        assert "Saved" in text("#br-status")

        # Saving again without a path keeps the logo; "No logo" drops it.
        screen.query_one("#br-name", Input).value = "Contoso"
        await click("#br-save")
        await wait_for(pilot, lambda: server.stored["name"] == "Contoso")
        assert server.stored["logo_png"] is not None
        screen.query_one("#br-no-logo", Checkbox).value = True
        await click("#br-save")
        await wait_for(pilot, lambda: server.stored["logo_png"] is None)
        await wait_for(pilot, lambda: "TetanusRMM mark" in text("#br-current"))

        await click("#br-reset")
        await wait_for(pilot, lambda: server.stored is None)
        await wait_for(pilot, lambda: "Not set" in text("#br-current"))
        assert "TetanusRMM again" in text("#br-status")
        await pilot.press("escape")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))


async def test_branding_is_not_offered_to_support_engineers(tmp_path: Path) -> None:
    server = BrandServer()
    app = signed_in(tmp_path, server, {"id": 7, "username": "jane", "role": "support_engineer"})
    async with app.run_test(size=(140, 44)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        assert app.screen.check_action("branding", ()) is False
        await pilot.press("B")
        await pilot.pause(0.2)
        assert isinstance(app.screen, MainScreen)
