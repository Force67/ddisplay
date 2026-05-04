// Force Windows to use the NVIDIA discrete GPU (Optimus laptops)
#[allow(non_upper_case_globals)]
#[no_mangle]
pub static NvOptimusEnablement: u32 = 1;
// Force Windows to use the high-performance AMD GPU
#[allow(non_upper_case_globals)]
#[no_mangle]
pub static AmdPowerXpressRequestHighPerformance: u32 = 1;

use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

mod protocol;
mod transport;
mod decoder;
mod renderer;
mod input;
mod clipboard;
mod overlay;
mod files;

use overlay::{OverlayAction, OverlayState};

use protocol::ServerMessage;
use transport::TransportEvent;

/// Parsed session info JSON from server.
#[derive(serde::Deserialize)]
struct SessionInfo {
    #[serde(default)]
    codec: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    fps: u32,
}

#[derive(Parser)]
#[command(name = "ddisplay-client", about = "Native remote display client")]
struct Args {
    /// Server address (e.g., 192.168.1.100:9550)
    #[arg(short, long)]
    server: String,

    /// Window title
    #[arg(long, default_value = "ddisplay")]
    title: String,

    /// Force a codec ("av1" or "h264"), ignoring the server's SessionInfo.
    /// Useful for diagnosing why AV1 is not working.
    #[arg(long)]
    force_codec: Option<String>,

    /// Local folder to share with the server on connect.
    /// All files in this directory are uploaded to the server's shared folder.
    #[arg(long)]
    share_dir: Option<std::path::PathBuf>,
}

/// Payload sent through the decode channel: raw frame bytes + keyframe flag.
struct DecodeJob {
    data: Vec<u8>,
    keyframe: bool,
}

