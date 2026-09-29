"""The Textual application: wiring and screen changes."""

from __future__ import annotations

import asyncio
import signal
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

from textual import work
from textual.app import App

from .api import ApiError
from .auth import SessionManager
from .config import Config, data_dir
from .screens import LoginScreen, MainScreen, SplashScreen
from .scripts import ScriptLibrary
from .viewer import ViewerCommand, close_all
from .viewer import launch as launch_process

Launcher = Callable[[ViewerCommand, Path], "subprocess.Popen[bytes]"]


class RmmApp(App):
    CSS_PATH = "app.tcss"
    TITLE = "RMM support"

    def __init__(
        self,
        config: Config,
        session: SessionManager,
        library: ScriptLibrary,
        launcher: Launcher = launch_process,
        log_dir: Path | None = None,
    ) -> None:
        super().__init__()
        self.config = config
        self.session = session
        self.library = library
        self._launcher = launcher
        self.log_dir = log_dir or data_dir()
        self.viewers: list[subprocess.Popen[bytes]] = []

    def on_mount(self) -> None:
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

    def close_viewers(self) -> None:
        """Close every viewer this TUI started. Safe to call more than once."""
        viewers, self.viewers = self.viewers, []
        close_all(viewers)

    async def on_unmount(self) -> None:
        # Viewer windows must not outlive the TUI that opened them.
        await asyncio.to_thread(self.close_viewers)
        await self.session.api.aclose()
