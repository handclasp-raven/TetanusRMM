<img width="1280" height="640" alt="Frame 1" src="https://github.com/user-attachments/assets/5eeb6c9e-2385-49c8-a998-604e2b016678" />

# Tetanus RMM

Terminal based remote monitoring and management tool.

## Table of Contents
- [About](#-about)
- [Features](#-features)
- [Quick Install](#-quick-install)
- [Still To Come](#-still-to-come)
- [Quick Assist](#-quick-assist)
- [Documentation](#-documentation)
- [Development Quick Start](#-development-quick-start)


## 🚀 About

- **Support TUI** (Python + Textual): Runs anywhere you can run python. Sign in, administrate accounts, add agents, launch the viewer, shell console and script runner.
- **Viewer** (Linux, macOS, Windows): end-to-end encrypted remote desktop, relayed through the server or direct when NAT allows.
- **Server** (Rust): Runs in Docker, maintains connection to agents and acts as a direct connection broker or fallback relay if necessary.
- **Agent** (Windows service): enrollment, telemetry, signed self-update, remote desktop capture, remote shell, scripts and file transfer.
- **Quick assist** (Windows, portable): one-time support for a machine with no agent. The user downloads one file from `https://<host>:8443/assist` and types a code provided by the technician.

Full documentation is in the **[wiki](https://github.com/handclasp-raven/TetanusRMM/wiki)**.


## ⚡ Quick Install

On a Linux x86_64 host with `curl`:

```bash
curl -fsSL https://raw.githubusercontent.com/handclasp-raven/TetanusRMM/main/scripts/install.sh -o install.sh
bash install.sh --host rmm.example.com
```
> [!TIP]
> If the server is reached at more than one name or address, list them all (`--host rmm.example.com,10.0.0.5`, or `--host` again for each): every one goes into the server certificate, and the first is the public address.

> [!NOTE]
>Open ports 8443/tcp, 4433/udp, 4433/tcp and 3478/udp. The script prints the admin's TOTP secret once, so add it to an authenticator app before closing the terminal. Then send staff to `https://<host>:8443/install` and [enroll an agent](https://github.com/handclasp-raven/TetanusRMM/wiki/Enrolling-an-Agent).

## 📥 Updating
The script installs the newest release: a server image from `ghcr.io/handclasp-raven/tetanusrmm` and the agent, viewer and TUI builds from the [releases page](https://github.com/handclasp-raven/TetanusRMM/releases). Nothing is compiled on the host. To upgrade later, run this in the install directory; it backs the database up to `backups/` first:

```bash
bash scripts/install.sh --upgrade
```

See [Installing the Server](https://github.com/handclasp-raven/TetanusRMM/wiki/Installing-the-Server) for options and the manual steps.



## 💎 Features
-  **TUI Built with Textual (so you know it's good):** Fast, keyboard shortcuts for everything, built in themes and a theme editor.
-  **Speed:** Remote access sessions are built in Rust and use the newer Windows.Graphics.Capture API (for Win 10 and Win 11) so the FPS is great.
-  **Speeeeed:** If your NAT allows for it, the server will broker a direct connection from your viewer to the agent so there is no middle man to slow things down.
-  **User management and RBAC:** Create and administrate users, give them access to certain functions for certain groups.
-  **Painless Agent Install:** Generate a URL, open it on the remote PC, Download an MSI, Install - done.
-  **Mass Agent Deployment:** Generate one MSI with a reusable, revocable key and push it with Intune or a GPO. It installs silently and each PC enrolls itself.
-  **Quick Support:** For when you just need to fix something for someone but don't need an agent. Give them a link, Give them a code and you're connected!
-  **Remote Shell:** Need to just fire some commands without bugging the user? Say less.
-  **Scripts:** Add scripts, choose target devices or a group and hit run. Easy.
-  **Quick Commands:** In your remove viewer, you can add your own quick command buttons to save you having to hit Win+R.
-  **Lent Password:** User off to lunch? From the viewer, ask them to type a password before they go. It stays on their PC, encrypted in memory, the agent types it for you (lock screen and UAC prompts too), and it is forgotten when the last session ends.
-  **Audit Logging** The server keeps an audit of who did what and where


## 🤔 Still To Come
- [ ] **Handle Login Screens and UAC:** currently you can interact with elevated windows, however the user still needs to handle the login screen and UAC prompts.
- [ ] **Password Manager Integration:** Integrate a password manager into the Remote Session Viewer to save you having to copy/paste everything
- [ ] **Cred Cache:** Prompt users for a credential so you can log into things without them, all without them actually giving you the credential!
- [ ] **Better Monitoring:** Sysmon and a sane config by default + log shipping.
- [ ] **Tunnelling and Remote Apps:** Create temporary tunnels and access devices within the customer's network like admin pages, printers, etc from your local browser.
- [ ] **More Agent Platform Support:** Linux (X and Wayland), MacOS and older versions of Windows (Currently untested but may work).
- [ ] **E-mail password reset self service:** Currently all administration is performed on the platform itself through the TUI and no E-mails are tied to user accounts.
- [ ] **Tailscale Integration:** build the server with Docker Tailscale integration so the server can hide on your mesh and you can minimize external exposure
- [ ] **Serve the TUI over the web** Access a TUI from a browser using Textual


## 💻 Quick Assist

To help someone whose computer has no agent, once:

1. In the TUI press `h` (Agent menu, Quick assist). It shows a page address and a six-digit code, valid for ten minutes and usable once.
2. The user opens `https://<host>:8443/assist`, downloads `TetanusRMM-Assist.exe` and runs it. Nothing is installed. Windows asks whether to let it run as administrator; saying no still works, but you then cannot control administrator windows.
3. The program shows a warning about scams (never pay anyone with gift cards or cryptocurrency) that cannot be accepted for five seconds, then asks for the code.
4. When the code is typed, your viewer opens and the user is asked, with your username, whether to allow you. They can stop you with Ctrl+F12, and closing the program ends the session.

You get remote desktop (screen, input, clipboard) and file transfer, running as the user. There is no shell, no scripts and no command buttons, for admins too. Only the technician who made the code, and admins, can use the session. While the program stays open the machine is in the agent table, so a closed viewer can be reopened with `d`; the server forgets the machine two minutes after the program is closed. Codes, their use and the end of each session are in the audit log (`assist.create`, `assist.redeem`, `assist.end`).

> [!IMPORTANT]
> Windows only. UAC prompts, the lock screen and Ctrl+Alt+Del cannot be seen or controlled (an installed agent can: it runs a service). With the installer's own CA the user's browser warns about the page's certificate, and Windows SmartScreen warns about the unsigned program; a publicly trusted certificate on the API and a code-signing certificate remove those warnings.


## 📚 Documentation

**Getting started:** [Installing the Server](https://github.com/handclasp-raven/TetanusRMM/wiki/Installing-the-Server) · [Enrolling an Agent](https://github.com/handclasp-raven/TetanusRMM/wiki/Enrolling-an-Agent) · [MSI Installer](https://github.com/handclasp-raven/TetanusRMM/wiki/MSI-Installer) · [Windows Agent Service](https://github.com/handclasp-raven/TetanusRMM/wiki/Windows-Agent-Service) · [Support TUI](https://github.com/handclasp-raven/TetanusRMM/wiki/Support-TUI)

**Features:** [Remote Desktop](https://github.com/handclasp-raven/TetanusRMM/wiki/Remote-Desktop) · [End-to-End Encryption](https://github.com/handclasp-raven/TetanusRMM/wiki/End-to-End-Encryption) · [Direct Connections](https://github.com/handclasp-raven/TetanusRMM/wiki/Direct-Connections) · [Adaptive Bitrate](https://github.com/handclasp-raven/TetanusRMM/wiki/Adaptive-Bitrate) · [Consent and Notifications](https://github.com/handclasp-raven/TetanusRMM/wiki/Consent-and-Notifications) · [Remote Shell, Scripts and File Transfer](https://github.com/handclasp-raven/TetanusRMM/wiki/Remote-Shell-Scripts-and-File-Transfer) · [Telemetry](https://github.com/handclasp-raven/TetanusRMM/wiki/Telemetry)

**Reference:** [Connectivity](https://github.com/handclasp-raven/TetanusRMM/wiki/Connectivity) · [Signed Updates](https://github.com/handclasp-raven/TetanusRMM/wiki/Signed-Updates) · [Configuration](https://github.com/handclasp-raven/TetanusRMM/wiki/Configuration) · [Access Control](https://github.com/handclasp-raven/TetanusRMM/wiki/Access-Control) · [HTTPS API](https://github.com/handclasp-raven/TetanusRMM/wiki/HTTPS-API) · [Metrics](https://github.com/handclasp-raven/TetanusRMM/wiki/Metrics)

**Development:** [Architecture](https://github.com/handclasp-raven/TetanusRMM/wiki/Architecture) · [Development Setup](https://github.com/handclasp-raven/TetanusRMM/wiki/Development-Setup) · [Testing](https://github.com/handclasp-raven/TetanusRMM/wiki/Testing)

The support TUI has its own README: [tui/README.md](tui/README.md).


## ⚡ Development Quick Start

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
