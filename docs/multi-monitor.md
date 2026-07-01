# Multi-monitor support

Status: **in progress** on `feat/multimonitor`. The plumbing is complete and
builds; the RandR head creation and the second-window rendering need validation
on a real dummy-driver session and on hardware.

Goal: plug in a virtual second monitor from the client. It should behave like a
real second display in the session (the desktop sees a separate head with its
own work area and maximize target), and show up on the client as an independent
window the user can place anywhere.

## How it works

One capture, one independent encoded stream per head.

- **Server.** `monitor::apply_layout` widens the X framebuffer to hold the heads
  side by side and declares one RandR monitor per head with `xrandr
  --setmonitor`, which X11 window managers (Mutter, KWin) honour as real
  displays. The screen is captured once; a `HeadStream` per head then encodes
  that head's region as its own stream (the primary head stays zero-copy when it
  covers the whole frame, other heads copy their sub-rect into a tight buffer).
  The authoritative layout is `equal_columns(count, actual_w, actual_h)`,
  recomputed from the real captured size after every resolution change so the
  client always matches the true framebuffer (the driver may snap to a mode).
- **Wire.** `MSG_MONITOR_LAYOUT` (0x08, server to client) carries
  `{monitors:[{id,x,y,width,height}]}`, sent on connect and on every change.
  Head 0's frames use `MSG_VIDEO_FRAME` (0x01, so the web client still shows the
  primary); heads 1+ use `MSG_MONITOR_FRAME` (0x09), tagged with the head id.
  `MSG_REQUEST_ADD_MONITOR` (0x29) / `MSG_REQUEST_REMOVE_MONITOR` (0x2a) go the
  other way.
- **Client.** Each head is a window with its own GPU surface and its own
  `DecodePipeline`, decoding that head's stream. The input map adds the head's
  framebuffer offset so a click in the second window lands on the second head.
  The second window is opened/closed to match the layout, can be moved and
  resized freely, and closing it unplugs the monitor.

## Lifecycle and edge cases handled

- Add/remove applies promptly (the request opens the capture loop's recheck
  immediately rather than waiting up to a second).
- Closing the second window unplugs the head; when no writable client remains,
  the server restores a single head so the session is never left altered.
- `--resize-to-client` is skipped while more than one head is plugged in (the
  layout owns the framebuffer size then). Single-head resizes keep the layout in
  step, so input scaling follows the resolution.
- Zero-size or malformed layout entries are dropped on the client.
- Framebuffer width is capped (the heads must fit within the resize limit).

## Constraints

- Needs a RandR-capable X server (the Xorg "dummy" driver, the usual ddisplay
  setup). Ignored on the Wayland backend and unavailable under Xvfb (no resize).
- The client supports one extra head (two windows total). The server encodes up
  to `MAX_MONITORS` heads, so raising the client cap is the only change needed
  for more.
- Adaptive bitrate drives the primary head live; extra heads use a fixed
  per-resolution bitrate, rebuilt when the layout changes.
- The single-head restore relies on RandR re-reporting the output's automatic
  monitor once the user monitors are deleted (standard behaviour).

## To validate

1. On a dummy-driver virtual session, add a monitor from F2 and confirm the WM
   treats head 2 as a separate display (drag a window across, maximize on it).
2. Confirm the second client window shows head 2, that input lands on the right
   head, and that move/resize/close behave.
3. Confirm remove (and disconnect) restores a clean single-head session.
