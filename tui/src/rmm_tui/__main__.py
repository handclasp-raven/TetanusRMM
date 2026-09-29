"""Entry point: ``rmm-tui`` or ``python -m rmm_tui``."""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path

from . import config as config_mod
from .api import ApiClient, ApiError
from .auth import SessionManager, TokenStore
from .scripts import ScriptLibrary


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="rmm-tui", description="RMM support TUI")
    parser.add_argument(
        "--config", type=Path, help=f"config file (default: {config_mod.default_config_path()})"
    )
    parser.add_argument("--server-url", help="HTTPS API base URL, e.g. https://rmm:8443")
    parser.add_argument("--ca", type=Path, dest="ca_path", help="PEM CA certificate to trust")
    parser.add_argument("--viewer", dest="viewer_path", help="path to the viewer binary")
    parser.add_argument("--quic-addr", help="server QUIC host:port for the viewer")
    parser.add_argument(
        "--logout", action="store_true", help="forget the saved session for this server and exit"
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = parse_args(argv)
    try:
        config = config_mod.load(args.config).with_overrides(
            server_url=args.server_url,
            ca_path=args.ca_path.resolve() if args.ca_path else None,
            viewer_path=args.viewer_path,
            quic_addr=args.quic_addr,
        )
    except config_mod.ConfigError as e:
        sys.exit(f"rmm-tui: {e}")

    log_dir = config_mod.data_dir()
    log_dir.mkdir(parents=True, exist_ok=True)
    # Never log to the terminal: it belongs to the TUI.
    logging.basicConfig(
        filename=log_dir / "rmm-tui.log",
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    store = TokenStore(config.server_url)
    if args.logout:
        store.clear()
        return

    try:
        api = ApiClient(config.server_url, config.ca_path)
    except ApiError as e:
        sys.exit(f"rmm-tui: {e.message}")

    from .app import RmmApp

    app = RmmApp(
        config=config,
        session=SessionManager(api, store),
        library=ScriptLibrary(config.scripts_path),
        log_dir=log_dir,
    )
    try:
        app.run()
    finally:
        # The app closes them on unmount; this covers a crash before that.
        app.close_viewers()


if __name__ == "__main__":
    main()
