use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod protocol;
mod capture;
mod control;
mod encoder;
mod transport;
mod input;
mod clipboard;
mod files;
mod resize;
mod wayland_session;
mod portal_session;

use capture::x11::X11Capturer;
use capture::ScreenCapturer;
use control::StreamControl;
use encoder::Encoder;
use input::x11::X11InputInjector;
use input::InputInjector;
use protocol::{ClientEvent, SessionInfo};
use transport::websocket::{ServerMetadata, ServerRuntimeConfig};

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum EncoderChoice {
    /// Auto-detect: try NVENC first, fall back to OpenH264.
    Auto,
    /// Force NVIDIA NVENC hardware encoder.
    Nvenc,
    /// Force OpenH264 software encoder.
    Openh264,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum BackendChoice {
    /// Auto-detect: Wayland when WAYLAND_DISPLAY (or DDISPLAY_WAYLAND=1) is
    /// set and either Mutter's ScreenCast/RemoteDesktop D-Bus services or an
    /// XDG Desktop Portal RemoteDesktop backend are reachable; X11 otherwise.
    Auto,
    /// X11: MIT-SHM capture + XTest input injection.
    X11,
    /// Wayland: PipeWire screen capture + RemoteDesktop input. Tries the
    /// Mutter-native session first (GNOME), falls back to the XDG portal.
    Wayland,
    /// Wayland via the XDG Desktop Portal only (org.freedesktop.portal.
    /// RemoteDesktop/ScreenCast) — the path for KDE Plasma Wayland sessions
    /// (xdg-desktop-portal-kde) and wlroots compositors.
    Portal,
}

/// Resolved capture/input backend.
#[derive(Clone, Copy, Debug, PartialEq)]
enum BackendKind {
    X11,
    Wayland,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CodecChoice {
    /// H.264 / AVC (universal browser support)
    H264,
    /// AV1 (~50% better compression, needs modern browser/client)
    Av1,
    /// Auto: use AV1 if NVENC supports it, else H.264
    Auto,
}

#[derive(Parser)]
#[command(name = "ddisplay-server", about = "Remote display server with H.264/AV1 streaming")]
struct Args {
    /// Address to bind the WebSocket server to.
    #[arg(short, long, default_value = "0.0.0.0:9550")]
    bind: String,

    /// Target frames per second.
    #[arg(short, long, default_value_t = 60)]
    fps: u32,

    /// Video bitrate in bits per second. 0 = auto (scaled to resolution,
    /// fps and codec; e.g. 1080p60 H.264 ≈ 8.7 Mbps, 4K60 AV1 ≈ 20 Mbps).
    #[arg(long, default_value_t = 0)]
    bitrate: u32,

    /// Path to the directory containing the web client files.
    #[arg(short, long, default_value = "./client")]
    client_dir: String,

    /// Capture/input backend: X11 (Xvfb/Xorg), Wayland (Mutter native with
    /// XDG portal fallback), or portal (XDG Desktop Portal only — KDE
    /// Plasma / wlroots).
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    backend: BackendChoice,

    /// X11 display to capture and inject into, for example `:10`.
    #[arg(long)]
    display: Option<String>,

    /// Xauthority file to use for the selected X11 display.
    #[arg(long)]
    xauthority: Option<PathBuf>,

    /// Human-readable session name shown in the client UI.
    #[arg(long, default_value = "session")]
    session_name: String,

    /// Video encoder to use. "auto" tries NVENC first, falls back to OpenH264.
    #[arg(long, value_enum, default_value_t = EncoderChoice::Auto)]
    encoder: EncoderChoice,

    /// Video codec. "auto" prefers AV1 when NVENC supports it, else H.264.
    /// The server switches dynamically when a connected client can't decode
    /// the preferred codec (e.g. AV1 server + H.264-only client → H.264).
    #[arg(long, value_enum, default_value_t = CodecChoice::Auto)]
    codec: CodecChoice,

    /// Resize the X display to the native resolution of the connecting
    /// client (uses RandR; the stream then matches the client 1:1, up to 4K+).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    resize_to_client: bool,

    /// Directory to share over HTTP for easy file transfer.
    /// If omitted, the /files endpoint is not served.
    #[arg(long)]
    shared_dir: Option<PathBuf>,

    /// Path to the Windows client binary to serve at /download/client.
    /// The file will be wrapped in a zip and offered as a download.
    #[arg(long)]
    client_bin: Option<PathBuf>,
}

/// Everything needed to (re)build an encoder at runtime.
struct EncoderSetup {
    choice: EncoderChoice,
    has_nvenc_h264: bool,
    has_nvenc_av1: bool,
}

impl EncoderSetup {
    fn probe(choice: EncoderChoice) -> Self {
        let (has_nvenc_h264, has_nvenc_av1) = if choice == EncoderChoice::Openh264 {
            (false, false)
        } else {
            encoder::nvenc_enc::probe_codecs()
        };
        Self { choice, has_nvenc_h264, has_nvenc_av1 }
    }

    /// Codecs this server can produce, in preference order.
    fn supported_codecs(&self, forced: CodecChoice) -> Vec<String> {
        let mut codecs = Vec::new();
        if self.has_nvenc_av1 {
            codecs.push("av1".to_string());
        }
        codecs.push("h264".to_string());
        match forced {
            CodecChoice::H264 => vec!["h264".to_string()],
            CodecChoice::Av1 => vec!["av1".to_string()],
            CodecChoice::Auto => codecs,
        }
    }

    /// Build an encoder for `codec` at the given dimensions. Returns the
    /// encoder and the codec actually in use (falls back to OpenH264 H.264
    /// when NVENC is unavailable and the encoder wasn't forced).
    fn build(
        &self,
        codec: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> anyhow::Result<(Box<dyn Encoder + Send>, String)> {
        if self.choice == EncoderChoice::Openh264 {
            let enc = encoder::openh264_enc::OpenH264Encoder::new(width, height, fps, bitrate)?;
            return Ok((Box::new(enc), "h264".to_string()));
        }

        let forced_nvenc = self.choice == EncoderChoice::Nvenc;
        let nvenc_codec = if codec == "av1" && self.has_nvenc_av1 {
            Some(encoder::nvenc_enc::CODEC_AV1)
        } else if self.has_nvenc_h264 {
            Some(encoder::nvenc_enc::CODEC_H264)
        } else {
            None
        };

        if let Some(nc) = nvenc_codec {
            match encoder::nvenc_enc::NvencEncoder::new(width, height, fps, bitrate, nc) {
                Ok(enc) => {
                    let name = enc.codec_name().to_string();
                    return Ok((Box::new(enc), name));
                }
                Err(e) if forced_nvenc => return Err(e),
                Err(e) => {
                    tracing::warn!("NVENC init failed, falling back to OpenH264: {}", e);
                }
            }
        } else if forced_nvenc {
            anyhow::bail!("NVENC forced but not available on this system");
        }

        let enc = encoder::openh264_enc::OpenH264Encoder::new(width, height, fps, bitrate)?;
        Ok((Box::new(enc), "h264".to_string()))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(display) = &args.display {
        unsafe { std::env::set_var("DISPLAY", display) };
    }
    if let Some(xauthority) = &args.xauthority {
        unsafe { std::env::set_var("XAUTHORITY", xauthority) };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ddisplay=info")),
        )
        .init();

    tracing::info!("ddisplay-server starting");

    // --- Backend selection (X11 vs Wayland) ---
    let backend = match args.backend {
        BackendChoice::X11 => BackendKind::X11,
        BackendChoice::Wayland | BackendChoice::Portal => BackendKind::Wayland,
        BackendChoice::Auto => {
            // An explicit --display is an X11 request (e.g. the virtual Xvfb
            // session) — don't let the launching desktop's WAYLAND_DISPLAY
            // hijack it.
            let wayland_hint = args.display.is_none()
                && (std::env::var_os("WAYLAND_DISPLAY").is_some()
                    || std::env::var("DDISPLAY_WAYLAND").map(|v| v == "1").unwrap_or(false));
            if wayland_hint
                && (wayland_session::mutter_available() || portal_session::portal_available())
            {
                BackendKind::Wayland
            } else {
                BackendKind::X11
            }
        }
    };
    tracing::info!("Backend: {:?}", backend);
    if backend == BackendKind::Wayland {
        // The NVENC zero-copy source path registers capture buffers with
        // CUDA. Wayland capture double-buffers in heap Vecs that may
        // reallocate on a stream resize while still registered — only the
        // X11 SHM segment is address-stable, so disable zero-copy here
        // (the GPU conversion kernel still runs; it just uses an HtoD copy).
        unsafe { std::env::set_var("DDISPLAY_NO_ZEROCOPY", "1") };
    }
    if backend == BackendKind::X11 {
        tracing::info!(
            "X11 target: DISPLAY={} XAUTHORITY={}",
            std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_string()),
            std::env::var("XAUTHORITY").unwrap_or_else(|_| "<unset>".to_string()),
        );
    }

    // --- Screen capturer + input injector ---
    let (capturer, mut injector): (Box<dyn ScreenCapturer>, Box<dyn InputInjector>) =
        match backend {
            BackendKind::X11 => (
                Box::new(X11Capturer::new()?),
                Box::new(X11InputInjector::new()?),
            ),
            BackendKind::Wayland => {
                // Session flavor: Mutter native (GNOME) preferred, XDG
                // portal (KDE Plasma / wlroots) as the fallback — unless
                // `--backend portal` forces the portal flow.
                let session: Arc<dyn wayland_session::RemoteSessionApi> =
                    if args.backend == BackendChoice::Portal {
                        tracing::info!("[wayland] session flavor: XDG Desktop Portal (forced)");
                        Arc::new(portal_session::PortalRemoteSession::new()?)
                    } else if wayland_session::mutter_available() {
                        tracing::info!("[wayland] session flavor: Mutter native");
                        Arc::new(wayland_session::MutterRemoteSession::new()?)
                    } else {
                        tracing::info!(
                            "[wayland] session flavor: XDG Desktop Portal \
                             (Mutter ScreenCast/RemoteDesktop not on this bus)"
                        );
                        Arc::new(portal_session::PortalRemoteSession::new()?)
                    };
                (
                    Box::new(capture::wayland::WaylandCapturer::new(Arc::clone(&session))?),
                    Box::new(input::wayland::WaylandInputInjector::new(session)),
                )
            }
        };
    let screen_w = capturer.width();
    let screen_h = capturer.height();
    tracing::info!("Screen: {}x{}", screen_w, screen_h);

    // --- Encoder + codec selection ---
    let setup = EncoderSetup::probe(args.encoder);
    let initial_codec = match args.codec {
        CodecChoice::Av1 => {
            if !setup.has_nvenc_av1 {
                if args.encoder == EncoderChoice::Nvenc {
                    anyhow::bail!("AV1 requested but NVENC does not support it on this GPU");
                }
                tracing::warn!("AV1 requested but not available, falling back to H.264");
                "h264"
            } else {
                "av1"
            }
        }
        CodecChoice::H264 => "h264",
        CodecChoice::Auto => {
            if setup.has_nvenc_av1 { "av1" } else { "h264" }
        }
    };

    // 0 = auto: scale bitrate to resolution/fps/codec.
    let user_bitrate = (args.bitrate > 0).then_some(args.bitrate);
    let initial_bitrate =
        user_bitrate.unwrap_or_else(|| control::auto_bitrate(screen_w, screen_h, args.fps, initial_codec));

    let (enc, codec_name) =
        setup.build(initial_codec, screen_w, screen_h, args.fps, initial_bitrate)?;
    tracing::info!(
        "Encoder ready: {} @ {}x{}, {} bps{}",
        codec_name, screen_w, screen_h, initial_bitrate,
        if user_bitrate.is_none() { " (auto)" } else { "" },
    );

    // --- Shared stream control (codec switches, ABR, resize requests) ---
    let session = SessionInfo {
        width: screen_w,
        height: screen_h,
        fps: args.fps,
        codec: codec_name.clone(),
        bitrate: initial_bitrate,
    };
    let stream_control = Arc::new(StreamControl::new(session, initial_bitrate));
    let server_codecs = setup.supported_codecs(args.codec);
    tracing::info!("Server codecs (preference order): {:?}", server_codecs);

    // --- Input sanity reset (releases keys left pressed by old sessions) ---
    injector.release_stuck_inputs()?;

    // --- WebSocket transport ---
    let client_dir = PathBuf::from(&args.client_dir);
    let shared_dir = if let Some(ref dir) = args.shared_dir {
        std::fs::create_dir_all(dir)?;
        tracing::info!("Shared folder: {}", dir.display());
        Some(dir.clone())
    } else {
        None
    };
    let metadata = ServerMetadata {
        session_name: args.session_name.clone(),
        display: std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_string()),
        xauthority: std::env::var("XAUTHORITY").unwrap_or_else(|_| "<unset>".to_string()),
    };
    let runtime = ServerRuntimeConfig {
        bind_addr: args.bind.clone(),
        client_dir: client_dir.clone(),
        session_name: args.session_name.clone(),
        display: metadata.display.clone(),
        xauthority: metadata.xauthority.clone(),
        fps: args.fps,
        bitrate: args.bitrate,
    };
    let client_bin = if let Some(ref p) = args.client_bin {
        if !p.exists() {
            tracing::warn!("--client-bin path not found: {}", p.display());
            None
        } else {
            tracing::info!("Client binary: {}", p.display());
            Some(p.clone())
        }
    } else {
        None
    };
    let (frame_tx, input_rx) = transport::websocket::start_server(
        args.bind.clone(),
        client_dir,
        metadata,
        runtime,
        shared_dir,
        client_bin,
        stream_control.clone(),
        server_codecs,
        args.resize_to_client,
    )
    .await?;

    tracing::info!("Listening on http://{}", args.bind);

    // --- Input handler (dedicated blocking thread) ---
    spawn_input_handler(injector, input_rx, stream_control.clone());

    // --- Capture+encode loop (dedicated thread, zero-copy) ---
    let fps = args.fps;
    let loop_control = stream_control;

    tokio::task::spawn_blocking(move || {
        if let Err(e) = capture_encode_loop(
            capturer,
            backend,
            enc,
            codec_name,
            setup,
            user_bitrate,
            frame_tx,
            fps,
            loop_control,
        ) {
            tracing::error!("Capture loop error: {}", e);
        }
    })
    .await?;

    Ok(())
}

/// Spawn a dedicated blocking task that reads client input events and
/// injects them into the X11 server.
fn spawn_input_handler(
    mut injector: Box<dyn InputInjector>,
    mut input_rx: mpsc::Receiver<ClientEvent>,
    control: Arc<StreamControl>,
) {
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<ClientEvent>();

    tokio::spawn(async move {
        while let Some(event) = input_rx.recv().await {
            if sync_tx.send(event).is_err() {
                break;
            }
        }
    });

    tokio::task::spawn_blocking(move || {
        for event in sync_rx {
            match &event {
                ClientEvent::RequestKeyframe => {
                    tracing::debug!("[keyframe] client requested keyframe");
                    control.force_keyframe.store(true, Ordering::Release);
                    control.notify_activity();
                    continue;
                }
                _ => {}
            }
            if let Err(e) = injector.inject_event(&event) {
                tracing::error!("Failed to inject input event: {}", e);
            }
            control.notify_activity();
        }
        tracing::info!("Input handler shutting down");
    });
}

/// Tight capture+encode loop running on a dedicated OS thread.
///
/// Avoids per-frame `spawn_blocking` overhead and uses zero-copy SHM reads.
/// Between frames it applies reconfiguration requests from `StreamControl`:
/// codec switches (client capability arbitration), bitrate changes (adaptive
/// bitrate; live NVENC reconfigure when possible), display resizes, and X
/// screen-size changes (encoder + capturer rebuild).
#[allow(clippy::too_many_arguments)]
fn capture_encode_loop(
    mut capturer: Box<dyn ScreenCapturer>,
    backend: BackendKind,
    mut encoder: Box<dyn Encoder + Send>,
    mut codec: String,
    setup: EncoderSetup,
    user_bitrate: Option<u32>,
    frame_tx: transport::websocket::FrameSender,
    fps: u32,
    control: Arc<StreamControl>,
) -> anyhow::Result<()> {
    let active_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let idle_interval = Duration::from_secs_f64(1.0 / 10.0); // 10 fps when idle
    // After this much silence we drop to idle FPS.
    let idle_timeout = Duration::from_millis(500);

    let mut screen_w = capturer.width();
    let mut screen_h = capturer.height();
    let mut applied_bitrate = control.target_bitrate.load(Ordering::Acquire);

    let mut frame_count: u64 = 0;
    let mut last_tick = Instant::now();
    let mut fps_timer = Instant::now();
    let mut fps_frame_count: u64 = 0;
    let mut last_activity = Instant::now();
    let mut last_recheck = Instant::now();
    let mut last_cursor: Option<(u16, u16, bool)> = None;

    loop {
        // ---- Frame pacing ----
        // Two-phase wait. Phase 1 is the hard rate cap: never start two
        // captures closer than the active interval — a flood of mouse moves
        // must not outrun the target fps, or the client's decoder drowns and
        // every queued frame becomes latency. Phase 2 stretches the wait to
        // the idle schedule, but is cut short the instant activity arrives.
        let active = last_activity.elapsed() < idle_timeout;
        let frame_interval = if active { active_interval } else { idle_interval };
        let earliest = last_tick + active_interval;
        let scheduled = last_tick + frame_interval;

        let now = Instant::now();
        if earliest > now {
            std::thread::sleep(earliest - now);
        }
        if scheduled > earliest && !control.input_pending.load(Ordering::Acquire) {
            control.wait_activity_until(scheduled);
        }
        last_tick = Instant::now();

        let input_arrived = control.input_pending.swap(false, Ordering::AcqRel);
        if input_arrived {
            last_activity = last_tick;
        }

        // ---- Reconfiguration checks (cheap; heavier X round-trips ~1/sec) ----
        let mut rebuild = false; // encoder must be rebuilt
        let mut resolution_changed = false;

        if last_recheck.elapsed() >= Duration::from_secs(1) {
            last_recheck = Instant::now();

            // Client requested a display resize (--resize-to-client).
            if let Some((rw, rh)) = control.resize_request.lock().take() {
                if (rw, rh) != (screen_w, screen_h) {
                    if backend == BackendKind::Wayland {
                        // Dynamic resize is out of scope for the Wayland v1
                        // backend (the headless virtual monitor is fixed).
                        tracing::info!(
                            "[resize] client requested {}x{} — ignored (wayland backend)",
                            rw, rh,
                        );
                    } else {
                        tracing::info!("[resize] client requested {}x{}", rw, rh);
                        if let Err(e) = resize::resize_display(rw, rh) {
                            tracing::warn!("[resize] failed: {:#}", e);
                        }
                    }
                }
            }

            // Display changed size (RandR / PipeWire renegotiation) →
            // re-init capturer + rebuild encoder.
            if let Some((nw, nh)) = capturer.size_changed() {
                tracing::info!(
                    "Screen resolution changed: {}x{} -> {}x{}",
                    screen_w, screen_h, nw, nh,
                );
                match capturer.reinit() {
                    Ok(()) => {
                        screen_w = capturer.width();
                        screen_h = capturer.height();
                        rebuild = true;
                        resolution_changed = true;
                    }
                    Err(e) => tracing::error!("Failed to rebuild capturer after resize: {}", e),
                }
            }
        }

        // Codec switch requested by capability arbitration. The live codec
        // only changes once the new encoder is actually built.
        let mut pending_codec: Option<String> = None;
        if let Some(new_codec) = control.desired_codec.lock().take() {
            if new_codec != codec {
                tracing::info!("[codec] switching {} -> {}", codec, new_codec);
                pending_codec = Some(new_codec);
                rebuild = true;
            }
        }

        // Adaptive bitrate target changed.
        let target_bitrate = control.target_bitrate.load(Ordering::Acquire);
        if target_bitrate != applied_bitrate && !rebuild {
            if encoder.set_bitrate(target_bitrate) {
                applied_bitrate = target_bitrate;
                control.session.lock().bitrate = target_bitrate;
            } else {
                // Encoder can't reconfigure live (OpenH264) — rebuild it.
                rebuild = true;
            }
        }

        if rebuild {
            let build_codec = pending_codec.unwrap_or_else(|| codec.clone());
            // Recompute the ceiling for the (possibly new) resolution/codec.
            let ceiling = user_bitrate
                .unwrap_or_else(|| control::auto_bitrate(screen_w, screen_h, fps, &build_codec));
            control.bitrate_ceiling.store(ceiling, Ordering::Release);
            // On a resolution change the old adaptive target is meaningless —
            // restart from the ceiling and let ABR back off if needed.
            let bitrate = if resolution_changed {
                ceiling
            } else {
                control.target_bitrate.load(Ordering::Acquire).min(ceiling)
            };
            control.target_bitrate.store(bitrate, Ordering::Release);

            match setup.build(&build_codec, screen_w, screen_h, fps, bitrate) {
                Ok((new_enc, actual_codec)) => {
                    encoder = new_enc;
                    codec = actual_codec;
                    applied_bitrate = bitrate;

                    let info = SessionInfo {
                        width: screen_w,
                        height: screen_h,
                        fps,
                        codec: codec.clone(),
                        bitrate,
                    };
                    *control.session.lock() = info.clone();
                    let _ = frame_tx.send(protocol::encode_session_info(&info));
                    control.force_keyframe.store(true, Ordering::Release);
                    last_activity = Instant::now();
                    tracing::info!(
                        "Encoder rebuilt: {} @ {}x{}, {} bps",
                        codec, screen_w, screen_h, bitrate,
                    );
                }
                Err(e) => {
                    tracing::error!("Encoder rebuild failed ({}); keeping old encoder", e);
                }
            }
        }

        // The GOP is infinite — IDRs only happen on demand: the first frame,
        // a new client connecting, a recovery request (client decode drop or
        // server broadcast lag), or after an encoder rebuild. Periodic IDRs
        // are pure quality loss with single-frame VBV (they show up as a
        // flash of pixelation) and the recovery paths above cover every
        // resync case.
        let requested_kf = control.force_keyframe.swap(false, Ordering::AcqRel);
        let force_kf = frame_count == 0 || requested_kf;
        if requested_kf {
            tracing::debug!("[keyframe] forcing IDR frame on request");
        }
        let has_damage = capturer.has_new_frame();

        // Damage counts as activity too (animations, video playback, etc.)
        if has_damage {
            last_activity = Instant::now();
        }

        // Send cursor updates when the cursor actually moved — unless the
        // backend embeds the cursor in the frames (Wayland EMBEDDED mode).
        // Also resend on keyframes so new clients learn the position.
        if !capturer.embeds_cursor() {
            if let Some(ci) = capturer.cursor_info().ok() {
                let cur = (ci.x.max(0) as u16, ci.y.max(0) as u16, ci.visible);
                if force_kf || last_cursor != Some(cur) {
                    last_cursor = Some(cur);
                    let _ = frame_tx.send(protocol::encode_cursor_update(cur.0, cur.1, cur.2));
                }
            }
        }

        // Skip capture+encode if nothing changed (saves CPU, GPU, and bandwidth).
        // Always capture on the frame after input (app may have just responded).
        if !force_kf && !has_damage && !input_arrived {
            frame_count += 1;
            fps_frame_count += 1;
            continue;
        }

        // Zero-copy capture: borrow SHM buffer directly.
        // Capture can fail transiently around display resizes (the SHM
        // segment no longer matches the root geometry) — force a recheck and
        // keep the loop alive instead of killing the stream.
        let frame = match capturer.frame_ref() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("Capture failed ({}); rechecking display geometry", e);
                last_recheck = Instant::now() - Duration::from_secs(1);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };

        // Encode directly from the SHM reference (no 8MB copy)
        let packet = match encoder.encode(
            frame.data,
            frame.width,
            frame.height,
            frame.stride,
            force_kf,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Encode failed ({}); skipping frame", e);
                control.force_keyframe.store(true, Ordering::Release);
                continue;
            }
        };

        // Build wire message
        let wire = protocol::encode_video_frame(
            packet.keyframe,
            packet.pts,
            screen_w as u16,
            screen_h as u16,
            &packet.data,
        );

        let _ = frame_tx.send(wire);

        frame_count += 1;
        fps_frame_count += 1;

        let elapsed = fps_timer.elapsed();
        if elapsed.as_secs() >= 5 {
            let actual_fps = fps_frame_count as f64 / elapsed.as_secs_f64();
            tracing::info!(
                "FPS: {:.1} (frames: {}, target: {}) | {} {}x{} @ {} bps",
                actual_fps,
                frame_count,
                fps,
                codec,
                screen_w,
                screen_h,
                applied_bitrate,
            );
            fps_timer = Instant::now();
            fps_frame_count = 0;
        }
    }
}
