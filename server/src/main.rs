use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod protocol;
mod capture;
mod encoder;
mod transport;
mod input;
mod clipboard;
mod files;

use capture::x11::X11Capturer;
use encoder::Encoder;
use input::x11::X11InputInjector;
use protocol::ClientEvent;
use transport::websocket::{ServerMetadata, ServerRuntimeConfig};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EncoderChoice {
    /// Auto-detect: try NVENC first, fall back to OpenH264.
    Auto,
    /// Force NVIDIA NVENC hardware encoder.
    Nvenc,
    /// Force OpenH264 software encoder.
    Openh264,
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
#[command(name = "ddisplay-server", about = "Remote display server with H.264 streaming")]
struct Args {
    /// Address to bind the WebSocket server to.
    #[arg(short, long, default_value = "0.0.0.0:9550")]
    bind: String,

    /// Target frames per second.
    #[arg(short, long, default_value_t = 60)]
    fps: u32,

    /// Video bitrate in bits per second.
    #[arg(long, default_value_t = 8_000_000)]
    bitrate: u32,

    /// Path to the directory containing the web client files.
    #[arg(short, long, default_value = "./client")]
    client_dir: String,

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
    #[arg(long, value_enum, default_value_t = CodecChoice::Auto)]
    codec: CodecChoice,

    /// Directory to share over HTTP for easy file transfer.
    /// If omitted, the /files endpoint is not served.
    #[arg(long)]
    shared_dir: Option<PathBuf>,

    /// Path to the Windows client binary to serve at /download/client.
    /// The file will be wrapped in a zip and offered as a download.
    #[arg(long)]
    client_bin: Option<PathBuf>,
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
    tracing::info!(
        "X11 target: DISPLAY={} XAUTHORITY={}",
        std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_string()),
        std::env::var("XAUTHORITY").unwrap_or_else(|_| "<unset>".to_string()),
    );

    // --- X11 screen capturer ---
    let capturer = X11Capturer::new()?;
    let screen_w = capturer.screen_width();
    let screen_h = capturer.screen_height();
    tracing::info!("Screen: {}x{}", screen_w, screen_h);

    // --- Encoder + codec selection ---
    let (encoder, codec_name): (Box<dyn Encoder + Send>, &str) = match args.encoder {
        EncoderChoice::Openh264 => {
            tracing::info!("Encoder: OpenH264 H.264 (forced)");
            let enc = encoder::openh264_enc::OpenH264Encoder::new(
                screen_w, screen_h, args.fps, args.bitrate,
            )?;
            (Box::new(enc), "h264")
        }
        _ => {
            // NVENC path (auto or forced)
            let (has_h264, has_av1) = encoder::nvenc_enc::probe_codecs();
            let forced_nvenc = matches!(args.encoder, EncoderChoice::Nvenc);

            // Resolve codec
            let nvenc_codec = match args.codec {
                CodecChoice::Av1 if has_av1 => encoder::nvenc_enc::CODEC_AV1,
                CodecChoice::Av1 => {
                    if forced_nvenc {
                        anyhow::bail!("AV1 requested but NVENC does not support it on this GPU");
                    }
                    tracing::warn!("AV1 requested but not available, falling back to H.264");
                    encoder::nvenc_enc::CODEC_H264
                }
                CodecChoice::Auto => {
                    if has_av1 { encoder::nvenc_enc::CODEC_AV1 }
                    else { encoder::nvenc_enc::CODEC_H264 }
                }
                CodecChoice::H264 => encoder::nvenc_enc::CODEC_H264,
            };

            if has_h264 || has_av1 {
                match encoder::nvenc_enc::NvencEncoder::new(
                    screen_w, screen_h, args.fps, args.bitrate, nvenc_codec,
                ) {
                    Ok(enc) => {
                        let name = enc.codec_name();
                        tracing::info!("Encoder: NVENC {} ({})", name,
                            if forced_nvenc { "forced" } else { "auto-detected" });
                        (Box::new(enc), if nvenc_codec == encoder::nvenc_enc::CODEC_AV1 { "av1" } else { "h264" })
                    }
                    Err(e) if !forced_nvenc => {
                        tracing::warn!("NVENC init failed, falling back to OpenH264: {}", e);
                        let enc = encoder::openh264_enc::OpenH264Encoder::new(
                            screen_w, screen_h, args.fps, args.bitrate,
                        )?;
                        (Box::new(enc), "h264")
                    }
                    Err(e) => return Err(e),
                }
            } else if !forced_nvenc {
                tracing::info!("Encoder: OpenH264 H.264 (NVENC not available)");
                let enc = encoder::openh264_enc::OpenH264Encoder::new(
                    screen_w, screen_h, args.fps, args.bitrate,
                )?;
                (Box::new(enc), "h264")
            } else {
                anyhow::bail!("NVENC forced but not available on this system");
            }
        }
    };

    // --- X11 input injector ---
    let injector = X11InputInjector::new()?;
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
        width: screen_w,
        height: screen_h,
        fps: args.fps,
        bitrate: args.bitrate,
        codec: codec_name.to_string(),
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
    let (frame_tx, input_rx, keyframe_cache) =
        transport::websocket::start_server(args.bind.clone(), client_dir, metadata, runtime, shared_dir, client_bin).await?;

    tracing::info!("Listening on http://{}", args.bind);

    // --- Input handler (dedicated blocking thread) ---
    let input_pending = Arc::new(AtomicBool::new(false));
    let force_keyframe = Arc::new(AtomicBool::new(false));
    spawn_input_handler(injector, input_rx, Arc::clone(&input_pending), Arc::clone(&force_keyframe));

    // --- Capture+encode loop (dedicated thread, zero-copy) ---
    let fps = args.fps;
    let kf_cache = keyframe_cache;

    tokio::task::spawn_blocking(move || {
        if let Err(e) = capture_encode_loop(capturer, encoder, frame_tx, kf_cache, screen_w, screen_h, fps, input_pending, force_keyframe) {            tracing::error!("Capture loop error: {}", e);
        }
    }).await?;

    Ok(())
}

