#!/usr/bin/env bash
# Install the RMM server on a Linux host, from nothing to a running server
# with published agent and viewer builds.
#
#   curl -fsSL https://raw.githubusercontent.com/CHANGE-ME/RMMTool/main/scripts/install.sh -o install.sh
#   bash install.sh --host rmm.example.com
#
# or, from a checkout: scripts/install.sh --host rmm.example.com
#
# What it does (each step is skipped if already done, so it is safe to re-run):
#   1. installs git and Docker if missing (asks first);
#   2. clones the repository, unless run from a checkout;
#   3. writes .env: a random database password, the public URL, your uid/gid;
#   4. builds the server image;
#   5. generates the CA and server certificate (./dev-certs) and the update
#      signing key (./update-keys);
#   6. starts Postgres and the server with Docker Compose;
#   7. creates the first admin user;
#   8. builds, signs and publishes the Windows agent, the viewers and the TUI.
#
# Only Docker is needed on the host: there is no Rust or Python to install.
#
# Options (or the environment variable in brackets):
#   --host NAME       name or IP that agents and staff reach this server at
#                     [RMM_HOST]; default: asks, suggesting this host's address
#   --dir PATH        where to clone to [RMM_DIR]; default /opt/rmmtool as
#                     root, ~/rmmtool otherwise
#   --repo URL        git repository to clone [RMM_REPO]
#   --branch NAME     branch or tag to check out [RMM_BRANCH]
#   --admin NAME      first admin's username [RMM_ADMIN]; default admin
#   --no-admin        do not create a user
#   --skip-clients    do not build the agent, viewers and TUI (step 8)
#   -y, --yes         never ask: take the defaults, generate the admin password
#   -h, --help        show this text
#
# RMM_ADMIN_PASSWORD sets the admin password without a prompt.
set -euo pipefail

REPO=${RMM_REPO:-https://github.com/CHANGE-ME/RMMTool.git}
BRANCH=${RMM_BRANCH:-}
DIR=${RMM_DIR:-}
HOST=${RMM_HOST:-}
ADMIN=${RMM_ADMIN:-admin}
ADMIN_PASSWORD=${RMM_ADMIN_PASSWORD:-}
MIN_PASSWORD_LEN=12
create_admin=yes
build_clients=yes
assume_yes=no

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die() { printf '\ninstall.sh: %s\n' "$*" >&2; exit 1; }

usage() { sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"; }

# Prompts read the terminal, not stdin, so `curl ... | bash` can still ask.
interactive() { [ "$assume_yes" = no ] && [ -r /dev/tty ] && [ -w /dev/tty ]; }

# ask PROMPT DEFAULT: prints the answer.
ask() {
    local answer=
    if interactive; then
        read -r -p "$1 [$2]: " answer </dev/tty
    fi
    echo "${answer:-$2}"
}

confirm() {
    interactive || return 0
    local answer
    read -r -p "$1 [Y/n]: " answer </dev/tty
    case $answer in [nN]*) return 1 ;; esac
}

random_hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }

# env_get KEY: the value in .env, or nothing.
env_get() { [ -f .env ] && sed -n "s/^$1=//p" .env | tail -n 1 || true; }

as_root() {
    if [ "$(id -u)" = 0 ]; then "$@"
    elif command -v sudo >/dev/null 2>&1; then sudo "$@"
    else die "need root to run: $*"; fi
}

install_git() {
    confirm "git is not installed. Install it?" || die "git is required"
    if command -v apt-get >/dev/null 2>&1; then
        as_root apt-get update -qq && as_root apt-get install -y -qq git
    elif command -v dnf >/dev/null 2>&1; then as_root dnf install -y -q git
    elif command -v yum >/dev/null 2>&1; then as_root yum install -y -q git
    elif command -v zypper >/dev/null 2>&1; then as_root zypper -n install git
    elif command -v pacman >/dev/null 2>&1; then as_root pacman -S --noconfirm --needed git
    else die "install git with your package manager, then run this again"; fi
}

install_docker() {
    confirm "Docker is not installed. Install it with Docker's script (get.docker.com)?" \
        || die "Docker is required: https://docs.docker.com/engine/install/"
    if command -v pacman >/dev/null 2>&1; then
        as_root pacman -S --noconfirm --needed docker docker-compose
    else
        curl -fsSL https://get.docker.com | as_root sh
    fi
    as_root systemctl enable --now docker
}

