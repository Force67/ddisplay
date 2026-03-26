use clap::Parser;
use std::path::PathBuf;
use std::time::Instant;
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
    #[arg(long, default_value_t = 5_000_000)]
    bitrate: u32,

    /// Path to the directory containing the web client files.
    #[arg(short, long, default_value = "./client")]
    client_dir: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ddisplay=info")),
        )
        .init();

    tracing::info!("ddisplay-server starting");

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

    // --- WebSocket transport ---
    let client_dir = PathBuf::from(&args.client_dir);
    let (frame_tx, input_rx, keyframe_cache) =
        transport::websocket::start_server(args.bind.clone(), client_dir).await?;

    tracing::info!("Listening on http://{}", args.bind);

    // --- Input handler task ---
    spawn_input_handler(injector, input_rx);

    // --- Capture / encode loop ---
    run_capture_loop(capturer, encoder, frame_tx, keyframe_cache, screen_w, screen_h, args.fps).await?;

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

/// Capture frames from X11, encode with H.264, and broadcast to all
/// connected WebSocket clients.
async fn run_capture_loop(
    mut capturer: X11Capturer,
    mut encoder: encoder::openh264_enc::OpenH264Encoder,
    frame_tx: transport::websocket::FrameSender,
    keyframe_cache: transport::websocket::KeyframeCache,
    screen_w: u32,
    screen_h: u32,
    fps: u32,
) -> anyhow::Result<()> {
    let frame_interval = std::time::Duration::from_secs_f64(1.0 / fps as f64);
    let mut interval = tokio::time::interval(frame_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut frame_count: u64 = 0;
    let mut force_keyframe = true;
    let mut fps_timer = Instant::now();
    let mut fps_frame_count: u64 = 0;

    loop {
        interval.tick().await;

        // Force keyframe every 2 seconds for late-joining clients.
        if frame_count > 0 && frame_count % (fps as u64 * 2) == 0 {
            force_keyframe = true;
        }

        let kf = force_keyframe;
        force_keyframe = false;
        let fc = frame_count;

        // Capture + encode on the blocking pool.
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(
            encoder::EncodedPacket,
            Option<capture::CursorInfo>,
            X11Capturer,
            encoder::openh264_enc::OpenH264Encoder,
        )> {
            let frame = capturer.capture_frame()?;
            let packet = encoder.encode(
                &frame.data,
                frame.width,
                frame.height,
                frame.stride,
                kf,
            )?;

            let cursor = if fc % 10 == 0 {
                capturer.get_cursor_info().ok()
            } else {
                None
            };

            Ok((packet, cursor, capturer, encoder))
        })
        .await?;

        let (packet, cursor, cap, enc) = result?;
        capturer = cap;
        encoder = enc;

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