/// Spawn a dedicated blocking task that reads client input events and
/// injects them into the X11 server.
fn spawn_input_handler(
    injector: X11InputInjector,
    mut input_rx: mpsc::Receiver<ClientEvent>,
    input_pending: Arc<AtomicBool>,
    force_keyframe: Arc<AtomicBool>,
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
                    force_keyframe.store(true, Ordering::Release);
                    input_pending.store(true, Ordering::Release);
                    continue;
                }
                _ => {}
            }
            if let Err(e) = injector.inject_event(&event) {
                tracing::error!("Failed to inject input event: {}", e);
            }
            input_pending.store(true, Ordering::Release);
        }
        tracing::info!("Input handler shutting down");
    });
}

/// Tight capture+encode loop running on a dedicated OS thread.
///
/// Avoids per-frame `spawn_blocking` overhead and uses zero-copy SHM reads.
fn capture_encode_loop(
    mut capturer: X11Capturer,
    mut encoder: Box<dyn Encoder + Send>,
    frame_tx: transport::websocket::FrameSender,
    keyframe_cache: transport::websocket::KeyframeCache,
    screen_w: u32,
    screen_h: u32,
    fps: u32,
    input_pending: Arc<AtomicBool>,
    force_keyframe: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let active_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let idle_interval = Duration::from_secs_f64(1.0 / 10.0); // 10 fps when idle
    // After this much silence we drop to idle FPS.
    let idle_timeout = Duration::from_millis(500);
    let keyframe_interval = fps as u64 * 2; // IDR every 2 seconds

    let mut frame_count: u64 = 0;
    let mut next_frame_time = Instant::now();
    let mut fps_timer = Instant::now();
    let mut fps_frame_count: u64 = 0;
    let mut last_activity = Instant::now();

    loop {
        let now = Instant::now();
        let input_arrived = input_pending.swap(false, Ordering::AcqRel);
        if input_arrived {
            last_activity = now;
        }

        // Pick frame interval based on recent activity.
        let active = now.duration_since(last_activity) < idle_timeout;
        let frame_interval = if active { active_interval } else { idle_interval };

        if !input_arrived && next_frame_time > now {
            std::thread::sleep(next_frame_time - now);
        }
        next_frame_time += frame_interval;
        if next_frame_time < Instant::now() {
            next_frame_time = Instant::now() + frame_interval;
        }

        let client_requested_kf = force_keyframe.swap(false, Ordering::AcqRel);
        let force_kf = frame_count == 0 || frame_count % keyframe_interval == 0 || client_requested_kf;
        if client_requested_kf {
            tracing::debug!("[keyframe] forcing IDR frame on client request");
        }
        let has_damage = capturer.has_damage();

        // Damage counts as activity too (animations, video playback, etc.)
        if has_damage {
            last_activity = Instant::now();
        }

        // Always send cursor updates regardless of screen damage
        if let Some(ci) = capturer.get_cursor_info().ok() {
            let wire = protocol::encode_cursor_update(
                ci.x.max(0) as u16,
                ci.y.max(0) as u16,
                ci.visible,
            );
            let _ = frame_tx.send(wire);
        }

        // Skip capture+encode if nothing changed (saves CPU, GPU, and bandwidth).
        // Always capture on keyframe intervals (for new client sync).
        // Also always capture on the frame after input (app may have just responded).
        if !force_kf && !has_damage && !input_arrived {
            frame_count += 1;
            fps_frame_count += 1;
            continue;
        }

        // Zero-copy capture: borrow SHM buffer directly
        let frame = capturer.capture_frame_ref()?;

        // Encode directly from the SHM reference (no 8MB copy)
        let packet = encoder.encode(
            frame.data,
            frame.width,
            frame.height,
            frame.stride,
            force_kf,
        )?;

        // Build wire message
        let wire = protocol::encode_video_frame(
            packet.keyframe,
            packet.pts,
            screen_w as u16,
            screen_h as u16,
            &packet.data,
        );

        if packet.keyframe {
            *keyframe_cache.lock() = Some(wire.clone());
        }

        let _ = frame_tx.send(wire);

        frame_count += 1;
        fps_frame_count += 1;

        let elapsed = fps_timer.elapsed();
        if elapsed.as_secs() >= 5 {
            let actual_fps = fps_frame_count as f64 / elapsed.as_secs_f64();
            tracing::info!(
                "FPS: {:.1} (frames: {}, target: {})",
                actual_fps,
                frame_count,
                fps,
            );
            fps_timer = Instant::now();
            fps_frame_count = 0;
        }
    }
}