prerequisites() {
    say "Checking prerequisites"
    [ "$(uname -s)" = Linux ] || die "the server installs on Linux (this is $(uname -s))"
    [ "$(uname -m)" = x86_64 ] \
        || note "warning: only x86_64 hosts are tested (this is $(uname -m))"
    command -v curl >/dev/null 2>&1 || die "curl is required"
    command -v git >/dev/null 2>&1 || install_git
    command -v docker >/dev/null 2>&1 || install_docker
    docker compose version >/dev/null 2>&1 \
        || die "the Docker Compose plugin is missing: https://docs.docker.com/compose/install/"
    if ! docker info >/dev/null 2>&1; then
        die "cannot talk to Docker as $(id -un). Start it (systemctl start docker), then
either run this script as root, or add yourself to the docker group
(sudo usermod -aG docker $(id -un)), log in again and re-run."
    fi
    note "git, Docker $(docker version -f '{{.Server.Version}}') and Compose are ready"
}

# Leaves the shell in the repository root.
fetch_source() {
    local here
    here=$(cd "$(dirname "${BASH_SOURCE[0]:-.}")/.." 2>/dev/null && pwd || true)
    if [ -z "$DIR" ] && [ -f "$here/docker-compose.yml" ] && [ -d "$here/crates/server" ]; then
        DIR=$here
        say "Using the checkout in $DIR"
    else
        if [ -z "$DIR" ]; then
            if [ "$(id -u)" = 0 ]; then DIR=/opt/rmmtool; else DIR=$HOME/rmmtool; fi
        fi
        if [ -d "$DIR/.git" ]; then
            say "Updating $DIR"
            git -C "$DIR" pull --ff-only
        else
            case $REPO in *CHANGE-ME*)
                die "no repository to clone: pass --repo URL (or set RMM_REPO)" ;;
            esac
            say "Cloning $REPO into $DIR"
            git clone ${BRANCH:+--branch "$BRANCH"} "$REPO" "$DIR"
        fi
    fi
    cd "$DIR"
}

detect_address() {
    local ip
    ip=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')
    echo "${ip:-$(hostname)}"
}

configure() {
    say "Configuring"
    if [ -f .env ]; then
        note "keeping the existing .env"
        local url
        url=$(env_get RMM_PUBLIC_URL)
        if [ -z "$HOST" ] && [ -n "$url" ]; then
            HOST=${url#*://}; HOST=${HOST%%/*}; HOST=${HOST%:*}
        fi
    fi
    if [ -z "$HOST" ]; then
        note "Agents and staff reach the server at one name or IP address. It goes"
        note "into the server certificate and every download link, so use the"
        note "address they can really reach (a public DNS name, if you have one)."
        HOST=$(ask "Server name or IP" "$(detect_address)")
    fi
    case $HOST in *[!A-Za-z0-9.:-]*|'') die "not a host name or IP address: '$HOST'" ;; esac

    if [ ! -f .env ]; then
        (
            umask 077
            cat >.env <<EOF
# Written by scripts/install.sh. See README "Compose settings".
POSTGRES_PASSWORD=$(random_hex 24)
RMM_PUBLIC_URL=https://$HOST:8443
RMM_UID=$(id -u)
RMM_GID=$(id -g)
EOF
        )
        note "wrote .env (random database password, mode 0600)"
    fi
    API_PORT=${RMM_API_PORT:-$(env_get RMM_API_PORT)}
    API_PORT=${API_PORT:-8443}
    note "public address: https://$HOST:$API_PORT"
}

# Does the server certificate cover $HOST? Unknown (no openssl) counts as yes.
cert_covers_host() {
    command -v openssl >/dev/null 2>&1 || return 0
    local check=-checkhost
    case $HOST in *:*) check=-checkip ;; *[!0-9.]*) ;; *) check=-checkip ;; esac
    { openssl x509 -in dev-certs/server.crt -noout "$check" "$HOST" 2>/dev/null || true; } \
        | grep -q 'does match'
}

certificates() {
    say "Certificates and keys"
    if [ -f dev-certs/ca.crt ]; then
        note "keeping the existing CA and server certificate in dev-certs/"
        cert_covers_host || note "warning: the server certificate does not cover '$HOST'.
    Clients will refuse it. Replacing it replaces the CA too, so agents must
    re-enroll: see 'gen-certs --force --san' in the README."
    else
        server gen-certs --san "$HOST"
    fi
    if [ -f update-keys/update.pub ]; then
        note "keeping the existing update signing key in update-keys/"
    else
        server gen-update-key >/dev/null
        note "wrote the update signing key to update-keys/ (keep update.key secret)"
    fi
}

start() {
    say "Starting Postgres and the server"
    mkdir -p updates
    docker compose up -d
    local i
    for i in $(seq 60); do
        if curl -fsS --cacert dev-certs/ca.crt "https://localhost:$API_PORT/api/health" \
            >/dev/null 2>&1; then
            note "the server is up: https://localhost:$API_PORT/api/health"
            return
        fi
        sleep 2
    done
    docker compose logs --tail 40 server >&2 || true
    die "the server did not become healthy; its last log lines are above"
}

