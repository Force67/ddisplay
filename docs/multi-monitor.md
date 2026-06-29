# Multi-monitor support

Status: **in progress** on `feat/multimonitor`. The plumbing is complete and
builds; the RandR head creation and the second-window rendering need validation
on a real dummy-driver session and on hardware.

Goal: plug in a virtual second monitor from the client. It should behave like a
real second display in the session (the desktop sees a separate head with its
own work area and maximize target), and show up on the client as an independent
window the user can place anywhere.

## How it works

One video stream, one or more heads.

- **Server.** `monitor::apply_layout` widens the X framebuffer to hold the heads
  side by side and declares one RandR monitor per head with `xrandr
  --setmonitor`, which X11 window managers (Mutter, KWin) honour as real
  displays. The screen capture still grabs the whole root as a single encoded
  stream. The authoritative client-facing layout is `equal_columns(count,
  actual_w, actual_h)`, recomputed from the real captured size after every
  resolution change so the client's crops and input always tile the true
  framebuffer (the driver may snap to a nearby mode).
- **Wire.** `MSG_MONITOR_LAYOUT` (0x08, server to client) carries
  `{monitors:[{id,x,y,width,height}]}`, sent on connect and on every change.
  `MSG_REQUEST_ADD_MONITOR` (0x29) / `MSG_REQUEST_REMOVE_MONITOR` (0x2a) go the
  other way.
- **Client.** Each head is a window with its own GPU surface. The renderer
  samples only that head's sub-rect of the shared decoded frame (a crop set in
  the uniform and applied in the shader), and the input map adds the head's
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
- Phase 1 supports one extra head (two total) and one shared stream, so a second
  head doubles the encoded width. Per-head independent streams are a possible
  later optimisation.
- The single-head restore relies on RandR re-reporting the output's automatic
  monitor once the user monitors are deleted (standard behaviour).

## To validate

1. On a dummy-driver virtual session, add a monitor from F2 and confirm the WM
   treats head 2 as a separate display (drag a window across, maximize on it).
2. Confirm the second client window shows head 2, that input lands on the right
   head, and that move/resize/close behave.
3. Confirm remove (and disconnect) restores a clean single-head session.
