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
}

/// Application state.
struct App {
    args: Args,
    window: Option<Arc<Window>>,
    renderer: Option<renderer::Renderer>,
    /// Sends raw frame bytes to the background decode thread.
    decode_tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    /// Latest frame decoded by the background thread; render loop takes it each tick.
    frame_slot: Arc<std::sync::Mutex<Option<decoder::DecodedFrame>>>,
    input_state: Option<input::InputState>,
    transport_rx: Option<tokio::sync::mpsc::UnboundedReceiver<TransportEvent>>,
    rt: tokio::runtime::Handle,
    codec: String,
    /// Frame counter for log throttling (total VideoFrame messages received).
    frame_count: u64,
    /// Tracks last clipboard text set from server (echo prevention for poll thread).
    clipboard_last_set: Arc<std::sync::Mutex<Option<String>>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
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

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.input_state = Some(input_state);
        self.transport_rx = Some(rx);

        eprintln!("[init] Ready.");
        tracing::info!("Window created, connecting to server...");
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
                    input.on_cursor_moved(position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(input) = &mut self.input_state {
                    input.on_mouse_button(button, state);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(input) = &mut self.input_state {
                    input.on_scroll(delta);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(input) = &mut self.input_state {
                    input.on_key(event.physical_key, event.state);
                }
            }
            WindowEvent::Focused(false) => {
                if let Some(input) = &self.input_state {
                    input.release_all();
                }
            }
            WindowEvent::RedrawRequested => {
                self.process_transport_events();

                // Upload latest frame decoded by the background thread.
                if let Some(frame) = self.frame_slot.lock().unwrap().take() {
                    if let Some(input) = &mut self.input_state {
                        input.set_remote_size(frame.width, frame.height);
                    }
                    if let Some(renderer) = &mut self.renderer {
                        renderer.upload_frame(&frame.rgba, frame.width, frame.height);
                    }
                }

                if let Some(renderer) = &self.renderer {
                    if let Err(e) = renderer.render() {
                        tracing::warn!("Render error: {}", e);
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Request continuous redraws for low-latency frame display
        if let Some(window) = &self.window {
            window.request_redraw();
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

                // Push raw bytes to the background decode thread (non-blocking).
                if let Some(tx) = &self.decode_tx {
                    if tx.try_send(frame.data.to_vec()).is_err() {
                        // Decode thread is behind — drop this frame.
                        if self.frame_count <= 10 {
                            eprintln!("[frame #{}] decode channel full, dropped", self.frame_count);
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

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        self.decode_tx = Some(tx);

        let slot = self.frame_slot.clone();
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
                while let Ok(data) = rx.recv() {
                    match dec.decode(&data) {
                        Ok(Some(frame)) => {
                            *slot.lock().unwrap() = Some(frame);
                        }
                        Ok(None) => {} // decoder buffering (EAGAIN)
                        Err(e) => eprintln!("[decode] error: {}", e),
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
        frame_count: 0,
        clipboard_last_set: Arc::new(std::sync::Mutex::new(None)),
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[FATAL] Event loop error: {}", e);
        std::process::exit(1);
    }
}
