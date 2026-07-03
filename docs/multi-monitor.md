# Multi-monitor support

Status: **implemented** on `feat/multimonitor` for any head count up to 16.
Needs validation on a real dummy-driver session and on hardware.

Goal: plug in as many virtual monitors as you want from the client. Each
behaves like a real display in the session (the desktop sees a separate head
with its own work area and maximize target), and shows up on the client as an
independent window the user can place anywhere.

## How it works

One capture, one independent encoded stream per head.

- **Server.** `monitor::apply_layout` sizes the X framebuffer to hold the
  heads in a grid (`monitor::grid_dims`: as square as possible, preferring
  wide — 2 and 3 heads are a row, 4 is 2x2, up to 4x4 = 16) and declares one
  RandR monitor per head with `xrandr --setmonitor`, which X11 window managers
  (Mutter, KWin) honour as real displays. Every head gets the primary head's
  current size, so the layout doesn't drift across add/remove cycles. The
  screen is captured once; a `HeadStream` per head then encodes that head's
  region as its own stream (the primary head stays zero-copy when it covers
  the whole frame, other heads copy their sub-rect into a tight buffer). The
  authoritative layout is `monitor::layout_rects(count, actual_w, actual_h)`,
  recomputed from the real captured size after every resolution change so the
  client always matches the true framebuffer (the driver may snap to a mode).
- **Wire.** `MSG_MONITOR_LAYOUT` (0x08, server to client) carries
  `{monitors:[{id,x,y,width,height}]}`, sent on connect and on every change.
  Head 0's frames use `MSG_VIDEO_FRAME` (0x01, so the web client still shows
  the primary); heads 1+ use `MSG_MONITOR_FRAME` (0x09), tagged with the head
  id. `MSG_REQUEST_ADD_MONITOR` (0x29) / `MSG_REQUEST_REMOVE_MONITOR` (0x2a)
  go the other way. `MSG_REQUEST_KEYFRAME` (0x21) takes an optional head id so
  one stream can resync without re-IDRing every other head; without a payload
  (web client) every head re-IDRs.
- **Client.** Each head is a window with its own GPU surface and its own
  `DecodePipeline`, decoding that head's stream. The input map adds the head's
  framebuffer offset so a click in any window lands on its head. Windows are
  positional (`extras[i]` shows head `i+1`): on a layout change they are
  opened, closed, or reassigned to their new head (fresh decoder + targeted
  keyframe request) rather than torn down. Closing any extra window unplugs
  one monitor; the server renumbers and the survivors reassign.

## Efficiency

- **Per-head damage gating.** The X Damage extension reports the bounding box
  of what changed (`take_damage_hint`). Heads the box never touches skip
  encoding entirely, so activity on one monitor doesn't burn encode time and
  bandwidth on the others. Without region info (Wayland, foreign events, the
  post-input hedge frame) every head encodes, as before.
- **Per-head keyframes.** A single head's decoder reset (drop, un-occlude,
  window reassignment) requests an IDR for that stream only, via the keyframe
  bitmask in `StreamControl`.
- Adaptive bitrate drives the primary head live; extra heads use a fixed
  per-resolution bitrate, rebuilt when the layout changes. Extra-head decode
  drops feed the same congestion controller.
- Occluded/minimized windows skip decode and render, and resync with a
  targeted keyframe request when shown again.

## Lifecycle and edge cases handled

- Add/remove applies promptly (the request opens the capture loop's recheck
  immediately rather than waiting up to a second).
- Closing an extra window unplugs a head; when no writable client remains,
  the server restores a single head so the session is never left altered.
- `--resize-to-client` is skipped while more than one head is plugged in (the
  layout owns the framebuffer size then). Single-head resizes keep the layout
  in step, so input scaling follows the resolution.
- Zero-size or malformed layout entries are dropped on the client.
- The grid must fit the 7680x4320 framebuffer limit; `apply_layout` rejects
  oversized requests with a clear error (16 heads need 1920x1080 or smaller
  heads; at 2560x1440, 9 heads fit).
- A freshly created window requests its own keyframe, so it can't stay black
  when the layout-change IDR raced ahead of window creation.

## Constraints

- Needs a RandR-capable X server (the Xorg "dummy" driver, the usual ddisplay
  setup). Ignored on the Wayland backend and unavailable under Xvfb (no
  resize).
- Up to `monitor::MAX_MONITORS` (16) heads, mirrored by the client overlay's
  add-button gate.
- The single-head restore relies on RandR re-reporting the output's automatic
  monitor once the user monitors are deleted (standard behaviour).

## To validate

1. On a dummy-driver virtual session, add monitors from F2 and confirm the WM
   treats each head as a separate display (drag a window across, maximize on
   it), including the second grid row from head 4 on.
2. Confirm each client window shows its head, that input lands on the right
   head, and that move/resize/close behave — including closing a *middle*
   window (survivors renumber onto the lower head ids).
3. Confirm remove (and disconnect) restores a clean single-head session.
4. With 3+ heads, verify the damage gating: play a video on head 0 and check
   (server FPS log / client F3 HUD) that idle heads receive no frames.
