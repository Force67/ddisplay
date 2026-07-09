#!/usr/bin/env bash
set -euo pipefail

# Headless GNOME Wayland session for ddisplay's wayland backend.
#
# Starts `gnome-shell --headless --wayland --no-x11 --virtual-monitor WxH`
# inside its own D-Bus session (dbus-run-session) and captures the session's
# DBUS_SESSION_BUS_ADDRESS / WAYLAND_DISPLAY to a state file so that
# ddisplay-server can be launched with the right environment:
#
#   $HOME/.local/state/ddisplay/wayland-session.env
#
# Usage:
#   start-virtual-gnome-wayland-session.sh                # session only
#   start-virtual-gnome-wayland-session.sh CMD [ARGS...]  # run CMD inside it
#
#   DDISPLAY_VIRTUAL_SCREEN=2560x1440 start-virtual-gnome-wayland-session.sh \
#       ./target/release/ddisplay-server --backend wayland --bind 0.0.0.0:9551
#
# Notes:
#   - Mutter's ScreenCast streams go through PipeWire. The PipeWire daemon is
#     per-user (socket in $XDG_RUNTIME_DIR), independent of the session bus.
#     If no pipewire daemon is running for this user, one is started here
#     (plus wireplumber for session management, if available) and torn down
#     with the session.
#   - The server connects via Mutter's native D-Bus APIs
#     (org.gnome.Mutter.ScreenCast / .RemoteDesktop), so it MUST run with the
#     session's DBUS_SESSION_BUS_ADDRESS (source the env file, or pass the
#     command directly to this script).

SCREEN_SPEC="${DDISPLAY_VIRTUAL_SCREEN:-1920x1080}"
SCREEN_SPEC="${SCREEN_SPEC%x24}"   # tolerate WxHxDEPTH specs from the X script
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ddisplay"
ENV_FILE="$STATE_DIR/wayland-session.env"
LOG_FILE="$STATE_DIR/gnome-shell-headless.log"

mkdir -p "$STATE_DIR"

