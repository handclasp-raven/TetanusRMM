"""First-run setup without a config: accepting the server's CA at sign-in,
and getting the viewer and its CA from the server."""

from __future__ import annotations

import hashlib
import ssl
from pathlib import Path

import httpx
import pytest
from textual.widgets import DataTable, Static

from rmm_tui import provision
from rmm_tui.__main__ import main
from rmm_tui.api import ApiClient, ApiError, UntrustedServer, cert_failure
from rmm_tui.app import RmmApp
from rmm_tui.auth import SessionManager, StoredSession, TokenStore
from rmm_tui.config import Config
from rmm_tui.screens import LoginScreen, MainScreen, TrustScreen
from rmm_tui.scripts import ScriptLibrary
from rmm_tui.state import UiState
from rmm_tui.trust import TrustStore, fingerprint

from .conftest import BASE, FakeServer, MemoryKeyring
from .test_app import LATER, FakeProcess, serve_agents, serve_login, sign_in, wait_for

# Certificate-shaped filler: these tests only fingerprint and store it.
CA_PEM = """-----BEGIN CERTIFICATE-----
MIIBdDCCARqgAwIBAgIUQmJ8mJXcYJ5cYz6YlH0k3bX3v1EwCgYIKoZIzj0EAwIw
EjEQMA4GA1UEAwwHdGVzdC1jYTAeFw0yNjEwMDEwMDAwMDBaFw0zNjA5MjgwMDAw
MDBaMBIxEDAOBgNVBAMMB3Rlc3QtY2EwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNC
AAQvJq0m4r6X3VhX0o1m0mY5i0mE1l0b2xq2K0iJ8m7V2s1cQ3o2m8w3rYw2bq0a
l0q3n1o2f3n4t5q6r7s8t9u0o1MwUTAdBgNVHQ4EFgQUAAAAAAAAAAAAAAAAAAAA
AAAAAAAwHwYDVR0jBBgwFoAUAAAAAAAAAAAAAAAAAAAAAAAAAAAwDwYDVR0TAQH/
BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiEA
-----END CERTIFICATE-----
"""


def untrusted() -> httpx.ConnectError:
    """What httpx raises when the server's certificate does not verify."""
    error = httpx.ConnectError("certificate verify failed")
    error.__cause__ = ssl.SSLCertVerificationError(1, "certificate verify failed")
    return error


def bare_app(
    server: FakeServer,
    kr: MemoryKeyring,
    tmp_path: Path,
    launched: list,
    *,
    private_ca: bool = True,
    offered_ca: str = CA_PEM,
) -> RmmApp:
    """The app as freshly installed: no CA and no viewer configured. With
    ``private_ca`` the server only verifies against a CA file."""
    fetched: list[str] = []

    def factory(url: str, ca: Path | None) -> ApiClient:
        def handle(request: httpx.Request) -> httpx.Response:
            if private_ca and ca is None:
                raise untrusted()
            return server(request)

        return ApiClient(url, ca, transport=httpx.MockTransport(handle))

    async def fetch(url: str) -> str:
        fetched.append(url)
        return offered_ca

    def launcher(command, log_path):
        launched.append(command)
        return FakeProcess()

    trust = TrustStore(tmp_path / "servers")
    app = RmmApp(
        Config(server_url=BASE),
        SessionManager(factory(BASE, trust.pinned(BASE)), TokenStore(BASE, kr)),
        ScriptLibrary(tmp_path / "scripts.json"),
        launcher=launcher,
        log_dir=tmp_path,
        state=UiState.load(tmp_path / "state.json"),
        api_factory=factory,
        trust=trust,
        ca_fetcher=fetch,
    )
    app.fetched = fetched  # type: ignore[attr-defined]
    return app


def test_fingerprints_read_like_openssl() -> None:
    der = ssl.PEM_cert_to_DER_cert(CA_PEM)
    digest = hashlib.sha256(der).hexdigest().upper()
    shown = fingerprint(CA_PEM)
    assert shown.replace(":", "") == digest and len(shown.split(":")) == 32
    with pytest.raises(ValueError):
        fingerprint("not a certificate")


