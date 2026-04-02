use clap::Parser;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod protocol;
mod capture;
mod encoder;
mod transport;
mod input;

use capture::x11::X11Capturer;
use encoder::Encoder;
use input::x11::X11InputInjector;
use protocol::ClientEvent;

#[derive(Parser)]
#[command(name = "ddisplay-server", about = "Remote display server with H.264 streaming")]
struct Args {
    /// Address to bind the WebSocket server to.
    #[arg(short, long, default_value = "0.0.0.0:9550")]
    bind: String,

    /// Target frames per second.
    #[arg(short, long, default_value_t = 30)]
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

    // --- H.264 encoder ---
    let encoder = encoder::openh264_enc::OpenH264Encoder::new(
        screen_w, screen_h, args.fps, args.bitrate,
    )?;

    // --- X11 input injector ---
    let injector = X11InputInjector::new()?;
    injector.release_stuck_inputs()?;

    // --- WebSocket transport ---
    let client_dir = PathBuf::from(&args.client_dir);
    let (frame_tx, input_rx, keyframe_cache) =
        transport::websocket::start_server(args.bind.clone(), client_dir).await?;

    tracing::info!("Listening on http://{}", args.bind);

    // --- Input handler (dedicated blocking thread) ---
    spawn_input_handler(injector, input_rx);

    // --- Capture+encode loop (dedicated thread, zero-copy) ---
    let fps = args.fps;
    let kf_cache = keyframe_cache;

    tokio::task::spawn_blocking(move || {
        if let Err(e) = capture_encode_loop(capturer, encoder, frame_tx, kf_cache, screen_w, screen_h, fps) {
            tracing::error!("Capture loop error: {}", e);
        }
    }).await?;

    Ok(())
}

/// Spawn a dedicated blocking task that reads client input events and
/// injects them into the X11 server.
fn spawn_input_handler(injector: X11InputInjector, mut input_rx: mpsc::Receiver<ClientEvent>) {
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
            if let Err(e) = injector.inject_event(&event) {
                tracing::error!("Failed to inject input event: {}", e);
            }
        }
        tracing::info!("Input handler shutting down");
    });
}

/// Tight capture+encode loop running on a dedicated OS thread.
///
/// Avoids per-frame `spawn_blocking` overhead and uses zero-copy SHM reads.
fn capture_encode_loop(
    capturer: X11Capturer,
    mut encoder: encoder::openh264_enc::OpenH264Encoder,
    frame_tx: transport::websocket::FrameSender,
    keyframe_cache: transport::websocket::KeyframeCache,
    screen_w: u32,
    screen_h: u32,
    fps: u32,
) -> anyhow::Result<()> {
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let keyframe_interval = fps as u64 * 2; // IDR every 2 seconds

    let mut frame_count: u64 = 0;
    let mut next_frame_time = Instant::now();
    let mut fps_timer = Instant::now();
    let mut fps_frame_count: u64 = 0;

    loop {
        // Sleep until next frame time
        let now = Instant::now();
        if next_frame_time > now {
            std::thread::sleep(next_frame_time - now);
        }
        next_frame_time += frame_interval;
        // If we fell behind, skip to now instead of trying to catch up
        if next_frame_time < Instant::now() {
            next_frame_time = Instant::now() + frame_interval;
        }

        let force_kf = frame_count == 0 || frame_count % keyframe_interval == 0;

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

        // Get cursor position (cheap X11 roundtrip, do it every frame)
        let cursor = capturer.get_cursor_info().ok();

        // Build wire messages
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

        if let Some(ci) = cursor {
            let wire = protocol::encode_cursor_update(
                ci.x.max(0) as u16,
                ci.y.max(0) as u16,
                ci.visible,
            );
            let _ = frame_tx.send(wire);
        }

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
