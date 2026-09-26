# Android client

Status: **implemented** on `feature/android-client`. Compiles and unit-tests
pass. Not yet validated on a device.

Goal: reach a ddisplay session from an Android phone or tablet with hardware
decode and touch input, matching the native client's wire protocol.

## How it works

The app is one Compose activity with two screens. A connect screen takes a
`host:port` and keeps a list of recent servers. Once the session connects the
app switches to the session screen, which renders the video and routes input to
the server.

- **Transport.** `OkHttpSessionClient` opens a binary WebSocket to `/ws` and
  reconnects with exponential backoff. It sets `TCP_NODELAY` and disables the
  read timeout to match the native client's socket. `RemoteSession` runs the
  handshake and confines message parsing and decoder work to one consumer
  coroutine off the main thread.
- **Decode.** `MediaCodecVideoDecoder` drives MediaCodec in async mode and
  renders straight onto the `SurfaceView`. It prefers a hardware decoder for
  H.264 (`video/avc`) or AV1 (`video/av01`) and turns on `KEY_LOW_LATENCY` on
  API 30 and later when the codec reports it. `CodecCaps` reports the device's
  decoders to the server, which picks the codec. A fresh decoder drops frames
  until the next keyframe and asks for one when it loses sync.
- **Video layout.** The frame is letterboxed to fit the view. A zoom and pan
  transform sits on top, so `ViewportMath` maps every touch back to a remote
  framebuffer pixel. Pan is clamped so the image always covers its letterbox
  area.
- **Input.** Touch input becomes remote pointer and key messages. A mouse or
  stylus maps its buttons and wheel directly. Trackpad mode moves a relative
  virtual cursor with a crosshair overlay. Direct mode maps the touched point to
  an absolute position. An off-screen text field owns the IME. A single mappable
  character is sent as key events by US-layout position. Anything else is
  pasted. Hardware keyboard keys are forwarded as position-based key events.
- **Clipboard.** `ClipboardSync` writes server clipboard text into the Android
  clipboard for the session's lifetime. It pushes the local clipboard back to
  the server when the app window regains focus and when the user taps the Copy
  control. The last value received from the server is not echoed straight back.

## Controls

The top bar switches between trackpad and direct touch. It toggles the keyboard
and the on-screen keys, and it shows or hides the stats HUD. A reset control
returns zoom to 1x and a Copy control sends the clipboard. The bar auto-hides
after a few seconds and returns when its handle is tapped.

The special-keys bar holds sticky modifiers and common navigation keys, plus an
optional function-key row. A sticky modifier releases after the next
non-modifier key, which makes combos like Ctrl+C reachable one key at a time.

The stats HUD reports round-trip time and frame rate, plus bitrate and decode
time. It also shows dropped frames, the codec and the remote size.

## Input cheat sheet

Trackpad mode:

- One finger moves the cursor.
- Tap for left click.
- Long-press then drag, or double-tap then drag, holds left and drags.
- Two-finger drag scrolls at 1x and pans once zoomed.
- Two-finger tap is a right click.
- Pinch zooms.

Direct mode:

- Tap left-clicks at the touched point.
- Long-press right-clicks.
- Drag holds left and moves.
- Pinch and two-finger drag zoom and pan as in trackpad mode.

An external mouse or stylus maps buttons and wheel directly and positions the
cursor absolutely.

## Build

Open `client-android/` in Android Studio, or from that directory run:

```sh
./gradlew :app:assembleDebug
```

The module targets `compileSdk` 36 and `minSdk` 26. CI on x86_64 builds the APK
artifact. An arm64 machine cannot run `aapt2`, so the resource and packaging
steps fail there. `:core:test` and `:app:compileDebugKotlin` still run on arm64
and cover the protocol code and the input math.

## Constraints

- Single head. `MSG_MONITOR_LAYOUT` and `MSG_MONITOR_FRAME` are ignored, so the
  client shows only the primary head.
- No USB device forwarding.
- The scroll-wheel direction for a physical mouse or trackpad is assumed from
  the Compose sign convention and is not confirmed on hardware.

## To validate

1. On a device, connect to a running server and confirm the video decodes and
   that the reported codec matches the device's hardware decoder.
2. Check both touch modes end to end. Confirm the crosshair tracks in trackpad
   mode and that direct taps land under the finger.
3. Confirm clipboard both ways. Copy on the server and paste on the phone, then
   copy on the phone and paste on the server after refocusing the app.
4. Type with the soft keyboard and a hardware keyboard, and confirm sticky
   modifiers reach combos like Ctrl+C.
5. Confirm reconnect. Drop the network and check that the session recovers or
   offers Retry and Back.
6. Confirm the scroll direction with a physical mouse and correct the sign if it
   is inverted.