def test_trust_store_keeps_one_ca_per_server(tmp_path) -> None:
    store = TrustStore(tmp_path)
    assert store.pinned(BASE) is None and not store.forget(BASE)
    path = store.pin(BASE, CA_PEM)
    assert store.pinned(BASE) == path and path.read_text() == CA_PEM
    # Another port, or another host, is another server.
    assert store.pinned("https://rmm.test:9443") is None
    assert store.pinned("https://[::1]:8443") is None
    # The viewer's CA is never what the API is verified against.
    store.save_viewer_ca("https://public.test", CA_PEM)
    assert store.pinned("https://public.test") is None
    assert store.forget(BASE) and store.pinned(BASE) is None


async def test_certificate_failures_are_told_apart_from_other_errors() -> None:
    def handle(request: httpx.Request) -> httpx.Response:
        raise untrusted() if request.url.path == "/api/me" else httpx.ConnectError("refused")

    api = ApiClient(BASE, token="t", transport=httpx.MockTransport(handle))
    with pytest.raises(UntrustedServer):
        await api.me()
    with pytest.raises(ApiError) as other:
        await api.list_agents()
    assert not isinstance(other.value, UntrustedServer)
    assert cert_failure(httpx.ConnectError("refused")) is None


async def test_first_sign_in_asks_to_trust_the_servers_ca(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    serve_agents(server)
    serve_login(server)
    app = bare_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        await sign_in(pilot, app)
        await wait_for(pilot, lambda: isinstance(app.screen, TrustScreen))
        shown = str(app.screen.query_one("#trust-fingerprint", Static).render())
        assert shown.replace("\n", ":") == fingerprint(CA_PEM)
        assert not server.requests, "nothing is sent to a server not yet trusted"
        await pilot.click("#trust")
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
    pinned = TrustStore(tmp_path / "servers").pinned(BASE)
    assert pinned is not None and pinned.read_text() == CA_PEM
    assert kr.entries.get(("rmm-tui", BASE))

    # Next launch: the saved session is checked against the accepted CA,
    # with no prompt and no fetch.
    again = bare_app(server, kr, tmp_path, launched)
    async with again.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(again.screen, MainScreen))
    assert not again.fetched  # type: ignore[attr-defined]


async def test_declining_the_ca_does_not_sign_in_or_remember_it(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    serve_login(server)
    app = bare_app(server, kr, tmp_path, launched)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        await sign_in(pilot, app)
        await wait_for(pilot, lambda: isinstance(app.screen, TrustScreen))
        await pilot.press("escape")
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        await pilot.pause(0.05)
        assert "not trusted" in str(app.screen.query_one("#login-status", Static).render())
    assert TrustStore(tmp_path / "servers").pinned(BASE) is None
    assert not server.requests and not kr.entries


async def test_a_changed_certificate_is_an_error_not_a_new_prompt(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    serve_login(server)
    TrustStore(tmp_path / "servers").pin(BASE, CA_PEM)

    def factory(url: str, ca: Path | None) -> ApiClient:
        def handle(request: httpx.Request) -> httpx.Response:
            raise untrusted()  # even against the accepted CA

        return ApiClient(url, ca, transport=httpx.MockTransport(handle))

    app = bare_app(server, kr, tmp_path, launched)
    app.session.api = factory(BASE, None)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, LoginScreen))
        await sign_in(pilot, app)
        await pilot.pause(0.2)
        assert isinstance(app.screen, LoginScreen)
        status = str(app.screen.query_one("#login-status", Static).render())
        assert "no longer matches" in status and "--forget-ca" in status
    assert not app.fetched  # type: ignore[attr-defined]


def serve_viewer(server: FakeServer, binary: bytes, platform: str) -> None:
    server.on(
        "GET",
        f"/api/viewer/{platform}/manifest",
        body={
            "platform": platform,
            "version": "0.3.0",
            "sha256": hashlib.sha256(binary).hexdigest(),
            "size": len(binary),
        },
    )
    server.on(
        "GET",
        f"/api/viewer/{platform}/binary",
        handler=lambda _req: httpx.Response(200, content=binary),
    )


def paths(server: FakeServer) -> list[str]:
    return [r.url.path for r in server.requests]


