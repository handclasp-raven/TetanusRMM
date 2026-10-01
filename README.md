# TetanusRMM

Remote monitoring and management tool.

- **Server** (Rust): QUIC listener for agents and viewers, HTTPS API, Postgres, audit log, enrollment CA and signed update publishing.
- **Agent** (Windows service): enrollment, telemetry, signed self-update, remote desktop capture, remote shell, scripts and file transfer.
- **Viewer** (Linux, macOS, Windows): end-to-end encrypted remote desktop, relayed through the server or direct when NAT allows.
- **Support TUI** (Python + Textual): sign in, agent table, launch the viewer, shell console and script runner.

Full documentation is in the **[wiki](https://github.com/handclasp-raven/TetanusRMM/wiki)**.

## Quick install

On a Linux x86_64 host with `curl`:

```bash
curl -fsSL https://raw.githubusercontent.com/handclasp-raven/TetanusRMM/main/scripts/install.sh -o install.sh
bash install.sh --host rmm.example.com
```

Open ports 8443/tcp, 4433/udp, 4433/tcp and 3478/udp. The script prints the admin's TOTP secret once, so add it to an authenticator app before closing the terminal. Then send staff to `https://<host>:8443/install` and [enroll an agent](https://github.com/handclasp-raven/TetanusRMM/wiki/Enrolling-an-Agent).

See [Installing the Server](https://github.com/handclasp-raven/TetanusRMM/wiki/Installing-the-Server) for options and the manual steps.

## Documentation

**Getting started:** [Installing the Server](https://github.com/handclasp-raven/TetanusRMM/wiki/Installing-the-Server) · [Enrolling an Agent](https://github.com/handclasp-raven/TetanusRMM/wiki/Enrolling-an-Agent) · [MSI Installer](https://github.com/handclasp-raven/TetanusRMM/wiki/MSI-Installer) · [Windows Agent Service](https://github.com/handclasp-raven/TetanusRMM/wiki/Windows-Agent-Service) · [Support TUI](https://github.com/handclasp-raven/TetanusRMM/wiki/Support-TUI)

**Features:** [Remote Desktop](https://github.com/handclasp-raven/TetanusRMM/wiki/Remote-Desktop) · [End-to-End Encryption](https://github.com/handclasp-raven/TetanusRMM/wiki/End-to-End-Encryption) · [Direct Connections](https://github.com/handclasp-raven/TetanusRMM/wiki/Direct-Connections) · [Adaptive Bitrate](https://github.com/handclasp-raven/TetanusRMM/wiki/Adaptive-Bitrate) · [Consent and Notifications](https://github.com/handclasp-raven/TetanusRMM/wiki/Consent-and-Notifications) · [Remote Shell, Scripts and File Transfer](https://github.com/handclasp-raven/TetanusRMM/wiki/Remote-Shell-Scripts-and-File-Transfer) · [Telemetry](https://github.com/handclasp-raven/TetanusRMM/wiki/Telemetry)

**Reference:** [Connectivity](https://github.com/handclasp-raven/TetanusRMM/wiki/Connectivity) · [Signed Updates](https://github.com/handclasp-raven/TetanusRMM/wiki/Signed-Updates) · [Configuration](https://github.com/handclasp-raven/TetanusRMM/wiki/Configuration) · [Access Control](https://github.com/handclasp-raven/TetanusRMM/wiki/Access-Control) · [HTTPS API](https://github.com/handclasp-raven/TetanusRMM/wiki/HTTPS-API) · [Metrics](https://github.com/handclasp-raven/TetanusRMM/wiki/Metrics)

**Development:** [Architecture](https://github.com/handclasp-raven/TetanusRMM/wiki/Architecture) · [Development Setup](https://github.com/handclasp-raven/TetanusRMM/wiki/Development-Setup) · [Testing](https://github.com/handclasp-raven/TetanusRMM/wiki/Testing)

## Development quick start

Requires Docker and a Rust toolchain.

```bash
cargo run -p server -- gen-certs
mkdir -p updates
docker compose up --build -d
curl --cacert dev-certs/ca.crt https://localhost:8443/api/health
```

Checks:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace        # needs a running Docker daemon
```

The support TUI has its own README: [tui/README.md](tui/README.md).
