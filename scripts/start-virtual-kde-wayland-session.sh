#!/usr/bin/env bash
set -euo pipefail

# Headless KDE Plasma Wayland session for ddisplay's portal backend.
#
# BEST-EFFORT script: kwin_wayland is not installed on the development box,
# so this could not be runtime-tested. It is modeled on
# start-virtual-gnome-wayland-session.sh and the documented kwin/plasma
# headless invocations. Expect to tweak it on a real Plasma install.
#
# Starts a virtual-output KWin/Plasma Wayland session inside its own D-Bus
# session (dbus-run-session), ensures xdg-desktop-portal + the KDE backend
# (xdg-desktop-portal-kde) are up on that bus, and captures the session's
# DBUS_SESSION_BUS_ADDRESS / WAYLAND_DISPLAY to a state file:
#
#   $HOME/.local/state/ddisplay/wayland-session.env
#
# Usage:
#   start-virtual-kde-wayland-session.sh                # session only
#   start-virtual-kde-wayland-session.sh CMD [ARGS...]  # run CMD inside it
#
#   DDISPLAY_VIRTUAL_SCREEN=2560x1440 start-virtual-kde-wayland-session.sh \
#       ./target/release/ddisplay-server --backend portal --bind 0.0.0.0:9551
#
# ─── PERMISSIONS ON A HEADLESS SESSION (IMPORTANT) ────────────────────────
# The XDG portal RemoteDesktop/ScreenCast flow is permission-gated: the first
# Start() pops an interactive dialog INSIDE the Plasma session. On a headless
# session there is no way to click it. Options, in order of preference:
#
#   1. Pre-authorize (Plasma >= 6.3): KDE's portal checks a `kde-authorized`
#      table in the xdg-desktop-portal permission store. Host apps without a
#      known app id match the empty string:
#
#          flatpak permission-set kde-authorized remote-desktop "" yes
#
#      (or for a registered app id:
#          flatpak permission-set kde-authorized remote-desktop org.kde.krdpserver yes)
#
#      Without the flatpak CLI the same record can be written via D-Bus:
#          dbus-send --session --print-reply \
#            --dest=org.freedesktop.impl.portal.PermissionStore \
#            /org/freedesktop/impl/portal/PermissionStore \
#            org.freedesktop.impl.portal.PermissionStore.SetPermission \
#            string:kde-authorized boolean:true string:remote-desktop \
#            string:'' array:string:'yes'
#
#      NOTE: this must run on THE SESSION BUS THE PORTAL USES (i.e. inside
#      this script's dbus-run-session, before the server starts), and only
#      covers the remote-desktop portal type. See
#      https://develop.kde.org/docs/administration/portal-permissions/
#      This script pre-seeds it automatically (DDISPLAY_KDE_PREAUTH=0 to skip).
#
#   2. Approve once from an attended session: run the server in a normal
#      (seated) Plasma session once, click "Allow" + "Remember", and the
#      restore token ddisplay saves at
#      ~/.local/state/ddisplay/portal-restore-token skips the dialog on all
#      later runs (persist_mode=2). Plasma < 6.x had bugs where persistence
#      did not survive reboots (KDE bug 480235) — prefer option 1 on 6.3+.
#
#   3. There is NO supported kwriteconfig knob to disable the dialog itself;
#      the permission store (option 1) is the supported pre-seeding path.
# ──────────────────────────────────────────────────────────────────────────

SCREEN_SPEC="${DDISPLAY_VIRTUAL_SCREEN:-1920x1080}"
SCREEN_SPEC="${SCREEN_SPEC%x24}"   # tolerate WxHxDEPTH specs from the X script
SCREEN_W="${SCREEN_SPEC%%x*}"
SCREEN_H="${SCREEN_SPEC##*x}"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ddisplay"
ENV_FILE="$STATE_DIR/wayland-session.env"
LOG_FILE="$STATE_DIR/kde-wayland-headless.log"

mkdir -p "$STATE_DIR"

