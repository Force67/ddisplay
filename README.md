# ddisplay

ddisplay captures a Linux desktop session and streams it to remote clients with low latency. Encoding runs on the GPU with NVENC (H.264 or AV1) and falls back to OpenH264 in software when no NVIDIA encoder is present. Input and clipboard sync both ways, and files transfer in either direction.

> Status: pre-alpha (`0.1.0-prealpha`). The wire protocol, CLI and APIs are unstable and change without notice.

The repository is a Rust workspace. The server in `server/` runs the capture and encoding pipeline on Linux, with X11 (MIT-SHM) and Wayland (Mutter native, with an XDG Desktop Portal fallback) backends. It adapts bitrate and resolution at runtime and requests keyframes on demand. The native client in `client-native/` runs on Windows and Linux and is built on winit and wgpu. On Windows it decodes in hardware through Media Foundation and D3D11, and elsewhere it falls back to OpenH264 or rav1d in software. A browser client in `client/` uses Media Source Extensions and is served by the server. Color conversion from BGRA to YUV/NV12 lives in `server/color/`, with AVX2 and NEON acceleration over a scalar baseline.

## Building

The toolchain is Rust stable, and the server uses the 2024 edition. Build one component at a time with `cargo build --release -p <crate>`.

### Server (Linux)

```sh
sudo apt install build-essential pkg-config clang libclang-dev \
                 libpipewire-0.3-dev libxcb1-dev nasm
```

The clang and libpipewire-0.3-dev packages provide bindgen and the Wayland backend headers. The X11 backend links libxcb, so the libxcb1-dev package is required. The nasm assembler builds the OpenH264 sources on x86_64. NVENC headers are vendored under `server/third_party/ffnvcodec`, and CUDA and NVENC load at runtime, so the build needs no CUDA toolkit.

```sh
cargo build --release -p ddisplay-server
```

### Native client (Windows and Linux)

On Linux:

```sh
sudo apt install build-essential pkg-config libssl-dev nasm
```

On Windows the build needs the MSVC toolchain and nasm.

```sh
cargo build --release -p ddisplay-client
```

CI builds both clients and the server for amd64 and arm64 on every push.

## Running

Start the server against an X11 display, then connect from a browser or the native client.

```sh
./target/release/ddisplay-server --bind 0.0.0.0:9550 --display :10
```

Opening `http://<server-host>:9550` in a browser loads the web client, and the native client connects to the same address. When `--display` is omitted the server tries to auto-detect a Wayland session. The `scripts/` directory has helpers that start headless GNOME and KDE sessions to capture.

The commonly used flags are below, and `--help` lists the rest.

| Flag | Default | Purpose |
|------|---------|---------|
| `--bind` | `0.0.0.0:9550` | Listen address |
| `--fps` | `60` | Target frame rate |
| `--bitrate` | `0` | Video bitrate in bps, where 0 auto-scales to resolution and fps |
| `--codec` | `auto` | `auto` / `av1` / `h264` |
| `--encoder` | `auto` | `auto` / `nvenc` / `openh264` |
| `--backend` | `auto` | `auto` / `x11` / `wayland` / `portal` |
| `--resize-to-client` | `true` | Resize the X display to the client resolution using RandR |
| `--shared-dir <dir>` | none | Serve a directory over HTTP for file transfer |

Connect the native client to the same address.

```sh
./target/release/ddisplay-client --server <server-host>:9550
```

F2 opens the overlay menu and F3 toggles the stats HUD, which `DDISPLAY_STATS=1` starts enabled. Setting `DDISPLAY_NO_HWDEC=1` forces software decode.

## Layout

```
server/         Linux capture and encode server
server/color/   BGRA to YUV/NV12 SIMD color conversion
client-native/  Windows and Linux native client (winit and wgpu)
client/         Browser client (MSE)
scripts/        Headless GNOME and KDE session launchers
```
