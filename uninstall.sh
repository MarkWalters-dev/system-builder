#!/bin/sh
# Remove system-builder.
#
#   ./uninstall.sh [server|client|all] [options]   (default: all)
#
# Options:
#   --prefix DIR    binary install prefix   (default: /usr/local)
#   --destdir DIR   remove from staged DIR instead of /
#   --purge         also remove /etc/system-builder
#
# Live uninstalls (no --destdir) must run as root.
set -eu

MODE="all"
PREFIX="/usr/local"
DESTDIR=""
PURGE="no"

for arg in "$@"; do
    case "$arg" in
        server|client|all) MODE="$arg" ;;
        --prefix=*) PREFIX="${arg#--prefix=}" ;;
        --destdir=*) DESTDIR="${arg#--destdir=}" ;;
        --prefix|--destdir)
            echo "error: $arg needs a value, use $arg=VALUE" >&2; exit 2 ;;
        --purge) PURGE="yes" ;;
        -h|--help)
            sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "error: unknown argument: $arg (see --help)" >&2; exit 2 ;;
    esac
done

if [ -z "$DESTDIR" ] && [ "$(id -u)" -ne 0 ]; then
    echo "error: live uninstall must run as root (or pass --destdir DIR)" >&2
    exit 1
fi

BIN="$PREFIX/bin"

stop_service() {
    if [ -z "$DESTDIR" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl disable --now system-builder 2>/dev/null || true
    fi
}

if [ "$MODE" = "server" ] || [ "$MODE" = "all" ]; then
    echo "==> remove gb-server service"
    stop_service
    rm -f "$DESTDIR$BIN/gb-server"
    rm -f "$DESTDIR/etc/systemd/system/system-builder.service"
    if [ -z "$DESTDIR" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload
    fi
    if [ "$PURGE" = "yes" ]; then
        rm -rf "$DESTDIR/etc/system-builder" "$DESTDIR/var/lib/system-builder"
    else
        echo "    kept config (pass --purge to remove)"
    fi
fi

if [ "$MODE" = "client" ] || [ "$MODE" = "all" ]; then
    echo "==> remove gb-client"
    rm -f "$DESTDIR$BIN/gb-client"
fi

echo "done."