# ──────────────────────────────────────────────────────────────────────
# Inner mode: we are now inside dbus-run-session with a fresh session bus.
# ──────────────────────────────────────────────────────────────────────
if [[ "${1:-}" == "--inner" ]]; then
    shift

    export XDG_SESSION_TYPE="wayland"
    export XDG_CURRENT_DESKTOP="KDE"
    export XDG_SESSION_DESKTOP="KDE"
    export DESKTOP_SESSION="plasma"
    export KDE_FULL_SESSION="true"
    # Software rendering keeps kwin alive on GPU-less/headless boxes; drop
    # this if you want kwin to use the real GPU.
    export QT_QPA_PLATFORM="wayland"

    PIDS=()
    cleanup() {
        local pid
        for pid in "${PIDS[@]}"; do
            kill "$pid" 2>/dev/null || true
        done
        wait 2>/dev/null || true
    }
    trap cleanup EXIT INT TERM

    # Record pre-existing wayland sockets so we can detect the new one.
    RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    declare -A OLD_SOCKETS=()
    for s in "$RUNTIME_DIR"/wayland-*; do
        [[ -e "$s" && "$s" != *.lock ]] && OLD_SOCKETS["$s"]=1
    done

    # ── Start the compositor + Plasma shell ──────────────────────────
    # Two known-good invocations, tried in order:
    #   (a) startplasma-wayland with KWIN args via KWIN_WAYLAND_VIRTUAL —
    #       on recent Plasma, startplasma-wayland spawns kwin_wayland itself
    #       and forwards $KDEWM-style options poorly, so instead:
    #   (b) kwin_wayland --virtual ... --exit-with-session startplasma-waylandsession
    #       (kwin spawns the plasma session as its child; this is what
    #       plasma's own kwin_wayland_wrapper does on some distros).
    # If only kwin (no full plasma shell) is needed for portal screencast,
    # plain kwin_wayland --virtual is enough — xdg-desktop-portal-kde talks
    # to kwin's own org.kde.KWin.ScreenShot2/RemoteDesktop interfaces.
    COMP_PID=""
    if command -v kwin_wayland >/dev/null 2>&1; then
        echo "Starting kwin_wayland --virtual (${SCREEN_W}x${SCREEN_H})..." >&2
        KWIN_ARGS=(
            --virtual
            --width "$SCREEN_W" --height "$SCREEN_H"
            --no-lockscreen
            --xwayland
        )
        if command -v startplasma-waylandsession >/dev/null 2>&1; then
            # (b) kwin owns the session and starts the plasma shell.
            kwin_wayland "${KWIN_ARGS[@]}" \
                --exit-with-session startplasma-waylandsession \
                >"$LOG_FILE" 2>&1 &
            COMP_PID=$!
        else
            # Plain kwin; portals can still screencast/inject without the
            # full plasma shell (no panel/desktop, just a black root).
            kwin_wayland "${KWIN_ARGS[@]}" >"$LOG_FILE" 2>&1 &
            COMP_PID=$!
        fi
    elif command -v startplasma-wayland >/dev/null 2>&1; then
        # (a) Newer Plasma: startplasma-wayland spawns kwin itself; virtual
        # outputs are requested through KWIN_WAYLAND env knobs.
        echo "Starting startplasma-wayland (kwin spawned internally)..." >&2
        export KWIN_WAYLAND_NO_PERMISSION_CHECKS=1
        startplasma-wayland >"$LOG_FILE" 2>&1 &
        COMP_PID=$!
    else
        echo "Neither kwin_wayland nor startplasma-wayland found — install plasma-workspace/kwin." >&2
        exit 1
    fi
    PIDS+=("$COMP_PID")

    # Wait for the new wayland socket.
    WL_DISPLAY=""
    for _ in $(seq 1 60); do
        if ! kill -0 "$COMP_PID" 2>/dev/null; then
            echo "compositor exited early — see $LOG_FILE" >&2
            tail -n 20 "$LOG_FILE" >&2 || true
            exit 1
        fi
        for s in "$RUNTIME_DIR"/wayland-*; do
            [[ -e "$s" && "$s" != *.lock && -z "${OLD_SOCKETS[$s]:-}" ]] || continue
            WL_DISPLAY="$(basename "$s")"
        done
        [[ -n "$WL_DISPLAY" ]] && break
        sleep 0.5
    done
    if [[ -z "$WL_DISPLAY" ]]; then
        echo "Timed out waiting for the kwin wayland socket — see $LOG_FILE" >&2
        exit 1
    fi
    export WAYLAND_DISPLAY="$WL_DISPLAY"
    export DDISPLAY_WAYLAND=1

    # ── Pre-seed KDE portal pre-authorization (Plasma >= 6.3) ─────────
    # Writes the kde-authorized/remote-desktop record for the empty app id
    # so the headless Start() does not block on an unclickable dialog.
    # Harmless on older Plasma (the table is simply ignored).
    if [[ "${DDISPLAY_KDE_PREAUTH:-1}" == "1" ]]; then
        if command -v flatpak >/dev/null 2>&1; then
            flatpak permission-set kde-authorized remote-desktop "" yes \
                && echo "Pre-authorized remote-desktop in kde-authorized (flatpak permission-set)." >&2 \
                || echo "WARN: kde-authorized pre-seed failed (needs xdg-permission-store on this bus)." >&2
        else
            dbus-send --session --print-reply \
                --dest=org.freedesktop.impl.portal.PermissionStore \
                /org/freedesktop/impl/portal/PermissionStore \
                org.freedesktop.impl.portal.PermissionStore.SetPermission \
                string:kde-authorized boolean:true string:remote-desktop \
                string:'' array:string:'yes' >/dev/null 2>&1 \
                && echo "Pre-authorized remote-desktop in kde-authorized (PermissionStore)." >&2 \
                || echo "WARN: kde-authorized pre-seed failed (no xdg-permission-store?)." >&2
        fi
    fi

    # ── Ensure xdg-desktop-portal + the KDE backend on this bus ───────
    # Order matters: the impl backend must be up (or D-Bus-activatable)
    # when the frontend starts resolving portals.
    PORTAL_KDE=""
    for p in /usr/libexec/xdg-desktop-portal-kde \
             /usr/lib/x86_64-linux-gnu/libexec/xdg-desktop-portal-kde \
             /usr/lib/xdg-desktop-portal-kde; do
        [[ -x "$p" ]] && PORTAL_KDE="$p" && break
    done
    if [[ -n "$PORTAL_KDE" ]]; then
        "$PORTAL_KDE" >"$STATE_DIR/xdg-desktop-portal-kde.log" 2>&1 &
        PIDS+=("$!")
        sleep 1
    else
        echo "WARN: xdg-desktop-portal-kde binary not found — relying on D-Bus activation." >&2
    fi
    PORTAL_FRONT=""
    for p in /usr/libexec/xdg-desktop-portal \
             /usr/lib/x86_64-linux-gnu/xdg-desktop-portal \
             /usr/lib/xdg-desktop-portal; do
        [[ -x "$p" ]] && PORTAL_FRONT="$p" && break
    done
    if [[ -n "$PORTAL_FRONT" ]]; then
        "$PORTAL_FRONT" >"$STATE_DIR/xdg-desktop-portal.log" 2>&1 &
        PIDS+=("$!")
    else
        echo "WARN: xdg-desktop-portal binary not found — relying on D-Bus activation." >&2
    fi

    # Wait for the portal RemoteDesktop service on this bus.
    ready=""
    for _ in $(seq 1 30); do
        if dbus-send --session --print-reply --dest=org.freedesktop.DBus \
            /org/freedesktop/DBus org.freedesktop.DBus.GetNameOwner \
            string:org.freedesktop.portal.Desktop >/dev/null 2>&1; then
            ready=1
            break
        fi
        sleep 0.5
    done
    if [[ -z "$ready" ]]; then
        echo "Timed out waiting for org.freedesktop.portal.Desktop — see $STATE_DIR/xdg-desktop-portal.log" >&2
        exit 1
    fi

    # State file for launching the server from outside this script.
    {
        echo "# Generated by start-virtual-kde-wayland-session.sh ($(date -Is))"
        echo "export DBUS_SESSION_BUS_ADDRESS='$DBUS_SESSION_BUS_ADDRESS'"
        echo "export WAYLAND_DISPLAY='$WAYLAND_DISPLAY'"
        echo "export XDG_RUNTIME_DIR='$RUNTIME_DIR'"
        echo "export XDG_CURRENT_DESKTOP=KDE"
        echo "export DDISPLAY_WAYLAND=1"
        echo "export KDE_COMPOSITOR_PID=$COMP_PID"
    } > "$ENV_FILE"

    echo "Headless KDE Plasma Wayland session ready:" >&2
    echo "  DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" >&2
    echo "  WAYLAND_DISPLAY=$WAYLAND_DISPLAY" >&2
    echo "  env file: $ENV_FILE" >&2

    if [[ $# -gt 0 ]]; then
        # Run the given command inside the session; tear down after.
        "$@"
        exit $?
    fi

    # No command: keep the session alive until the compositor exits.
    wait "$COMP_PID"
    exit $?
fi

# ──────────────────────────────────────────────────────────────────────
# Outer mode: ensure PipeWire, then enter a fresh D-Bus session.
# ──────────────────────────────────────────────────────────────────────

# KWin's screen casting needs a per-user PipeWire daemon. Start one if
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

# dbus-run-session gives kwin/plasma (and anything passed in "$@") a private
# session bus; --inner re-enters this script on that bus.
dbus-run-session -- "$0" --inner "$@"
