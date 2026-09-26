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
mod head_stream;
mod monitor;
mod transport;
mod input;
mod usb;
mod clipboard;
mod files;
mod resize;
mod wayland_session;
mod portal_session;

use capture::x11::X11Capturer;
use capture::ScreenCapturer;
use control::StreamControl;
use encoder::Encoder;
use head_stream::HeadStream;
use protocol::MonitorRect;
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

    /// Accept USB devices forwarded by clients (attached via usbip vhci-hcd).
    /// There is no authentication: anyone who can reach the port can then
    /// attach arbitrary USB devices to this machine.
    #[arg(long)]
    allow_usb: bool,
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

    // Backend selection (X11 vs Wayland)
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

    // Screen capturer + input injector. On the Wayland backend the session is
    // kept so the capture loop can add/remove heads (Mutter virtual monitors).
    let mut wl_session: Option<Arc<dyn wayland_session::RemoteSessionApi>> = None;
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
                wl_session = Some(Arc::clone(&session));
                (
                    Box::new(capture::wayland::WaylandCapturer::new(Arc::clone(&session))?),
                    Box::new(input::wayland::WaylandInputInjector::new(session)),
                )
            }
        };
    let screen_w = capturer.width();
    let screen_h = capturer.height();
    tracing::info!("Screen: {}x{}", screen_w, screen_h);

    // Encoder + codec selection
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

    // Shared stream control (codec switches, ABR, resize requests)
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

    // Input sanity reset (releases keys left pressed by old sessions)
    injector.release_stuck_inputs()?;

    // WebSocket transport
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
        args.allow_usb,
    )
    .await?;

    tracing::info!("Listening on http://{}", args.bind);

    // Input handler (dedicated blocking thread)
    spawn_input_handler(injector, input_rx, stream_control.clone());

    // Capture+encode loop (dedicated thread, zero-copy)
    let fps = args.fps;
    let loop_control = stream_control;

    tokio::task::spawn_blocking(move || {
        if let Err(e) = capture_encode_loop(
            capturer,
            wl_session,
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
        // Coalesce pointer motion. Each MouseMove is a D-Bus round-trip to the
        // compositor, so under a fast drag they can arrive faster than they
        // inject and back up, making the window trail the cursor and only catch
        // up once you stop. Draining the backlog each pass and dropping every
        // MouseMove that is immediately followed by another keeps only the
        // latest position per run, so the pointer tracks the real one. Buttons,
        // scroll and keys are never dropped and stay in order.
        loop {
            let Ok(first) = sync_rx.recv() else { break };
            let mut batch = vec![first];
            while let Ok(ev) = sync_rx.try_recv() {
                batch.push(ev);
            }
            for i in 0..batch.len() {
                if matches!(batch[i], ClientEvent::MouseMove { .. })
                    && batch
                        .get(i + 1)
                        .is_some_and(|n| matches!(n, ClientEvent::MouseMove { .. }))
                {
                    continue; // superseded by a newer move in this batch
                }
                if let ClientEvent::RequestKeyframe { head } = &batch[i] {
                    tracing::debug!("[keyframe] client requested keyframe (head {:?})", head);
                    control.request_keyframe(*head);
                    control.notify_activity();
                    continue;
                }
                if let Err(e) = injector.inject_event(&batch[i]) {
                    tracing::error!("Failed to inject input event: {}", e);
                }
                control.notify_activity();
            }
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
/// Build one encoder per head. The primary head (index 0) uses the ABR-driven
/// `primary_bitrate`; extra heads use a fixed per-resolution bitrate. All heads
/// share one codec (the actual codec is whatever the encoder resolved to).
fn build_heads(
    setup: &EncoderSetup,
    rects: &[MonitorRect],
    codec: &str,
    fps: u32,
    user_bitrate: Option<u32>,
    primary_bitrate: u32,
) -> anyhow::Result<(Vec<HeadStream>, String)> {
    let mut heads = Vec::with_capacity(rects.len());
    let mut actual_codec = codec.to_string();
    for (i, r) in rects.iter().enumerate() {
        let bitrate = if i == 0 {
            primary_bitrate
        } else {
            user_bitrate.unwrap_or_else(|| control::auto_bitrate(r.width, r.height, fps, codec))
        };
        let (enc, resolved) = setup.build(codec, r.width, r.height, fps, bitrate)?;
        actual_codec = resolved;
        heads.push(HeadStream::new(r.clone(), enc));
    }
    Ok((heads, actual_codec))
}

/// Recreate the per-head Wayland capturers after a Mutter session rebuild:
/// head 0 uses the first PipeWire node, the rest become extra capturers.
fn rebuild_wayland_capturers(
    session: &Arc<dyn wayland_session::RemoteSessionApi>,
    node_ids: &[u32],
) -> anyhow::Result<(Box<dyn ScreenCapturer>, Vec<Box<dyn ScreenCapturer>>)> {
    anyhow::ensure!(!node_ids.is_empty(), "session rebuild returned no heads");
    let head0: Box<dyn ScreenCapturer> =
        Box::new(capture::wayland::WaylandCapturer::for_node(Arc::clone(session), node_ids[0])?);
    let mut extras: Vec<Box<dyn ScreenCapturer>> = Vec::new();
    for &node in &node_ids[1..] {
        extras.push(Box::new(capture::wayland::WaylandCapturer::for_node(
            Arc::clone(session),
            node,
        )?));
    }
    Ok((head0, extras))
}

/// Client-space layout for Wayland heads: a horizontal row, each head at its
/// own captured size (Mutter picks each virtual monitor's resolution).
fn wayland_layout(head0: &dyn ScreenCapturer, extras: &[Box<dyn ScreenCapturer>]) -> Vec<MonitorRect> {
    let mut rects = Vec::with_capacity(1 + extras.len());
    let mut x = 0u32;
    for (i, (w, h)) in std::iter::once((head0.width(), head0.height()))
        .chain(extras.iter().map(|c| (c.width(), c.height())))
        .enumerate()
    {
        rects.push(MonitorRect { id: i as u32, x, y: 0, width: w, height: h });
        x += w;
    }
    rects
}

/// Per-head ENCODE rects (what each `HeadStream` slices out). X11 heads are
/// sub-rects of one shared framebuffer (`control.monitors`); each Wayland head
/// captures its own stream and encodes the whole frame at (0,0,w,h).
fn head_encode_rects(
    backend: BackendKind,
    head0: &dyn ScreenCapturer,
    extras: &[Box<dyn ScreenCapturer>],
    control: &StreamControl,
) -> Vec<MonitorRect> {
    if backend == BackendKind::Wayland {
        std::iter::once((head0.width(), head0.height()))
            .chain(extras.iter().map(|c| (c.width(), c.height())))
            .enumerate()
            .map(|(i, (w, h))| MonitorRect { id: i as u32, x: 0, y: 0, width: w, height: h })
            .collect()
    } else {
        control.monitors.lock().clone()
    }
}

fn capture_encode_loop(
    mut capturer: Box<dyn ScreenCapturer>,
    wl_session: Option<Arc<dyn wayland_session::RemoteSessionApi>>,
    backend: BackendKind,
    encoder: Box<dyn Encoder + Send>,
    mut codec: String,
    setup: EncoderSetup,
    user_bitrate: Option<u32>,
    frame_tx: transport::websocket::FrameSender,
    fps: u32,
    control: Arc<StreamControl>,
) -> anyhow::Result<()> {
    // Wayland extra heads (heads 1+) each capture their own Mutter virtual
    // monitor stream; head 0 stays on `capturer`. Empty on X11 and single-head
    // Wayland, where all heads share `capturer` (sub-rects of one framebuffer).
    let mut extra_capturers: Vec<Box<dyn ScreenCapturer>> = Vec::new();
    let active_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let idle_interval = Duration::from_secs_f64(1.0 / 10.0); // 10 fps when idle
    // After this much silence we drop to idle FPS.
    let idle_timeout = Duration::from_millis(500);

    let mut screen_w = capturer.width();
    let mut screen_h = capturer.height();
    let mut applied_bitrate = control.target_bitrate.load(Ordering::Acquire);

    // Each monitor head is encoded as its own stream. heads[0] is the primary
    // (the whole framebuffer when single-monitor, keeping the zero-copy path);
    // extra heads encode their sub-rect. head_rects tracks what heads reflect,
    // so a layout change triggers a rebuild.
    let mut head_rects: Vec<MonitorRect> = control.monitors.lock().clone();
    let mut heads: Vec<HeadStream> = vec![HeadStream::new(head_rects[0].clone(), encoder)];

    let mut frame_count: u64 = 0;
    let mut last_tick = Instant::now();
    let mut fps_timer = Instant::now();
    let mut fps_frame_count: u64 = 0;
    let mut last_activity = Instant::now();
    let mut last_recheck = Instant::now();
    let mut last_cursor: Option<(u16, u16, bool)> = None;

    loop {
        // Frame pacing
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

        // Reconfiguration checks (cheap; heavier X round-trips ~1/sec)
        let mut rebuild = false; // encoder must be rebuilt
        let mut resolution_changed = false;

        // A pending monitor plug/unplug opens the recheck immediately (the
        // transport wakes the loop via notify_activity) so it doesn't wait up to
        // a second, otherwise add/remove feels sluggish.
        let monitor_pending = control.monitor_request.lock().is_some();
        if last_recheck.elapsed() >= Duration::from_secs(1) || monitor_pending {
            last_recheck = Instant::now();

            // Client requested a display resize (--resize-to-client). Skipped
            // while more than one head is plugged in, since the monitor layout owns
            // the framebuffer size then (resize-to-client would shrink it back).
            if let Some((rw, rh)) = control.resize_request.lock().take() {
                let multi_monitor = control.monitors.lock().len() > 1;
                if (rw, rh) != (screen_w, screen_h) && !multi_monitor {
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

            // Client plugged/unplugged a virtual monitor. Re-lay-out the heads;
            // the framebuffer resize is picked up by the size_changed() check
            // below (which rebuilds the capturer + encoder). The new layout is
            // broadcast so every client opens/closes its extra windows.
            if let Some(desired) = control.monitor_request.lock().take() {
                let cur_len = control.monitors.lock().len();
                let desired = desired.clamp(1, monitor::MAX_MONITORS);
                if let Some(sess) = wl_session.as_ref() {
                    // Wayland: each head is a Mutter virtual monitor. Mutter only
                    // accepts Record* before Start(), so add/remove rebuilds the
                    // session with `desired` streams and reconnects the capturers.
                    if desired != cur_len {
                        tracing::info!("[monitor] wayland heads {} -> {}", cur_len, desired);
                        match sess
                            .set_head_count(desired)
                            .and_then(|node_ids| rebuild_wayland_capturers(sess, &node_ids))
                        {
                            Ok((cap0, extras)) => {
                                capturer = cap0;
                                extra_capturers = extras;
                                let rects = wayland_layout(capturer.as_ref(), &extra_capturers);
                                sess.set_head_layout(
                                    rects.iter().map(|r| (r.x, r.y, r.width, r.height)).collect(),
                                );
                                *control.monitors.lock() = rects;
                                let _ = frame_tx.send(protocol::encode_monitor_layout(
                                    &control.monitor_layout(),
                                ));
                            }
                            Err(e) => tracing::error!("[monitor] wayland head change failed: {:#}", e),
                        }
                    }
                } else if desired != cur_len {
                    let cur = control.monitors.lock().clone();
                    // Every head keeps the primary head's current size, so the
                    // layout doesn't drift across repeated add/remove cycles.
                    let base_w = cur[0].width.max(2) & !1;
                    let base_h = cur[0].height.max(2) & !1;
                    tracing::info!(
                        "[monitor] heads {} -> {} (base {}x{})",
                        cur.len(), desired, base_w, base_h,
                    );
                    match monitor::apply_layout(desired, base_w, base_h) {
                        Ok(rects) => {
                            // Provisional layout (responsive); the size_changed
                            // reconcile below corrects it to the real captured
                            // size, which is what the client maps input against.
                            *control.monitors.lock() = rects;
                            let _ = frame_tx.send(protocol::encode_monitor_layout(
                                &control.monitor_layout(),
                            ));
                        }
                        Err(e) => tracing::warn!("[monitor] layout change failed: {:#}", e),
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

                        // Reconcile the broadcast layout to the ACTUAL captured
                        // size (the driver may snap to a nearby mode). The client
                        // crops and maps input against these rects, so they must
                        // tile the real framebuffer exactly. X11 only: Wayland
                        // heads are independent streams whose layout is owned by
                        // wayland_layout() (a shared-framebuffer grid is wrong).
                        if backend != BackendKind::Wayland {
                            let count = control.monitors.lock().len().max(1);
                            let rects = monitor::layout_rects(count, screen_w, screen_h);
                            let mut mons = control.monitors.lock();
                            if *mons != rects {
                                *mons = rects;
                                drop(mons);
                                let _ = frame_tx.send(protocol::encode_monitor_layout(
                                    &control.monitor_layout(),
                                ));
                            }
                        }
                    }
                    Err(e) => tracing::error!("Failed to rebuild capturer after resize: {}", e),
                }
            }
        }

        // Monitor layout changed (head added/removed or a head resized) →
        // rebuild every head's encoder. Uses ENCODE rects (Wayland heads each
        // encode their own whole frame; X11 heads slice the shared framebuffer).
        let desired_rects = head_encode_rects(backend, capturer.as_ref(), &extra_capturers, &control);
        if desired_rects != head_rects {
            rebuild = true;
        }

        // Codec switch requested by capability arbitration. The live codec
        // only changes once the new encoders are actually built.
        let mut pending_codec: Option<String> = None;
        if let Some(new_codec) = control.desired_codec.lock().take() {
            if new_codec != codec {
                tracing::info!("[codec] switching {} -> {}", codec, new_codec);
                pending_codec = Some(new_codec);
                rebuild = true;
            }
        }

        // Adaptive bitrate target changed. ABR drives the primary head live;
        // extra heads use a fixed per-resolution bitrate (rebuilt on change).
        let target_bitrate = control.target_bitrate.load(Ordering::Acquire);
        if target_bitrate != applied_bitrate && !rebuild {
            if heads[0].encoder.set_bitrate(target_bitrate) {
                applied_bitrate = target_bitrate;
                control.session.lock().bitrate = target_bitrate;
            } else {
                // Encoder can't reconfigure live (OpenH264) — rebuild it.
                rebuild = true;
            }
        }

        if rebuild {
            let build_codec = pending_codec.unwrap_or_else(|| codec.clone());
            let primary = desired_rects[0].clone();
            // Ceiling and ABR target track the primary head's resolution.
            let ceiling = user_bitrate.unwrap_or_else(|| {
                control::auto_bitrate(primary.width, primary.height, fps, &build_codec)
            });
            control.bitrate_ceiling.store(ceiling, Ordering::Release);
            let primary_bitrate = if resolution_changed {
                ceiling
            } else {
                control.target_bitrate.load(Ordering::Acquire).min(ceiling)
            };
            control.target_bitrate.store(primary_bitrate, Ordering::Release);

            match build_heads(&setup, &desired_rects, &build_codec, fps, user_bitrate, primary_bitrate) {
                Ok((new_heads, actual_codec)) => {
                    heads = new_heads;
                    head_rects = desired_rects;
                    codec = actual_codec;
                    applied_bitrate = primary_bitrate;

                    let info = SessionInfo {
                        width: primary.width,
                        height: primary.height,
                        fps,
                        codec: codec.clone(),
                        bitrate: primary_bitrate,
                    };
                    *control.session.lock() = info.clone();
                    let _ = frame_tx.send(protocol::encode_session_info(&info));
                    control.request_keyframe_all();
                    last_activity = Instant::now();
                    tracing::info!(
                        "Encoders rebuilt: {} head(s), {} @ primary {}x{}, {} bps",
                        heads.len(), codec, primary.width, primary.height, primary_bitrate,
                    );
                }
                Err(e) => {
                    tracing::error!("Encoder rebuild failed ({}); keeping old encoders", e);
                }
            }
        }

        // The GOP is infinite — IDRs only happen on demand: the first frame,
        // a new client connecting, a recovery request (client decode drop or
        // server broadcast lag), or after an encoder rebuild. Periodic IDRs
        // are pure quality loss with single-frame VBV (they show up as a
        // flash of pixelation) and the recovery paths above cover every
        // resync case. The mask is per head, so one stream's resync doesn't
        // cost an IDR on every other head.
        let mut kf_mask = control.take_kf_mask();
        if frame_count == 0 {
            kf_mask = u64::MAX;
        }
        if kf_mask != 0 {
            tracing::debug!("[keyframe] forcing IDR (head mask {:#x})", kf_mask);
        }
        let force_kf = kf_mask == u64::MAX;
        let has_damage = capturer.has_new_frame();

        // Wayland extra heads capture independently; poll each once per loop so
        // activity on any head keeps the stream awake and gates its own encode.
        let extra_new: Vec<bool> = extra_capturers.iter_mut().map(|c| c.has_new_frame()).collect();
        let any_extra_new = extra_new.iter().any(|&b| b);

        // Damage counts as activity too (animations, video playback, etc.)
        if has_damage || any_extra_new {
            last_activity = Instant::now();
        }

        // Send cursor updates when the cursor actually moved — unless the
        // backend embeds the cursor in the frames (Wayland EMBEDDED mode).
        // Also resend on keyframes so new clients learn the position.
        if !capturer.embeds_cursor() {
            if let Some(ci) = capturer.cursor_info().ok() {
                let cur = (ci.x.max(0) as u16, ci.y.max(0) as u16, ci.visible);
                if kf_mask != 0 || last_cursor != Some(cur) {
                    last_cursor = Some(cur);
                    let _ = frame_tx.send(protocol::encode_cursor_update(cur.0, cur.1, cur.2));
                }
            }
        }

        // Skip capture+encode if nothing changed (saves CPU, GPU, and bandwidth).
        // Always capture on the frame after input (app may have just responded).
        if kf_mask == 0 && !has_damage && !any_extra_new && !input_arrived {
            frame_count += 1;
            fps_frame_count += 1;
            continue;
        }

        // What actually changed this frame (drained before frame_ref, which
        // holds the capturer borrow). When damage reports a bounding box,
        // heads it never touched skip encoding entirely — with many heads
        // that is most of them, most frames (activity on one monitor doesn't
        // burn encode time and bandwidth on the others). Without region info
        // (or on the post-input hedge frame) every head encodes.
        let damage_hint = if has_damage {
            capturer.take_damage_hint()
        } else if input_arrived {
            capture::DamageHint::Unknown
        } else {
            // Here only because of a keyframe request: nothing on screen
            // changed, so heads that weren't asked for an IDR stay silent.
            capture::DamageHint::Bbox { x: 0, y: 0, width: 0, height: 0 }
        };

        // Helper: encode one head's packet and send it as VideoFrame (head 0)
        // or a tagged MonitorFrame (heads 1+).
        let send_head =
            |head: &mut HeadStream, data: &[u8], w: u32, h: u32, stride: u32, head_kf: bool| {
                match head.encode(data, w, h, stride, head_kf) {
                    Ok(packet) => {
                        let (rw, rh) = (head.rect.width as u16, head.rect.height as u16);
                        let wire = if head.rect.id == 0 {
                            protocol::encode_video_frame(packet.keyframe, packet.pts, rw, rh, &packet.data)
                        } else {
                            protocol::encode_monitor_frame(
                                head.rect.id as u8, packet.keyframe, packet.pts, rw, rh, &packet.data,
                            )
                        };
                        let _ = frame_tx.send(wire);
                        true
                    }
                    Err(e) => {
                        tracing::warn!("Encode failed on head {} ({}); skipping", head.rect.id, e);
                        false
                    }
                }
            };

        if extra_capturers.is_empty() {
            // Shared-framebuffer path (X11 any head count; Wayland single head).
            // Zero-copy: borrow the SHM/PipeWire buffer directly. Capture can
            // fail transiently around resizes — recheck and keep the loop alive.
            let frame = match capturer.frame_ref() {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("Capture failed ({}); rechecking display geometry", e);
                    last_recheck = Instant::now() - Duration::from_secs(1);
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            for head in heads.iter_mut() {
                let (id, rx, ry, rw, rh) = {
                    let r = &head.rect;
                    (r.id, r.x, r.y, r.width, r.height)
                };
                let head_kf = force_kf || (kf_mask >> id.min(63)) & 1 != 0;
                if !head_kf && !damage_hint.intersects(rx, ry, rw, rh) {
                    continue;
                }
                if !send_head(head, frame.data, frame.width, frame.height, frame.stride, head_kf) {
                    control.request_keyframe(Some(id as u8));
                }
            }
        } else {
            // Wayland multi-head: each head captures its own Mutter virtual
            // monitor stream and encodes the whole frame.
            for (i, head) in heads.iter_mut().enumerate() {
                let head_kf = force_kf || (kf_mask >> (i as u32).min(63)) & 1 != 0;
                let head_new = if i == 0 { has_damage } else { extra_new.get(i - 1).copied().unwrap_or(false) };
                if !head_kf && !head_new {
                    continue;
                }
                let cap: &mut Box<dyn ScreenCapturer> =
                    if i == 0 { &mut capturer } else { &mut extra_capturers[i - 1] };
                let frame = match cap.frame_ref() {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("Capture failed on head {} ({}); skipping", i, e);
                        continue;
                    }
                };
                if !send_head(head, frame.data, frame.width, frame.height, frame.stride, head_kf) {
                    control.request_keyframe(Some(i as u8));
                }
            }
        }

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