# ──────────────────────────────────────────────────────────────────────
# Inner mode: we are now inside dbus-run-session with a fresh session bus.
# ──────────────────────────────────────────────────────────────────────
if [[ "${1:-}" == "--inner" ]]; then
    shift

    export XDG_SESSION_TYPE="wayland"
    export XDG_CURRENT_DESKTOP="GNOME"
    export XDG_SESSION_DESKTOP="GNOME"

    SHELL_PID=""
    cleanup() {
        if [[ -n "$SHELL_PID" ]] && kill -0 "$SHELL_PID" 2>/dev/null; then
            kill "$SHELL_PID" 2>/dev/null || true
            wait "$SHELL_PID" 2>/dev/null || true
        fi
    }
    trap cleanup EXIT INT TERM

    # Record pre-existing wayland sockets so we can detect the new one.
    RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    declare -A OLD_SOCKETS=()
    for s in "$RUNTIME_DIR"/wayland-*; do
        [[ -e "$s" && "$s" != *.lock ]] && OLD_SOCKETS["$s"]=1
    done

    echo "Starting gnome-shell --headless (--virtual-monitor $SCREEN_SPEC)..." >&2
    gnome-shell --headless --wayland --no-x11 \
        --virtual-monitor "$SCREEN_SPEC" \
        >"$LOG_FILE" 2>&1 &
    SHELL_PID=$!

    # Wait for Mutter's ScreenCast service to appear on this session bus.
    ready=""
    for _ in $(seq 1 60); do
        if ! kill -0 "$SHELL_PID" 2>/dev/null; then
            echo "gnome-shell exited early — see $LOG_FILE" >&2
            tail -n 20 "$LOG_FILE" >&2 || true
            exit 1
        fi
        if dbus-send --session --print-reply --dest=org.freedesktop.DBus \
            /org/freedesktop/DBus org.freedesktop.DBus.GetNameOwner \
            string:org.gnome.Mutter.ScreenCast >/dev/null 2>&1; then
            ready=1
            break
        fi
        sleep 0.5
    done
    if [[ -z "$ready" ]]; then
        echo "Timed out waiting for org.gnome.Mutter.ScreenCast — see $LOG_FILE" >&2
        exit 1
    fi

    # Detect the wayland socket gnome-shell created (usually wayland-N).
    WL_DISPLAY=""
    for s in "$RUNTIME_DIR"/wayland-*; do
        [[ -e "$s" && "$s" != *.lock && -z "${OLD_SOCKETS[$s]:-}" ]] || continue
        WL_DISPLAY="$(basename "$s")"
    done
    export WAYLAND_DISPLAY="${WL_DISPLAY:-wayland-0}"
    export DDISPLAY_WAYLAND=1

    # ── Unlock gnome-keyring (Secret Service + SSH agent) ─────────────
    # A systemd-service session has no PAM login to unlock the login
    # keyring, so headless apps requesting secrets (git/libsecret,
    # browsers, etc.) would hang. Unlock it from a 0600 password file
    # (DDISPLAY_KEYRING_PW_FILE, default below) — skipped if absent.
    # gnome-shell already started a locked pkcs11,secrets daemon; --replace
    # takes it over and adds the ssh agent. The Secret Service is reached
    # over D-Bus (org.freedesktop.secrets), so every app gets it regardless
    # of env; SSH_AUTH_SOCK is pushed to the D-Bus/systemd activation env
    # for ssh clients.
    KEYRING_PW_FILE="${DDISPLAY_KEYRING_PW_FILE:-$STATE_DIR/keyring-password}"
    if [[ -r "$KEYRING_PW_FILE" ]] && command -v gnome-keyring-daemon >/dev/null 2>&1; then
        echo "Unlocking gnome-keyring (secrets, ssh, pkcs11)..." >&2
        KR_ENV="$(gnome-keyring-daemon --replace --unlock \
            --components=secrets,ssh,pkcs11 <"$KEYRING_PW_FILE" 2>/dev/null)"
        if [[ -n "$KR_ENV" ]]; then
            while IFS= read -r kv; do
                [[ "$kv" == *=* ]] && export "${kv?}"
            done <<< "$KR_ENV"
            dbus-update-activation-environment --all >/dev/null 2>&1 || true
            echo "  keyring unlocked; SSH_AUTH_SOCK=${SSH_AUTH_SOCK:-<unset>}" >&2
        else
            echo "  WARN: keyring unlock produced no env (wrong password?)" >&2
        fi
    fi

    # State file for launching the server from outside this script.
    {
        echo "# Generated by start-virtual-gnome-wayland-session.sh ($(date -Is))"
        echo "export DBUS_SESSION_BUS_ADDRESS='$DBUS_SESSION_BUS_ADDRESS'"
        echo "export WAYLAND_DISPLAY='$WAYLAND_DISPLAY'"
        echo "export XDG_RUNTIME_DIR='$RUNTIME_DIR'"
        echo "export DDISPLAY_WAYLAND=1"
        echo "export GNOME_SHELL_PID=$SHELL_PID"
        [[ -n "${SSH_AUTH_SOCK:-}" ]] && echo "export SSH_AUTH_SOCK='$SSH_AUTH_SOCK'"
        [[ -n "${GNOME_KEYRING_CONTROL:-}" ]] && echo "export GNOME_KEYRING_CONTROL='$GNOME_KEYRING_CONTROL'"
    } > "$ENV_FILE"

    echo "Headless GNOME Wayland session ready:" >&2
    echo "  DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" >&2
    echo "  WAYLAND_DISPLAY=$WAYLAND_DISPLAY" >&2
    echo "  env file: $ENV_FILE" >&2

    if [[ $# -gt 0 ]]; then
        # Run the given command inside the session; stop the shell after.
        "$@"
        exit $?
    fi

    # No command: keep the session alive until gnome-shell exits.
    wait "$SHELL_PID"
    exit $?
fi

# ──────────────────────────────────────────────────────────────────────
# Outer mode: ensure PipeWire, then enter a fresh D-Bus session.
# ──────────────────────────────────────────────────────────────────────

# Mutter's screen casting needs a per-user PipeWire daemon. Start one if
# missing (scoped to this script: killed again when the session ends).
PW_PID=""
WP_PID=""
if ! pgrep -u "$USER" -x pipewire >/dev/null 2>&1; then
    echo "No pipewire daemon for $USER — starting one for this session." >&2
    pipewire >"$STATE_DIR/pipewire.log" 2>&1 &
    PW_PID=$!
    if command -v wireplumber >/dev/null 2>&1; then
        sleep 0.5
        wireplumber >"$STATE_DIR/wireplumber.log" 2>&1 &
        WP_PID=$!
    fi
    sleep 1
    outer_cleanup() {
        [[ -n "$WP_PID" ]] && kill "$WP_PID" 2>/dev/null || true
        [[ -n "$PW_PID" ]] && kill "$PW_PID" 2>/dev/null || true
    }
    trap outer_cleanup EXIT INT TERM
fi

# dbus-run-session gives gnome-shell (and anything passed in "$@") a private
# session bus; --inner re-enters this script on that bus.
dbus-run-session -- "$0" --inner "$@"