/// Application state.
struct App {
    args: Args,
    window: Option<Arc<Window>>,
    renderer: Option<renderer::Renderer>,
    /// Sends raw frame bytes to the background decode thread.
    decode_tx: Option<std::sync::mpsc::SyncSender<DecodeJob>>,
    /// Latest frame decoded by the background thread; render loop takes it each tick.
    frame_slot: Arc<std::sync::Mutex<Option<decoder::DecodedFrame>>>,
    input_state: Option<input::InputState>,
    transport_rx: Option<tokio::sync::mpsc::UnboundedReceiver<TransportEvent>>,
    rt: tokio::runtime::Handle,
    codec: String,
    session_fps: u32,
    /// Frame counter for log throttling (total VideoFrame messages received).
    frame_count: u64,
    /// Tracks last clipboard text set from server (echo prevention for poll thread).
    clipboard_last_set: Arc<std::sync::Mutex<Option<String>>>,
    /// False when window is minimized or fully occluded — skip decode and render.
    window_visible: bool,
    /// Set to true by the decode thread when a new frame is stored in frame_slot.
    /// Cleared by about_to_wait after requesting a redraw.
    frame_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the decode thread when the AV1 decoder self-reset and needs a keyframe.
    needs_keyframe: Arc<std::sync::atomic::AtomicBool>,
    /// Egui overlay state.
    overlay: Option<OverlayState>,
    /// File transfer state (HTTP, talks to server's /files/* endpoints).
    files_state: Option<files::FileTransferState>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Continuously poll so about_to_wait fires every iteration.
        // GPU stays idle because we only request_redraw when frame_ready is set.
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);

        if self.window.is_some() {
            return;
        }

        eprintln!("[init] Creating window...");

        let attrs = WindowAttributes::default()
            .with_title(&self.args.title)
            .with_inner_size(winit::dpi::LogicalSize::new(1280, 720));

        let window = Arc::new(event_loop.create_window(attrs).expect("Failed to create window"));

        eprintln!("[init] Initializing GPU renderer...");

        // Use pollster for the one-time async wgpu init (tokio block_on can deadlock with winit)
        let renderer = match pollster::block_on(renderer::Renderer::new(window.clone())) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[FATAL] Failed to create renderer: {}", e);
                event_loop.exit();
                return;
            }
        };

        eprintln!("[init] Initializing decoder...");

        // Determine startup codec: honour --force-codec if provided.
        let startup_codec = self.args.force_codec
            .as_deref()
            .unwrap_or("h264")
            .to_lowercase();
        if let Some(ref fc) = self.args.force_codec {
            eprintln!("[init] --force-codec={} (server SessionInfo codec will be IGNORED)", fc);
        }

        self.start_decode_thread(&startup_codec);
        self.codec = startup_codec;

        eprintln!("[init] Connecting to ws://{}...", self.args.server);

        // Start transport on the tokio runtime
        let ws_url = format!("ws://{}/ws", self.args.server);
        let (sender, rx) = self.rt.block_on(async { transport::spawn(ws_url) });

        let clipboard_last_set = clipboard::spawn_monitor(sender.clone());
        self.clipboard_last_set = clipboard_last_set;

        let input_state = input::InputState::new(sender);

        self.window = Some(window.clone());
        self.renderer = Some(renderer);
        self.input_state = Some(input_state);
        self.transport_rx = Some(rx);
        self.overlay = Some(OverlayState::new(&window));
        self.files_state = Some(files::FileTransferState::new(
            &self.args.server,
            self.rt.clone(),
        ));

        eprintln!("[init] Ready.");
        tracing::info!("Window created, connecting to server...");
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Pass ALL events to egui before our own handling
        if let (Some(overlay), Some(window)) = (&mut self.overlay, &self.window) {
            let resp = overlay.winit_state.on_window_event(window, &event);
            if resp.consumed {
                return;
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                if let Some(input) = &self.input_state {
                    input.release_all();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                if let Some(input) = &mut self.input_state {
                    input.set_window_size(size.width, size.height);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(input) = &mut self.input_state {
                    // Only forward mouse to server when overlay is hidden
                    if self.overlay.as_ref().map_or(true, |o| !o.visible) {
                        input.on_cursor_moved(position.x, position.y);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(input) = &mut self.input_state {
                    if self.overlay.as_ref().map_or(true, |o| !o.visible) {
                        input.on_mouse_button(button, state);
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(input) = &mut self.input_state {
                    if self.overlay.as_ref().map_or(true, |o| !o.visible) {
                        input.on_scroll(delta);
                    }
                }
            }
            WindowEvent::KeyboardInput { event: ref key_event, .. } => {
                // F2 toggles overlay (not forwarded to server)
                if key_event.state == winit::event::ElementState::Pressed {
                    if let winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::F2) = key_event.physical_key {
                        if let Some(overlay) = &mut self.overlay {
                            overlay.visible = !overlay.visible;
                            return;
                        }
                    }
                }
                if self.overlay.as_ref().map_or(true, |o| !o.visible) {
                    if let Some(input) = &mut self.input_state {
                        input.on_key(key_event.physical_key, key_event.state);
                    }
                }
            }
            WindowEvent::Focused(false) => {
                if let Some(input) = &self.input_state {
                    input.release_all();
                }
            }
            WindowEvent::Occluded(occluded) => {
                self.window_visible = !occluded;
            }
            WindowEvent::RedrawRequested => {
                // Drain async file-transfer results.
                if let Some(files) = &mut self.files_state {
                    files.poll();
                }

                if !self.window_visible {
                    return;
                }

                // Upload latest frame decoded by the background thread.
                if let Some(frame) = self.frame_slot.lock().unwrap().take() {
                    if let Some(input) = &mut self.input_state {
                        input.set_remote_size(frame.width, frame.height);
                    }
                    if let Some(renderer) = &mut self.renderer {
                        renderer.upload_frame(&frame.rgba, frame.width, frame.height);
                    }
                }

                // Run egui UI — only produces output when overlay is visible.
                // When hidden, run_ui() still drains the winit event queue.
                let (egui_output, action) = if let (Some(overlay), Some(window), Some(files)) =
                    (&mut self.overlay, &self.window, &mut self.files_state)
                {
                    let server = &self.args.server;
                    let codec = &self.codec;
                    let fps = self.session_fps;
                    let (egui_data, act) = overlay.run_ui(server, codec, fps, window, Some(files));
                    if let Some(renderer) = &mut self.renderer {
                        renderer.set_display_mode(overlay.display_mode);
                    }
                    (egui_data, act)
                } else if let (Some(overlay), Some(window)) = (&mut self.overlay, &self.window) {
                    let server = &self.args.server;
                    let codec = &self.codec;
                    let fps = self.session_fps;
                    let (egui_data, act) = overlay.run_ui(server, codec, fps, window, None);
                    if let Some(renderer) = &mut self.renderer {
                        renderer.set_display_mode(overlay.display_mode);
                    }
                    (egui_data, act)
                } else {
                    (None, OverlayAction::None)
                };

                // Handle file-transfer actions triggered from the overlay UI.
                match action {
                    OverlayAction::RefreshFiles => {
                        if let Some(files) = &mut self.files_state {
                            files.refresh();
                        }
                    }
                    OverlayAction::Download(name) => {
                        if let Some(files) = &mut self.files_state {
                            files.download(name);
                        }
                    }
                    OverlayAction::RequestUpload => {
                        if let Some(paths) = rfd::FileDialog::new().pick_files() {
                            if let Some(files) = &mut self.files_state {
                                for p in paths {
                                    files.upload(p);
                                }
                            }
                        }
                    }
                    OverlayAction::None => {}
                }

                if let Some(renderer) = &mut self.renderer {
                    if let Err(e) = renderer.render(egui_output) {
                        tracing::warn!("Render error: {}", e);
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Always drain transport events — this must not depend on rendering.
        self.process_transport_events();

        // If the decoder reset itself, ask the server for a fresh keyframe immediately.
        if self.needs_keyframe.swap(false, std::sync::atomic::Ordering::Relaxed) {
            if let Some(input) = &self.input_state {
                input.send_keyframe_request();
            }
        }

        if !self.window_visible {
            return;
        }
        let overlay_open = self.overlay.as_ref().map_or(false, |o| o.visible);
        let has_frame = self.frame_ready.load(std::sync::atomic::Ordering::Relaxed);
        if has_frame || overlay_open {
            self.frame_ready.store(false, std::sync::atomic::Ordering::Relaxed);
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, _event: DeviceEvent) {
        // Could use raw mouse motion here for pointer-lock mode later
    }
}

impl App {
    fn process_transport_events(&mut self) {
        // Collect events first to avoid borrow conflicts
        let events: Vec<_> = {
            let rx = match &mut self.transport_rx {
                Some(rx) => rx,
                None => return,
            };
            let mut buf = Vec::new();
            while let Ok(event) = rx.try_recv() {
                buf.push(event);
            }
            buf
        };

        for event in events {
            match event {
                TransportEvent::Connected => {
                    tracing::info!("Connected to server");
                    if let Some(input) = &self.input_state {
                        input.send_client_ready();
                    }
                    // Auto-upload --share-dir contents to the server's shared folder.
                    if let (Some(dir), Some(files)) =
                        (&self.args.share_dir, &mut self.files_state)
                    {
                        files.upload_dir(dir.clone());
                    }
                }
                TransportEvent::Disconnected => {
                    tracing::warn!("Disconnected from server");
                }
                TransportEvent::Data(data) => {
                    self.handle_server_message(&data);
                }
            }
        }
    }

    fn handle_server_message(&mut self, data: &[u8]) {
        let msg = match protocol::parse_server_message(data) {
            Some(m) => m,
            None => return,
        };

        match msg {
            ServerMessage::VideoFrame(frame) => {
                self.frame_count += 1;

                // Log first 5 frames + occasional status; suppress per-frame spam after priming.
                if self.frame_count <= 5 || self.frame_count % 300 == 0 {
                    eprintln!("[frame #{}] kf={} pts={} data_bytes={} dim={}x{}  codec={}",
                        self.frame_count, frame.keyframe, frame.pts,
                        frame.data.len(), frame.width, frame.height,
                        self.codec);
                }

                // Push to the background decode thread (non-blocking).
                // Skip when the window is hidden — no point decoding frames nobody sees.
                if self.window_visible {
                    if let Some(tx) = &self.decode_tx {
                        let job = DecodeJob {
                            data: frame.data.to_vec(),
                            keyframe: frame.keyframe,
                        };
                        if tx.try_send(job).is_err() {
                            // Decode thread is behind — drop this frame.
                            if self.frame_count <= 10 {
                                eprintln!("[frame #{}] decode channel full, dropped", self.frame_count);
                            }
                        }
                    }
                }
            }
            ServerMessage::SessionInfo(json_bytes) => {
                let raw = String::from_utf8_lossy(json_bytes);
                eprintln!("[session] raw JSON: {}", raw);

                if let Ok(info) = serde_json::from_slice::<SessionInfo>(json_bytes) {
                    eprintln!("[session] parsed: codec={} {}x{} @ {} fps",
                        info.codec, info.width, info.height, info.fps);

                    self.session_fps = info.fps;

                    // --force-codec wins over server-sent codec.
                    if self.args.force_codec.is_some() {
                        eprintln!("[session] --force-codec active — ignoring server codec '{}'", info.codec);
                        return;
                    }

                    if !info.codec.is_empty() && info.codec != self.codec {
                        eprintln!("[session] switching decoder: {} → {}", self.codec, info.codec);
                        self.start_decode_thread(&info.codec);
                        self.codec = info.codec.clone();
                    } else {
                        eprintln!("[session] codec unchanged ({})", self.codec);
                    }
                } else {
                    eprintln!("[session] WARNING: failed to parse SessionInfo JSON");
                }
            }
            ServerMessage::CursorUpdate(_cursor) => {
                // TODO: render remote cursor overlay
            }
            ServerMessage::ClipboardData(text) => {
                clipboard::set_clipboard(&text, &self.clipboard_last_set);
            }
            ServerMessage::Unknown(t) => {
                eprintln!("[proto] unknown message type 0x{:02x}", t);
            }
        }
    }

    /// Spawn a background decode thread for `codec`, replacing any previous one.
    /// The old thread exits automatically when its channel sender is dropped.
    fn start_decode_thread(&mut self, codec: &str) {
        // Dropping the old sender closes the channel → old thread exits cleanly.
        self.decode_tx = None;

        let (tx, rx) = std::sync::mpsc::sync_channel::<DecodeJob>(16);
        self.decode_tx = Some(tx);

        let slot = self.frame_slot.clone();
        let frame_ready = self.frame_ready.clone();
        let needs_keyframe = self.needs_keyframe.clone();
        let codec_str = codec.to_string();

        std::thread::Builder::new()
            .name(format!("decode-{}", codec_str))
            .spawn(move || {
                eprintln!("[decode] thread started, codec={}", codec_str);
                let mut dec = match decoder::VideoDecoder::for_codec(&codec_str) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("[decode] FATAL: {} decoder init failed: {}", codec_str, e);
                        return;
                    }
                };
                // After a decoder reset we must skip non-keyframes — a fresh
                // dav1d context cannot parse inter-frames without a prior
                // sequence header + keyframe.
                let mut awaiting_keyframe = false;

                while let Ok(job) = rx.recv() {
                    if awaiting_keyframe && !job.keyframe {
                        continue; // discard until a keyframe arrives
                    }
                    if awaiting_keyframe && job.keyframe {
                        eprintln!("[decode] got keyframe after reset — resuming");
                        awaiting_keyframe = false;
                    }

                    match dec.decode(&job.data) {
                        Ok(Some(frame)) => {
                            *slot.lock().unwrap() = Some(frame);
                            frame_ready.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("[decode] error: {}", e),
                    }
                    if dec.take_needs_keyframe() {
                        awaiting_keyframe = true;
                        needs_keyframe.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                eprintln!("[decode] thread exiting");
            })
            .expect("failed to spawn decode thread");
    }
}

fn main() {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ddisplay=info,wgpu=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    eprintln!("ddisplay-client v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("Server: {}", args.server);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    let event_loop = EventLoop::new().expect("Failed to create event loop");

    let mut app = App {
        args,
        window: None,
        renderer: None,
        decode_tx: None,
        frame_slot: Arc::new(std::sync::Mutex::new(None)),
        input_state: None,
        transport_rx: None,
        rt: rt.handle().clone(),
        codec: "h264".to_string(),
        session_fps: 60,
        frame_count: 0,
        clipboard_last_set: Arc::new(std::sync::Mutex::new(None)),
        window_visible: true,
        frame_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        needs_keyframe: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        overlay: None,
        files_state: None,
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[FATAL] Event loop error: {}", e);
        std::process::exit(1);
    }
}