read_password() {
    local first second
    while :; do
        read -r -s -p "Password for '$ADMIN' (at least $MIN_PASSWORD_LEN characters; empty to generate one): " \
            first </dev/tty; echo >/dev/tty
        [ -n "$first" ] || return 0
        if [ "${#first}" -lt "$MIN_PASSWORD_LEN" ]; then
            echo "Too short." >/dev/tty; continue
        fi
        read -r -s -p "Again: " second </dev/tty; echo >/dev/tty
        if [ "$first" = "$second" ]; then ADMIN_PASSWORD=$first; return 0; fi
        echo "They do not match." >/dev/tty
    done
}

admin_user() {
    [ "$create_admin" = yes ] || return 0
    say "First admin user"
    local count
    count=$(docker compose exec -T db sh -c \
        'psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAc "select count(*) from users"')
    if [ "$count" != 0 ]; then
        note "the database already has $count user(s): not creating another"
        return 0
    fi
    if interactive; then
        ADMIN=$(ask "Admin username" "$ADMIN")
        [ -n "$ADMIN_PASSWORD" ] || read_password
    fi
    local generated=no
    if [ -z "$ADMIN_PASSWORD" ]; then
        ADMIN_PASSWORD=$(random_hex 12); generated=yes
    fi
    ADMIN_SUMMARY=$(printf '%s\n' "$ADMIN_PASSWORD" \
        | docker compose run --rm -T server create-user --username "$ADMIN" --role admin \
        | grep -E '^(TOTP secret|otpauth URL):')
    [ "$generated" = no ] || ADMIN_SUMMARY="Password:    $ADMIN_PASSWORD
$ADMIN_SUMMARY"
    note "created '$ADMIN'; its sign-in details are in the summary below"
}

clients() {
    if [ "$build_clients" = no ]; then
        CLIENTS_NOTE="Agent, viewer and TUI builds were skipped. Build them with
  scripts/build-windows-agent.sh && scripts/build-clients.sh"
        return 0
    fi
    say "Building the Windows agent (the first build downloads a ~3.6 GB toolchain image)"
    scripts/build-windows-agent.sh
    say "Building the viewers and the TUI"
    scripts/build-clients.sh
}

summary() {
    say "Done"
    cat <<EOF

  Server:        https://$HOST:$API_PORT
  Staff install: https://$HOST:$API_PORT/install   (TUI download and instructions)
  Installed in:  $DIR
  CA fingerprint (staff confirm it at first sign-in):
    $(server_fingerprint)

EOF
    if [ -n "${ADMIN_SUMMARY:-}" ]; then
        cat <<EOF
  Admin user '$ADMIN'. This is shown once: save it now, and add the TOTP
  secret (or the otpauth URL) to an authenticator app.
$(printf '%s\n' "$ADMIN_SUMMARY" | sed 's/^/    /')

EOF
    fi
    [ -z "${CLIENTS_NOTE:-}" ] || printf '  %s\n\n' "$CLIENTS_NOTE"
    cat <<EOF
  Open these ports to agents and staff: 8443/tcp (API), 4433/udp and
  4433/tcp (agents and viewers), 3478/udp (direct connections).

  Keep dev-certs/ca.key, update-keys/update.key and .env private, and back
  them up: they cannot be recreated.

  Manage it from $DIR:
    docker compose logs -f server      # logs
    docker compose down                # stop (data is kept)
    git pull && bash scripts/install.sh  # upgrade
EOF
}

server_fingerprint() {
    if command -v openssl >/dev/null 2>&1; then
        openssl x509 -in dev-certs/ca.crt -noout -fingerprint -sha256 | sed 's/.*=//'
    else
        echo "see 'CA certificate fingerprint' in: docker compose logs server"
    fi
}

main() {
    while [ $# -gt 0 ]; do
        case $1 in
            --host) HOST=${2:?--host needs a value}; shift ;;
            --dir) DIR=${2:?--dir needs a value}; shift ;;
            --repo) REPO=${2:?--repo needs a value}; shift ;;
            --branch) BRANCH=${2:?--branch needs a value}; shift ;;
            --admin) ADMIN=${2:?--admin needs a value}; shift ;;
            --no-admin) create_admin=no ;;
            --skip-clients) build_clients=no ;;
            -y|--yes) assume_yes=yes ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
        shift
    done
    if [ -n "$ADMIN_PASSWORD" ] && [ "${#ADMIN_PASSWORD}" -lt "$MIN_PASSWORD_LEN" ]; then
        die "RMM_ADMIN_PASSWORD must be at least $MIN_PASSWORD_LEN characters"
    fi

    prerequisites
    fetch_source
    # The server CLI runs from the image built below, not from a host toolchain.
    export RMM_SERVER_CLI=docker
    # shellcheck source=scripts/lib.sh
    . scripts/lib.sh
    configure
    say "Building the server image (several minutes the first time)"
    docker compose build server
    certificates
    start
    admin_user
    clients
    summary
}

# Everything is inside main so that a truncated download runs nothing.
main "$@"
