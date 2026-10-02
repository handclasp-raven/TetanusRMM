"""The Textual application: wiring and screen changes."""

from __future__ import annotations

import asyncio
import shutil
import signal
import subprocess
import sys
from collections.abc import Awaitable, Callable, Iterable
from pathlib import Path

from textual import work
from textual.app import App
from textual.theme import Theme

from . import provision
from .api import ApiClient, ApiError
from .auth import SessionManager
from .commands import QuickCommand
from .config import Config, data_dir
from .screens import LoginScreen, MainScreen, SplashScreen
from .scripts import ScriptLibrary
from .state import UiState
from .themes import DEFAULT_THEME, PREVIEW, THEMES
from .trust import TrustStore, fetch_ca
from .viewer import FONT_FILE, ViewerCommand, ViewerError, build_command, close_all
from .viewer import launch as launch_process

Launcher = Callable[[ViewerCommand, Path], "subprocess.Popen[bytes]"]
#: Makes the API client for a server URL (tests substitute a mock).
ApiFactory = Callable[[str, Path | None], ApiClient]
#: Fetches the CA certificate a server offers, for the user to accept.
CaFetcher = Callable[[str], Awaitable[str]]


class RmmApp(App):
    CSS_PATH = "app.tcss"
    TITLE = "TetanusRMM"

    def __init__(
        self,
        config: Config,
        session: SessionManager,
        library: ScriptLibrary,
        launcher: Launcher = launch_process,
        log_dir: Path | None = None,
        state: UiState | None = None,
        api_factory: ApiFactory = ApiClient,
        trust: TrustStore | None = None,
        ca_fetcher: CaFetcher = fetch_ca,
    ) -> None:
        super().__init__()
        self.config = config
        self.session = session
        self.library = library
        self._launcher = launcher
        self._api_factory = api_factory
        self.log_dir = log_dir or data_dir()
        self.state = state or UiState(self.log_dir / "state.json")
        self.trust = trust or TrustStore(self.log_dir / "servers")
        self.fetch_ca = ca_fetcher
        self.viewers: list[subprocess.Popen[bytes]] = []
        for theme in (*THEMES, *self.state.custom_themes.values()):
            self.register_theme(theme)

    def on_mount(self) -> None:
        # A theme saved by a newer version may not exist here: use the default.
        saved = self.state.theme
        self.theme = saved if saved in self.available_themes else DEFAULT_THEME
        self.theme_changed_signal.subscribe(self, self.remember_theme)
        if sys.platform != "win32":
            # Closing the terminal or `kill` should still close the viewers.
            loop = asyncio.get_running_loop()
            for sig in (signal.SIGHUP, signal.SIGTERM):
                loop.add_signal_handler(sig, self.exit)
        self.push_screen(SplashScreen())
        self.restore_session()

    @work(exclusive=True)
    async def restore_session(self) -> None:
        try:
            user = await self.session.restore()
        except ApiError as e:
            self.show_login(f"Could not check the saved session: {e.message}")
            return
        if user is None:
            self.show_login()
        else:
            self.show_main()

    def ca_for(self, server_url: str) -> Path | None:
        """The CA to verify ``server_url`` against: the configured one, else
        the one accepted for that server (``None``: the system trust store)."""
        return self.config.ca_path or self.trust.pinned(server_url)

    async def use_server(self, server_url: str, *, reconnect: bool = False) -> None:
        """Talk to ``server_url`` from now on (signed out there until the
        caller signs in). ``reconnect`` makes a new client even for the
        current server, to pick up a newly accepted CA. Raises
        :class:`ApiError` if the client cannot be set up, e.g. the CA
        certificate is unreadable."""
        if server_url == self.config.server_url and not reconnect:
            return
        api = self._api_factory(server_url, self.ca_for(server_url))
        old = self.session.switch_server(api)
        self.config = self.config.for_server(server_url)
        await old.aclose()

    async def trust_server(self, server_url: str, pem: str) -> None:
        """The user accepted ``pem`` as ``server_url``'s CA: verify that
        server against it from now on."""
        self.trust.pin(server_url, pem)
        await self.use_server(server_url, reconnect=True)

    async def viewer_config(self) -> Config:
        """The config to launch a viewer with: the viewer binary and the CA
        filled in from the server where the config names none."""
        config, api = self.config, self.session.api
        if config.viewer_path is None:
            path = await provision.ensure_viewer(
                api,
                self.log_dir / "viewer",
                on_download=lambda build: self.notify(f"Downloading viewer {build.version}…"),
            )
            if path is None and shutil.which("viewer") is None:
                raise ViewerError(
                    f"the server has no viewer for {provision.current_platform()}: ask an "
                    "administrator to publish one, or set viewer_path in the config"
                )
            config = config.with_overrides(viewer_path=str(path) if path else "viewer")
        ca_path = self.ca_for(config.server_url)
        if ca_path is None:
            # The API has a publicly trusted certificate; the viewer still
            # needs the CA behind the QUIC listener and the agents.
            pem = await api.ca_certificate()
            try:
                ca_path = self.trust.save_viewer_ca(config.server_url, pem)
            except OSError as e:
                raise ViewerError(f"cannot save the server's CA certificate: {e}") from e
        return config.with_overrides(ca_path=ca_path)

    def remember_theme(self, theme: Theme) -> None:
        """The theme was changed: start with it next time."""
        if theme.name == PREVIEW:
            return  # the theme editor's unsaved changes
        if theme.name != (self.state.theme or DEFAULT_THEME):
            self.state.theme = theme.name
            self.state.save()

    def remember_server(self) -> None:
        """Signed in: offer this server first next time."""
        if self.state.last_server != self.config.server_url:
            self.state.last_server = self.config.server_url
            self.state.save()

    def _reset_to(self, screen) -> None:  # noqa: ANN001
        # Keep the default screen; replace whatever else is showing.
        while len(self.screen_stack) > 2:
            self.pop_screen()
        self.switch_screen(screen)

    def show_login(self, message: str | None = None) -> None:
        self._reset_to(LoginScreen(message))

    def show_main(self) -> None:
        self._reset_to(MainScreen())

    def session_expired(self) -> None:
        """The server rejected the session: back to the login screen."""
        self.session.expired()
        self.show_login("Your session has expired. Sign in again.")

    def launch_viewer(self, command: ViewerCommand) -> subprocess.Popen[bytes]:
        process = self._launcher(command, self.log_dir / "viewer.log")
        self.viewers = [p for p in self.viewers if p.poll() is None]
        self.viewers.append(process)
        return process

    async def start_viewer(
        self, agent_id: str, commands: Iterable[QuickCommand]
    ) -> subprocess.Popen[bytes] | None:
        """Ask for a viewer session on ``agent_id`` and start the viewer
        for it, with ``commands`` as its buttons. ``None`` if the agent has
        gone offline. Raises :class:`ApiError` or :class:`ViewerError`."""
        session = await self.session.api.create_viewer_session(agent_id)
        if not session.online:
            return None
        command = build_command(
            await self.viewer_config(),
            session,
            api_token=self.session.api.token,
            commands=commands,
            font_file=self.state.path.with_name(FONT_FILE),
        )
        return self.launch_viewer(command)

    def close_viewers(self) -> None:
        """Close every viewer this TUI started. Safe to call more than once."""
        viewers, self.viewers = self.viewers, []
        close_all(viewers)

    async def on_unmount(self) -> None:
        # Viewer windows must not outlive the TUI that opened them.
        await asyncio.to_thread(self.close_viewers)
        await self.session.api.aclose()
