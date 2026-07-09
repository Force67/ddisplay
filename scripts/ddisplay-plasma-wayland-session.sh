#!/usr/bin/env bash
# ddisplay: lean Plasma Wayland "shell" session launcher.
#
# Run by kwin_wayland via --exit-with-session, i.e. INSIDE an already-running
# `kwin_wayland --virtual` compositor (kwin exports WAYLAND_DISPLAY for us).
#
# Unlike plasma_session / ksmserver, this does NOT start a second
# kwin_wayland. Handing --exit-with-session the full plasma_session makes
# ksmserver spawn its own kwin, leaving a nested duplicate compositor at the
# default 1024x768 (the portal then captures that instead of our virtual
# output). Here the single virtual kwin stays THE compositor and we only bring
# up the Plasma shell plus the handful of KDE daemons a usable desktop wants,
# then block on plasmashell. When plasmashell exits, kwin exits with it.
set -u

log() { echo "[ddisplay-session] $*" >&2; }

LIBEXEC="/usr/lib/$(uname -m)-linux-gnu/libexec"

start_bg() {
    local name="$1"; shift
    local bin
    bin="$(command -v "$name" 2>/dev/null || true)"
    [[ -z "$bin" && -x "$LIBEXEC/$name" ]] && bin="$LIBEXEC/$name"
    if [[ -n "$bin" ]]; then
        log "starting $bin"
        "$bin" "$@" >/dev/null 2>&1 &
    else
        log "skip (not installed): $name"
    fi
}

# Best-effort KDE daemons (also D-Bus activatable; starting them explicitly is
# belt-and-suspenders so the panel, shortcuts and tray come up cleanly).
start_bg kded5
start_bg kactivitymanagerd
start_bg kglobalaccel5
start_bg org_kde_powerdevil
start_bg polkit-kde-authentication-agent-1

log "starting plasmashell (foreground — session lifetime)"
exec plasmashell
