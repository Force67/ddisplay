# ddisplay

ddisplay captures a Linux desktop session and streams it to remote clients with low latency. Encoding runs on the GPU through NVENC (H.264 or AV1) and falls back to OpenH264 in software. Input and clipboard sync both ways, and files transfer in either direction. Client USB devices can be forwarded into the session KVM-style over usbip (see `docs/usb-forwarding.md`).

> Status: pre-alpha (`0.1.0-prealpha`). Protocol and CLI change without notice.

It is a Rust workspace. `server/` does capture and encoding on Linux over X11 or Wayland. `client-native/` is a winit and wgpu client for Windows and Linux, with hardware decode on Windows. `client/` is a browser client the server hosts itself. `server/color/` holds the SIMD color conversion. `client-android/` is a Compose client for Android phones and tablets, with hardware decode over MediaCodec.

## Build

```sh
sudo apt install build-essential pkg-config clang libclang-dev \
                 libpipewire-0.3-dev libxcb1-dev nasm
cargo build --release -p ddisplay-server
```

The native client needs `libssl-dev` and `nasm` on Linux, or the MSVC toolchain and `nasm` on Windows. On Windows it prefers a DX12 renderer that imports Media Foundation's D3D11 NV12 output through a shared GPU texture ring, avoiding video-frame readback and re-upload. Unsupported adapters or drivers automatically use the existing wgpu upload path. Set `DDISPLAY_NO_DX12=1` to force that fallback or `DDISPLAY_NO_HWDEC=1` to disable Media Foundation decoding.

```sh
cargo build --release -p ddisplay-client
```

NVENC is optional and loaded at runtime, so building needs no CUDA toolkit. CI covers both clients and the server on amd64 and arm64.

## Run

```sh
./target/release/ddisplay-server --bind 0.0.0.0:9550 --display :10
./target/release/ddisplay-client --server <host>:9550
```

The web client is served at `http://<host>:9550`. Without `--display` the server looks for a Wayland session, and `scripts/` can start headless GNOME or KDE sessions to capture. In the native client, F2 opens the overlay and F3 the stats HUD.

| Flag | Default | Purpose |
|------|---------|---------|
| `--bind` | `0.0.0.0:9550` | Listen address |
| `--fps` | `60` | Target frame rate |
| `--bitrate` | `0` | Bitrate in bps, 0 auto-scales |
| `--codec` | `auto` | `auto` / `av1` / `h264` |
| `--encoder` | `auto` | `auto` / `nvenc` / `openh264` |
| `--backend` | `auto` | `auto` / `x11` / `wayland` / `portal` |
| `--resize-to-client` | `true` | Match the X display to the client size |
| `--shared-dir <dir>` | none | Serve a folder over HTTP |
| `--allow-usb` | off | Accept forwarded client USB devices (no auth, see `docs/usb-forwarding.md`) |

`--help` lists the rest.

## Layout

```
server/         capture and encode server (Linux)
server/color/   SIMD BGRA to YUV/NV12 conversion
client-native/  native client (Windows and Linux)
client/         browser client
client-android/ Android client (Compose)
scripts/        headless GNOME and KDE launchers
```
