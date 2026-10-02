#!/usr/bin/env bash
# Install the TetanusRMM server on a Linux host, from nothing to a running server
# with published agent and viewer builds.
#
#   curl -fsSL https://raw.githubusercontent.com/handclasp-raven/TetanusRMM/main/scripts/install.sh -o install.sh
#   bash install.sh --host rmm.example.com
#
# or, from a checkout: scripts/install.sh --host rmm.example.com
#
# To upgrade an installed server to the newest release, from its directory:
#
#   bash scripts/install.sh --upgrade
#
# What it does (each step is skipped if already done, so it is safe to re-run):
#   1. installs Docker if missing (asks first);
#   2. downloads the release: the compose file, these scripts and the agent,
#      viewer and TUI builds, checked against the release's SHA256SUMS;
#   3. writes .env: a random database password, the public URL, your uid/gid
#      and the server image of that release;
#   4. pulls the server image (an upgrade backs the database up first, into
#      ./backups);
#   5. generates the CA and server certificate (./dev-certs) and the update
#      signing key (./update-keys);
#   6. starts Postgres and the server with Docker Compose;
#   7. creates the first admin user;
#   8. signs the Windows agent with your update key and publishes it, the
#      viewers and the TUI.
#
# Only Docker is needed on the host: there is no Rust or Python to install.
#
# Run from a git checkout, or with --from-source, it builds everything from
# the source tree instead of using a release (steps 2, 4 and 8; this takes
# a while, and the first agent build downloads a ~3.6 GB toolchain image).
#
# Options (or the environment variable in brackets):
#   --host NAME       name or IP that agents and staff reach this server at
#                     [RMM_HOST]; default: asks, suggesting this host's address.
#                     Repeat it, or separate names with commas, to put more
#                     names in the server certificate: the first is the one
#                     in the public URL and the download links
#   --dir PATH        where to install [RMM_DIR]; default /opt/tetanusrmm as
#                     root, ~/tetanusrmm otherwise
#   --upgrade         move to the newest release (without it, a re-run keeps
#                     the installed version)
#   --version X.Y.Z   install or move to this release [RMM_VERSION]
#   --from-source     clone the repository and build from source
#   --repo URL        git repository, and where its releases are [RMM_REPO]
#   --branch NAME     branch or tag to check out, with --from-source [RMM_BRANCH]
#   --admin NAME      first admin's username [RMM_ADMIN]; default admin
#   --no-admin        do not create a user
#   --skip-clients    do not publish the agent, viewers and TUI (step 8)
#   -y, --yes         never ask: take the defaults, generate the admin password
#   -h, --help        show this text
#
# RMM_ADMIN_PASSWORD sets the admin password without a prompt.
set -euo pipefail

REPO=${RMM_REPO:-https://github.com/handclasp-raven/TetanusRMM.git}
BRANCH=${RMM_BRANCH:-}
IMAGE_REPO=${RMM_IMAGE_REPO:-ghcr.io/handclasp-raven/tetanusrmm}
VERSION=${RMM_VERSION:-}
DIR=${RMM_DIR:-}
# Comma-separated until configure splits it: then HOST is the first name, the
# public one, and HOSTS is all of them.
HOST=${RMM_HOST:-}
ADMIN=${RMM_ADMIN:-admin}
ADMIN_PASSWORD=${RMM_ADMIN_PASSWORD:-}
MIN_PASSWORD_LEN=12
create_admin=yes
build_clients=yes
assume_yes=no
upgrade=no
from_source=no
# "release": run a published release. "source": build this tree. See locate.
mode=

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

# env_set KEY VALUE: replace or add the line in .env.
env_set() {
    env_unset "$1"
    echo "$1=$2" >>.env
}

env_unset() { sed -i "/^$1=/d" .env; }

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
    command -v docker >/dev/null 2>&1 || install_docker
    docker compose version >/dev/null 2>&1 \
        || die "the Docker Compose plugin is missing: https://docs.docker.com/compose/install/"
    if ! docker info >/dev/null 2>&1; then
        die "cannot talk to Docker as $(id -un). Start it (systemctl start docker), then
either run this script as root, or add yourself to the docker group
(sudo usermod -aG docker $(id -un)), log in again and re-run."
    fi
    note "Docker $(docker version -f '{{.Server.Version}}') and Compose are ready"
}