async def test_the_viewer_is_downloaded_once_and_replaced_by_a_new_build(tmp_path, api, server):
    directory = tmp_path / "viewer"
    assert await provision.ensure_viewer(api, directory, "linux-x86_64") is None

    serve_viewer(server, b"viewer one", "linux-x86_64")
    announced = []
    first = await provision.ensure_viewer(api, directory, "linux-x86_64", announced.append)
    assert first is not None and first.read_bytes() == b"viewer one"
    assert first.stat().st_mode & 0o111, "executable"
    assert [b.version for b in announced] == ["0.3.0"]

    # Already there: only the manifest is asked for.
    server.requests.clear()
    assert await provision.ensure_viewer(api, directory, "linux-x86_64", announced.append) == first
    assert paths(server) == ["/api/viewer/linux-x86_64/manifest"] and len(announced) == 1

    serve_viewer(server, b"viewer two", "linux-x86_64")
    second = await provision.ensure_viewer(api, directory, "linux-x86_64")
    assert second != first and second.read_bytes() == b"viewer two"
    assert list(directory.iterdir()) == [second], "the old build is removed"


async def test_a_viewer_that_does_not_match_its_manifest_is_not_kept(tmp_path, api, server):
    serve_viewer(server, b"viewer one", "windows-x86_64")
    server.on(
        "GET",
        "/api/viewer/windows-x86_64/binary",
        handler=lambda _req: httpx.Response(200, content=b"something else"),
    )
    with pytest.raises(ApiError, match="does not match"):
        await provision.ensure_viewer(api, tmp_path, "windows-x86_64")
    assert list(tmp_path.iterdir()) == []


async def test_remote_desktop_fetches_the_viewer_and_uses_the_accepted_ca(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    pinned = TrustStore(tmp_path / "servers").pin(BASE, CA_PEM)
    serve_agents(server)
    serve_viewer(server, b"viewer one", provision.current_platform())
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
    app = bare_app(server, kr, tmp_path, launched)
    app.config = app.config.with_overrides(quic_addr="127.0.0.1:4433")
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        await wait_for(pilot, lambda: app.screen.query_one("#agents", DataTable).row_count == 3)
        await pilot.press("d")
        await wait_for(pilot, lambda: launched)
    (command,) = launched
    viewer = Path(command.argv[0])
    assert viewer.parent == tmp_path / "viewer" and viewer.read_bytes() == b"viewer one"
    assert command.argv[command.argv.index("--ca") + 1] == str(pinned)
    assert "/api/ca" not in paths(server)


async def test_with_a_public_certificate_the_viewers_ca_comes_from_the_server(tmp_path) -> None:
    server, kr, launched = FakeServer(), MemoryKeyring(), []
    TokenStore(BASE, kr).save(StoredSession("sess", LATER, "jane"))
    serve_agents(server)
    serve_viewer(server, b"viewer one", provision.current_platform())
    server.on("GET", "/api/ca", handler=lambda _req: httpx.Response(200, text=CA_PEM))
    app = bare_app(server, kr, tmp_path, launched, private_ca=False)
    async with app.run_test(size=(140, 40)) as pilot:
        await wait_for(pilot, lambda: isinstance(app.screen, MainScreen))
        config = await app.viewer_config()
    assert config.ca_path is not None and config.ca_path.read_text() == CA_PEM
    # Fetched for the viewer only: the API keeps using the system store.
    assert app.ca_for(BASE) is None and not app.fetched  # type: ignore[attr-defined]


def test_forget_ca_drops_the_accepted_certificate(tmp_path, monkeypatch, capsys) -> None:
    monkeypatch.setattr("rmm_tui.config.data_dir", lambda: tmp_path)
    TrustStore(tmp_path / "servers").pin(BASE, CA_PEM)
    args = ["--config", str(tmp_path / "none.toml"), "--server-url", BASE, "--forget-ca"]
    main(args)
    assert "Forgot" in capsys.readouterr().out
    assert TrustStore(tmp_path / "servers").pinned(BASE) is None
    main(args)
    assert "No certificate authority" in capsys.readouterr().out
