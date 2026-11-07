#!/bin/sh
# Install system-builder: the gb-server systemd service and/or the gb-client CLI.
#
#   ./install.sh server [options]   build + install gb-server + systemd unit (default)
#   ./install.sh client [options]   build + install gb-client CLI only
#
# Options:
#   --prefix DIR    binary install prefix            (default: /usr/local)
#   --destdir DIR   stage into DIR instead of /      (for packaging/tests;
#                   skips the systemd daemon-reload/enable)
#   --no-build      skip `cargo build`, reuse target/release binaries
#   --no-enable     install files but don't enable/start the service
#   --config FILE   config template to seed          (default: config.example.toml)
#
# Live installs (no --destdir) must run as root.
set -eu

MODE="server"
PREFIX="/usr/local"
DESTDIR=""
BUILD="yes"
ENABLE="yes"
CONFIG_TMPL="config.example.toml"

for arg in "$@"; do
    case "$arg" in
        server|client) MODE="$arg" ;;
        --prefix=*) PREFIX="${arg#--prefix=}" ;;
        --destdir=*) DESTDIR="${arg#--destdir=}" ;;
        --config=*) CONFIG_TMPL="${arg#--config=}" ;;
        --prefix|--destdir|--config)
            echo "error: $arg needs a value, use $arg=VALUE" >&2; exit 2 ;;
        --no-build) BUILD="no" ;;
        --no-enable) ENABLE="no" ;;
        -h|--help)
            sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "error: unknown argument: $arg (see --help)" >&2; exit 2 ;;
    esac
done

REPO="$(cd "$(dirname "$0")" && pwd)"
cd "$REPO"

if [ -z "$DESTDIR" ] && [ "$(id -u)" -ne 0 ]; then
    echo "error: live install must run as root (or pass --destdir DIR to stage)" >&2
    exit 1
fi

BIN="$PREFIX/bin"
UNIT="$DESTDIR/etc/systemd/system/system-builder.service"

if [ "$MODE" = "server" ] || [ "$MODE" = "client" ]; then
    pkg="gb-server"; [ "$MODE" = "client" ] && pkg="gb-client"
    if [ "$BUILD" = "yes" ]; then
        echo "==> cargo build --release -p $pkg"
        cargo build --release -p "$pkg"
    fi
    if [ ! -x "target/release/$pkg" ]; then
        echo "error: target/release/$pkg missing (drop --no-build to build it)" >&2
        exit 1
    fi
    echo "==> install $pkg -> $DESTDIR$BIN/$pkg"
    install -Dm755 "target/release/$pkg" "$DESTDIR$BIN/$pkg"
fi

if [ "$MODE" = "server" ]; then
    echo "==> seed config (never overwrites)"
    mkdir -p "$DESTDIR/etc/system-builder"
    if [ -f "$DESTDIR/etc/system-builder/config.toml" ]; then
        echo "    keeping existing $DESTDIR/etc/system-builder/config.toml"
    else
        install -Dm600 "$CONFIG_TMPL" "$DESTDIR/etc/system-builder/config.toml"
        echo "    wrote $DESTDIR/etc/system-builder/config.toml (edit systems/bind)"
    fi
    echo "==> install systemd unit -> $UNIT"
    mkdir -p "$(dirname "$UNIT")"
    # Rewrite the binary path when --prefix moves it.
    sed "s|/usr/local/bin/gb-server|$BIN/gb-server|" \
        packaging/system-builder.service > "$UNIT"
    chmod 644 "$UNIT"

    if [ -z "$DESTDIR" ] && command -v systemctl >/dev/null 2>&1 && [ "$ENABLE" = "yes" ]; then
        echo "==> systemctl daemon-reload + enable --now"
        systemctl daemon-reload
        systemctl enable --now system-builder
        systemctl --no-pager status system-builder || true
    else
        echo "==> skipped enable (DESTDIR set, no systemctl, or --no-enable)"
        echo "    next: edit /etc/system-builder/config.toml, then:"
        echo "      systemctl daemon-reload && systemctl enable --now system-builder"
    fi
fi

if [ "$MODE" = "client" ]; then
    echo "==> done. First run creates ~/.config/gb-client/config.toml;"
    echo "    set the server there or export GB_SERVER, then:"
    echo "    gb-client systems"
fi