# Sets DIR (where the server is, or goes) and mode.
locate() {
    HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-.}")/.." 2>/dev/null && pwd || true)
    if [ -z "$DIR" ] && [ -f "$HERE/docker-compose.yml" ] && [ -f "$HERE/scripts/lib.sh" ]; then
        DIR=$HERE
    elif [ -z "$DIR" ]; then
        if [ "$(id -u)" = 0 ]; then DIR=/opt/tetanusrmm; else DIR=$HOME/tetanusrmm; fi
    fi
    if [ "$from_source" = yes ]; then mode=source
    elif [ "$upgrade" = yes ] || [ -n "$VERSION" ]; then mode=release
    elif grep -q '^RMM_IMAGE=' "$DIR/.env" 2>/dev/null; then mode=release
    elif [ -d "$DIR/crates/server" ]; then mode=source
    else mode=release; fi
}

# Leaves the shell in the repository root.
fetch_source() {
    if [ "$DIR" = "$HERE" ] && [ -d "$DIR/crates/server" ]; then
        say "Using the checkout in $DIR"
    else
        command -v git >/dev/null 2>&1 || install_git
        if [ -d "$DIR/.git" ]; then
            say "Updating $DIR"
            git -C "$DIR" pull --ff-only
        else
            say "Cloning $REPO into $DIR"
            git clone ${BRANCH:+--branch "$BRANCH"} "$REPO" "$DIR"
        fi
    fi
    cd "$DIR"
}

# The release the server image in .env belongs to, or nothing.
installed_version() {
    local image
    image=$(env_get RMM_IMAGE)
    case $image in "$IMAGE_REPO":*) echo "${image##*:}" ;; esac
}

# Settles VERSION: the one asked for, else the installed one (unless
# upgrading), else the newest release.
resolve_version() {
    VERSION=${VERSION#v}
    [ -n "$VERSION" ] || [ "$upgrade" = yes ] || VERSION=$OLD_VERSION
    [ -z "$VERSION" ] || return 0
    local url
    # The "latest" page redirects to the newest release's tag.
    url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "${REPO%.git}/releases/latest") \
        || die "cannot reach ${REPO%.git}/releases"
    case $url in
        */releases/tag/v*) VERSION=${url##*/releases/tag/v} ;;
        *) die "${REPO%.git} has no release yet: pass --version, or build with --from-source" ;;
    esac
}

release_url() { echo "${REPO%.git}/releases/download/v$VERSION/$1"; }

# install_file NAME DEST: put a release file in place. Moved into place, not
# written over, because DEST may be this very script, still running.
install_file() {
    cp "releases/$VERSION/$1" "$2.new"
    mv "$2.new" "$2"
}

