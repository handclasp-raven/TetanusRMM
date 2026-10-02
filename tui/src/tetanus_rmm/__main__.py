"""Entry point: ``tetanus-rmm`` or ``python -m tetanus_rmm``."""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path

from . import config as config_mod
from .api import ApiClient, ApiError
from .auth import SessionManager, TokenStore
from .scripts import ScriptLibrary
from .state import UiState
from .trust import TrustStore


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="tetanus-rmm", description="RMM support TUI")
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
    parser.add_argument(
        "--forget-ca",
        action="store_true",
        help="forget the certificate authority trusted for this server and exit",
    )
    return parser.parse_args(argv)


def resolve_config(args: argparse.Namespace, state: UiState) -> config_mod.Config:
    """The config file, overridden by flags. The server is ``--server-url``
    if given, else the one signed in to last time, else the file's."""
    config = config_mod.load(args.config).with_overrides(
        ca_path=args.ca_path.resolve() if args.ca_path else None,
        viewer_path=args.viewer_path,
        quic_addr=args.quic_addr,
    )
    if args.server_url:
        return config.with_overrides(server_url=args.server_url)
    if state.last_server:
        try:
            return config.for_server(config_mod.normalize_server_url(state.last_server))
        except config_mod.ConfigError:
            pass  # a hand-edited state file: ignore it
    return config


def main(argv: list[str] | None = None) -> None:
    config_mod.migrate_legacy_dirs()
    args = parse_args(argv)
    log_dir = config_mod.data_dir()
    state = UiState.load(log_dir / "state.json")
    try:
        config = resolve_config(args, state)
    except config_mod.ConfigError as e:
        sys.exit(f"tetanus-rmm: {e}")

    log_dir.mkdir(parents=True, exist_ok=True)
    # Never log to the terminal: it belongs to the TUI.
    logging.basicConfig(
        filename=log_dir / "tetanus-rmm.log",
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    store = TokenStore(config.server_url)
    if args.logout:
        store.clear()
        return

    trust = TrustStore(log_dir / "servers")
    if args.forget_ca:
        forgotten = trust.forget(config.server_url)
        print(
            f"Forgot the certificate authority for {config.server_url}."
            if forgotten
            else f"No certificate authority is trusted for {config.server_url}."
        )
        return

    try:
        api = ApiClient(config.server_url, config.ca_path or trust.pinned(config.server_url))
    except ApiError as e:
        sys.exit(f"tetanus-rmm: {e.message}")

    from .app import RmmApp

    app = RmmApp(
        config=config,
        session=SessionManager(api, store),
        library=ScriptLibrary(config.scripts_path),
        log_dir=log_dir,
        state=state,
        trust=trust,
    )
    try:
        app.run()
    finally:
        # The app closes them on unmount; this covers a crash before that.
        app.close_viewers()


if __name__ == "__main__":
    main()
