#!/usr/bin/env bash
set -euo pipefail

DISPLAY_NUM="${DDISPLAY_VIRTUAL_DISPLAY:-:10}"
SCREEN_SPEC="${DDISPLAY_VIRTUAL_SCREEN:-1920x1080x24}"
AUTH_FILE="${DDISPLAY_VIRTUAL_XAUTHORITY:-$HOME/.Xauthority-ddisplay-virtual}"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ddisplay"

mkdir -p "$STATE_DIR"
touch "$AUTH_FILE"
chmod 600 "$AUTH_FILE"

xauth -f "$AUTH_FILE" remove "$DISPLAY_NUM" >/dev/null 2>&1 || true
xauth -f "$AUTH_FILE" add "$DISPLAY_NUM" . "$(mcookie)"

XVFB_PID=""
cleanup() {
    if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

Xvfb "$DISPLAY_NUM" \
    -screen 0 "$SCREEN_SPEC" \
    -auth "$AUTH_FILE" \
    -nolisten tcp \
    +extension RANDR \
    +extension RENDER \
    +extension GLX \
    >"$STATE_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!

DISPLAY_SOCKET="/tmp/.X11-unix/X${DISPLAY_NUM#:}"
for _ in $(seq 1 50); do
    if [[ -S "$DISPLAY_SOCKET" ]]; then
        break
    fi
    sleep 0.2
done

if [[ ! -S "$DISPLAY_SOCKET" ]]; then
    echo "Virtual display $DISPLAY_NUM failed to start" >&2
    exit 1
fi

unset DBUS_SESSION_BUS_ADDRESS
unset GNOME_SHELL_SESSION_MODE
unset DESKTOP_SESSION
unset XDG_SESSION_DESKTOP
unset XDG_CURRENT_DESKTOP

export DISPLAY="$DISPLAY_NUM"
export XAUTHORITY="$AUTH_FILE"
export XDG_SESSION_TYPE="x11"
export XDG_SESSION_DESKTOP="KDE"
export XDG_CURRENT_DESKTOP="KDE"
export DESKTOP_SESSION="plasma"
export KDE_FULL_SESSION="true"
export XDG_CONFIG_DIRS="/etc/xdg"
export QT_X11_NO_MITSHM="1"

exec dbus-run-session /usr/bin/startplasma-x11