# Download the release into ./releases/VERSION and put its compose file and
# scripts in place. Leaves the shell in DIR.
fetch_release() {
    mkdir -p "$DIR"
    cd "$DIR"
    OLD_VERSION=$(installed_version)
    resolve_version
    local rel=releases/$VERSION file
    if (cd "$rel" 2>/dev/null && sha256sum -c --quiet SHA256SUMS >/dev/null 2>&1); then
        say "Using release $VERSION (already downloaded)"
    else
        say "Downloading release $VERSION"
        mkdir -p "$rel"
        curl -fsSL -o "$rel/SHA256SUMS" "$(release_url SHA256SUMS)" \
            || die "there is no release $VERSION at ${REPO%.git}/releases"
        while read -r _ file; do
            file=${file#\*}
            case $file in */*|.*|'') die "unexpected file name in SHA256SUMS: '$file'" ;; esac
            note "$file"
            curl -fsSL -o "$rel/$file" "$(release_url "$file")" || die "downloading $file failed"
        done <"$rel/SHA256SUMS"
        (cd "$rel" && sha256sum -c --quiet SHA256SUMS) \
            || die "the downloaded files do not match the release's SHA256SUMS"
    fi

    if [ -d .git ]; then
        # A git checkout moving to releases keeps its tracked files; it only
        # needs a compose file that takes the image from .env.
        if ! grep -q RMM_IMAGE docker-compose.yml; then
            note "updating the checkout"
            git pull --ff-only
        fi
        return 0
    fi
    local same=no
    ! cmp -s "${BASH_SOURCE[0]}" "$rel/install.sh" || same=yes
    mkdir -p scripts
    install_file docker-compose.yml docker-compose.yml
    install_file lib.sh scripts/lib.sh
    install_file install.sh scripts/install.sh
    # Carry on with the release's own copy of this script, once.
    if [ "$same" = no ] && [ -z "${RMM_INSTALL_REEXEC:-}" ]; then
        RMM_INSTALL_REEXEC=1 exec bash scripts/install.sh \
            ${ARGS[@]+"${ARGS[@]}"} --dir "$DIR" --version "$VERSION"
    fi
}

# Before an upgrade changes anything: migrations cannot be undone.
backup() {
    [ "$mode" = release ] && [ "$upgrade" = yes ] || return 0
    [ "${OLD_VERSION:-source}" != "$VERSION" ] || return 0
    [ -n "$(docker compose ps -q db 2>/dev/null)" ] || return 0
    say "Backing up the database"
    mkdir -p backups
    BACKUP=backups/$(date +%Y%m%d-%H%M%S)-${OLD_VERSION:-source}.sql.gz
    (
        umask 077
        docker compose exec -T db sh -c 'pg_dump -U "$POSTGRES_USER" "$POSTGRES_DB"' \
            | gzip >"$BACKUP"
    ) || die "the database backup failed; the server has not been changed"
    note "wrote $BACKUP"
}

# Postgres sets its password only when it creates the database, so a volume
# left by an earlier install (`docker compose down` keeps it) cannot be
# opened with the new password a fresh .env gets.
leftover_database() {
    local volume answer=
    volume=$(docker compose config 2>/dev/null | sed -n 's/^name: //p' | head -n 1)_pgdata
    docker volume inspect "$volume" >/dev/null 2>&1 || return 0
    note "A database from an earlier install is still here (Docker volume $volume),"
    note "but the .env with its password is gone, so the server cannot open it."
    if interactive; then
        read -r -p "    Delete that database and start fresh? [y/N]: " answer </dev/tty
    fi
    case $answer in
        [yY]*) ;;
        *) die "to keep the old database, put its .env back in $DIR and run this again;
to discard it: (cd $DIR && docker compose down) && docker volume rm $volume" ;;
    esac
    docker compose down >/dev/null 2>&1 || true
    docker volume rm "$volume" >/dev/null
    note "deleted $volume"
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
        note "To put more names in the certificate, list them after it, separated"
        note "by commas."
        HOST=$(ask "Server name or IP" "$(detect_address)")
    fi
    IFS=', ' read -r -a HOSTS <<<"$HOST"
    [ "${#HOSTS[@]}" -gt 0 ] || die "no host name or IP address given"
    local name
    for name in "${HOSTS[@]}"; do
        case $name in *[!A-Za-z0-9.:-]*|'') die "not a host name or IP address: '$name'" ;; esac
    done
    HOST=${HOSTS[0]}

    if [ ! -f .env ]; then
        leftover_database
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
    if [ "$mode" = release ]; then
        export RMM_IMAGE=$IMAGE_REPO:$VERSION
        [ "$(env_get RMM_IMAGE)" = "$RMM_IMAGE" ] || env_set RMM_IMAGE "$RMM_IMAGE"
        note "server image: $RMM_IMAGE"
    elif [ -n "$(env_get RMM_IMAGE)" ]; then
        env_unset RMM_IMAGE
        note "removed RMM_IMAGE from .env: the server image is built here"
    fi
    API_PORT=${RMM_API_PORT:-$(env_get RMM_API_PORT)}
    API_PORT=${API_PORT:-8443}
    note "public address: https://$HOST:$API_PORT"
}

image() {
    if [ "$mode" = source ]; then
        say "Building the server image (several minutes the first time)"
        docker compose build server
    else
        say "Pulling the server image"
        docker compose pull server
    fi
}

# cert_covers_host NAME: does the server certificate cover it? Unknown (no
# openssl) counts as yes.
cert_covers_host() {
    command -v openssl >/dev/null 2>&1 || return 0
    local check=-checkhost
    case $1 in *:*) check=-checkip ;; *[!0-9.]*) ;; *) check=-checkip ;; esac
    { openssl x509 -in dev-certs/server.crt -noout "$check" "$1" 2>/dev/null || true; } \
        | grep -q 'does match'
}

certificates() {
    say "Certificates and keys"
    local name sans=()
    if [ -f dev-certs/ca.crt ]; then
        note "keeping the existing CA and server certificate in dev-certs/"
        for name in "${HOSTS[@]}"; do
            cert_covers_host "$name" || note "warning: the server certificate does not cover '$name'.
    Clients will refuse it. Replacing it replaces the CA too, so agents must
    re-enroll: see 'gen-certs --force --san' in the README."
        done
    else
        for name in "${HOSTS[@]}"; do sans+=(--san "$name"); done
        server gen-certs "${sans[@]}"
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

# The release's agent is the same for every server: it is signed here, with
# this server's update key, which agents pin the first time they connect.
publish_release() {
    say "Signing and publishing the agent, the viewers and the TUI ($VERSION)"
    local rel=releases/$VERSION
    server sign-update "$rel/rmm-agent-windows-x86_64.exe" \
        --platform windows-x86_64 --version "$VERSION"
    server publish-update "$rel/rmm-agent-windows-x86_64.exe" \
        --platform windows-x86_64 --version "$VERSION"
    server publish-viewer "$rel/rmm-viewer-linux-x86_64" \
        --platform linux-x86_64 --version "$VERSION"
    server publish-viewer "$rel/rmm-viewer-windows-x86_64.exe" \
        --platform windows-x86_64 --version "$VERSION"
    server publish-tui "$rel"/tetanus_rmm-*.whl
    # Downloads of earlier releases are not needed again.
    find releases -mindepth 1 -maxdepth 1 ! -name "$VERSION" -exec rm -rf {} +
}

clients() {
    if [ "$build_clients" = no ] && [ "$mode" = release ]; then
        CLIENTS_NOTE="The agent, viewers and TUI were not published. Publish them by
  running this again without --skip-clients."
        return 0
    elif [ "$build_clients" = no ]; then
        CLIENTS_NOTE="Agent, viewer and TUI builds were skipped. Build them with
  scripts/build-windows-agent.sh && scripts/build-clients.sh"
        return 0
    fi
    if [ "$mode" = release ]; then
        publish_release
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
  Version:       $(version_summary)
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
    $(upgrade_command)
EOF
}

version_summary() {
    if [ "$mode" = source ]; then
        echo "built from source"
    elif [ -n "${BACKUP:-}" ]; then
        echo "$VERSION (was ${OLD_VERSION:-built from source}; database backup in $BACKUP)"
    else
        echo "$VERSION"
    fi
}

upgrade_command() {
    if [ "$mode" = source ]; then
        echo "git pull && bash scripts/install.sh  # upgrade"
    else
        echo "bash scripts/install.sh --upgrade  # upgrade to the newest release"
    fi
}

server_fingerprint() {
    if command -v openssl >/dev/null 2>&1; then
        openssl x509 -in dev-certs/ca.crt -noout -fingerprint -sha256 | sed 's/.*=//'
    else
        echo "see 'CA certificate fingerprint' in: docker compose logs server"
    fi
}

main() {
    ARGS=("$@")
    local host_args=
    while [ $# -gt 0 ]; do
        case $1 in
            --host) host_args=${host_args:+$host_args,}${2:?--host needs a value}; shift ;;
            --dir) DIR=${2:?--dir needs a value}; shift ;;
            --repo) REPO=${2:?--repo needs a value}; shift ;;
            --branch) BRANCH=${2:?--branch needs a value}; shift ;;
            --version) VERSION=${2:?--version needs a value}; shift ;;
            --upgrade) upgrade=yes ;;
            --from-source) from_source=yes ;;
            --admin) ADMIN=${2:?--admin needs a value}; shift ;;
            --no-admin) create_admin=no ;;
            --skip-clients) build_clients=no ;;
            -y|--yes) assume_yes=yes ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
        shift
    done
    [ -z "$host_args" ] || HOST=$host_args
    if [ -n "$ADMIN_PASSWORD" ] && [ "${#ADMIN_PASSWORD}" -lt "$MIN_PASSWORD_LEN" ]; then
        die "RMM_ADMIN_PASSWORD must be at least $MIN_PASSWORD_LEN characters"
    fi

    if [ "$from_source" = yes ] && { [ "$upgrade" = yes ] || [ -n "$VERSION" ]; }; then
        die "--from-source builds the source tree: it cannot take --upgrade or --version"
    fi

    locate
    prerequisites
    if [ "$mode" = source ]; then fetch_source; else fetch_release; fi
    # The server CLI runs from the server image, not from a host toolchain.
    export RMM_SERVER_CLI=docker
    # shellcheck source=scripts/lib.sh
    . scripts/lib.sh
    backup
    configure
    image
    certificates
    start
    admin_user
    clients
    summary
}

# Everything is inside main so that a truncated download runs nothing.
main "$@"
exit
