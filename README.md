# ddisplay

GPU-accelerated remote display. It captures a Linux desktop session, encodes it
on the GPU (NVENC H.264/AV1, with an OpenH264 software fallback), and streams it
with low latency to a native Windows/Linux client or a browser. Input, clipboard,
and file transfer are bidirectional.

> **Status: pre-alpha (`0.1.0-prealpha`).** Experimental and unstable — the wire
> protocol, CLI, and APIs change without notice. Expect rough edges.

## Components

- **`server/`** — Linux capture/encode/stream server. X11 (MIT-SHM) and Wayland
  (Mutter native, XDG Desktop Portal) backends; NVENC via CUDA with an OpenH264
  software fallback; adaptive bitrate, dynamic resolution, on-demand keyframes.
  Serves the web client and the WebSocket stream on a single port.
- **`client-native/`** — Native client for Windows and Linux (winit + wgpu).
  Hardware decode on Windows via Media Foundation / D3D11 (NVDEC / QuickSync);
  OpenH264 and rav1d software decode elsewhere.
- **`client/`** — Browser client (Media Source Extensions), served by the server.
- **`server/color/`** — Standalone BGRA → YUV/NV12 color conversion (scalar,
  AVX2, NEON).

## Building

Rust stable (the server uses the 2024 edition). Build one component at a time
with `cargo build --release -p <crate>`.

### Server (Linux)

System packages (Debian/Ubuntu):

```sh
sudo apt install build-essential pkg-config clang libclang-dev \
                 libpipewire-0.3-dev libxcb1-dev nasm
```

- `clang` / `libclang-dev` and `libpipewire-0.3-dev` — bindgen and headers for
  the Wayland/PipeWire backend.
- `libxcb1-dev` — the X11 backend links libxcb.
- `nasm` — OpenH264 assembly (x86_64 only).
- NVENC headers are vendored (`server/third_party/ffnvcodec`); CUDA and NVENC are
  loaded at runtime, so no CUDA toolkit is needed to build.

```sh
cargo build --release -p ddisplay-server
```

### Native client (Windows / Linux)

- Windows: the MSVC toolchain and `nasm`.
- Linux: `build-essential pkg-config libssl-dev nasm`.

```sh
cargo build --release -p ddisplay-client
```

Binaries for Linux and Windows (amd64 and arm64) are also built by CI on every
push.

## Running

### Server

```sh
./target/release/ddisplay-server --bind 0.0.0.0:9550 --display :10
```

Open `http://<server-host>:9550` in a browser for the web client, or point the
native client at the same address. The server captures the X11 display given by
`--display` (or auto-detects a Wayland session). `scripts/` has helpers that spin
up headless GNOME/KDE sessions to capture.

Common flags (`--help` for the full list):

| Flag | Default | Purpose |
|------|---------|---------|
| `--bind` | `0.0.0.0:9550` | Listen address |
| `--fps` | `60` | Target frame rate |
| `--bitrate` | `0` (auto) | Video bitrate in bps; auto scales to resolution/fps/codec |
| `--codec` | `auto` | `av1`, `h264`, or `auto` (AV1 when NVENC supports it) |
| `--encoder` | `auto` | `nvenc`, `openh264`, or `auto` |
| `--backend` | `auto` | `x11`, `wayland`, `portal`, or `auto` |
| `--resize-to-client` | `true` | Resize the X display to the client's resolution (RandR) |
| `--shared-dir <dir>` | — | Serve a directory over HTTP for file transfer |

### Native client

```sh
./target/release/ddisplay-client --server <server-host>:9550
```

- **F2** — overlay menu (display mode, stats, codec).
- **F3** — latency / fps / bandwidth HUD (`DDISPLAY_STATS=1` to start it enabled).
- `DDISPLAY_NO_HWDEC=1` forces software decode.

## Layout

```
server/         Linux capture/encode/stream server
server/color/   BGRA -> YUV/NV12 SIMD color conversion
client-native/  Windows/Linux native client (winit + wgpu)
client/         Browser client (MSE)
scripts/        Headless GNOME/KDE session launchers
```
