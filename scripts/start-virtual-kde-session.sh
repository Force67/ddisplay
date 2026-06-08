#!/usr/bin/env bash
set -euo pipefail

# Virtual KDE X11 session for ddisplay.
#
# Backends:
#   xorg-dummy — Xorg with the "dummy" video driver. Supports RandR mode
#                changes at runtime, which ddisplay-server uses for
#                --resize-to-client (the display follows the connecting
#                client's native resolution, up to 8K virtual).
#                Requires: sudo apt install xserver-xorg-video-dummy
#                (and Xwrapper.config with needs_root_rights=no + allowed_users
#                relaxed if running from a non-console session).
#   xvfb       — Xvfb fallback. Resolution is FIXED at startup; RandR mode
#                switching is not supported, so --resize-to-client is a no-op.
#                Pick the size via DDISPLAY_VIRTUAL_SCREEN (e.g. 3840x2160x24
#                for a fixed 4K session).
#
# DDISPLAY_VIRTUAL_BACKEND=auto|xorg-dummy|xvfb (default: auto)

DISPLAY_NUM="${DDISPLAY_VIRTUAL_DISPLAY:-:10}"
SCREEN_SPEC="${DDISPLAY_VIRTUAL_SCREEN:-1920x1080x24}"
AUTH_FILE="${DDISPLAY_VIRTUAL_XAUTHORITY:-$HOME/.Xauthority-ddisplay-virtual}"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ddisplay"
BACKEND="${DDISPLAY_VIRTUAL_BACKEND:-auto}"

mkdir -p "$STATE_DIR"
touch "$AUTH_FILE"
chmod 600 "$AUTH_FILE"

xauth -f "$AUTH_FILE" remove "$DISPLAY_NUM" >/dev/null 2>&1 || true
xauth -f "$AUTH_FILE" add "$DISPLAY_NUM" . "$(mcookie)"

X_PID=""
cleanup() {
    if [[ -n "$X_PID" ]] && kill -0 "$X_PID" 2>/dev/null; then
        kill "$X_PID" 2>/dev/null || true
        wait "$X_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# Initial resolution (WxH) from the screen spec, e.g. "1920x1080x24".
RES_W="${SCREEN_SPEC%%x*}"
RES_REST="${SCREEN_SPEC#*x}"
RES_H="${RES_REST%%x*}"

have_dummy_driver() {
    [[ -e /usr/lib/xorg/modules/drivers/dummy_drv.so ]]
}

write_dummy_conf() {
    cat > "$STATE_DIR/xorg-dummy.conf" << EOF
Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "DontVTSwitch" "true"
EndSection
Section "Device"
    Identifier "dummy"
    Driver "dummy"
    VideoRam 256000
EndSection
Section "Monitor"
    Identifier "monitor"
    HorizSync 5.0-1000.0
    VertRefresh 5.0-200.0
EndSection
Section "Screen"
    Identifier "screen"
    Device "dummy"
    Monitor "monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Virtual 7680 4320
    EndSubSection
EndSection
EOF
}

start_xorg_dummy() {
    write_dummy_conf
    Xorg "$DISPLAY_NUM" \
        -config "$STATE_DIR/xorg-dummy.conf" \
        -auth "$AUTH_FILE" \
        -logfile "$STATE_DIR/xorg-dummy.log" \
        -noreset \
        -nolisten tcp \
        >"$STATE_DIR/xorg.log" 2>&1 &
    X_PID=$!
}

start_xvfb() {
    Xvfb "$DISPLAY_NUM" \
        -screen 0 "$SCREEN_SPEC" \
        -auth "$AUTH_FILE" \
        -nolisten tcp \
        +extension RANDR \
        +extension RENDER \
        +extension GLX \
        >"$STATE_DIR/xvfb.log" 2>&1 &
    X_PID=$!
}

wait_for_socket() {
    local socket="/tmp/.X11-unix/X${DISPLAY_NUM#:}"
    for _ in $(seq 1 50); do
        if [[ -S "$socket" ]] && kill -0 "$X_PID" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

case "$BACKEND" in
    xorg-dummy)
        start_xorg_dummy
        ;;
    xvfb)
        start_xvfb
        ;;
    auto)
        if have_dummy_driver; then
            echo "Using Xorg dummy backend (RandR resize supported)" >&2
            start_xorg_dummy
            if ! wait_for_socket; then
                echo "Xorg dummy failed to start (see $STATE_DIR/xorg-dummy.log), falling back to Xvfb" >&2
                X_PID=""
                start_xvfb
            fi
        else
            echo "xserver-xorg-video-dummy not installed — using Xvfb." >&2
            echo "NOTE: Xvfb resolution is fixed at $SCREEN_SPEC; ddisplay's" >&2
            echo "--resize-to-client (match the client's monitor, 4K etc.) needs" >&2
            echo "the dummy driver: sudo apt install xserver-xorg-video-dummy" >&2
            start_xvfb
        fi
        ;;
    *)
        echo "Unknown DDISPLAY_VIRTUAL_BACKEND: $BACKEND" >&2
        exit 1
        ;;
esac

if ! wait_for_socket; then
    echo "Virtual display $DISPLAY_NUM failed to start" >&2
    exit 1
fi

# On the dummy backend, set the initial mode to the requested resolution
# (the server adds/removes modes at runtime for --resize-to-client).
if [[ -e "$STATE_DIR/xorg-dummy.conf" ]] && kill -0 "$X_PID" 2>/dev/null; then
    DISPLAY="$DISPLAY_NUM" XAUTHORITY="$AUTH_FILE" xrandr --query >/dev/null 2>&1 || true
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
