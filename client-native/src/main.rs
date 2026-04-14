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
    decoder: Option<decoder::VideoDecoder>,
    input_state: Option<input::InputState>,
    transport_rx: Option<tokio::sync::mpsc::UnboundedReceiver<TransportEvent>>,
    rt: tokio::runtime::Handle,
    codec: String,
    /// Frame counter for log throttling (total VideoFrame messages received).
    frame_count: u64,
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

        // Start with the chosen codec; switch to AV1 when server sends session info
        let video_decoder = match decoder::VideoDecoder::for_codec(&startup_codec) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[FATAL] Failed to create {} decoder: {}", startup_codec, e);
                event_loop.exit();
                return;
            }
        };
        self.codec = startup_codec;

        eprintln!("[init] Connecting to ws://{}...", self.args.server);

        // Start transport on the tokio runtime
        let ws_url = format!("ws://{}/ws", self.args.server);
        let (sender, rx) = self.rt.block_on(async { transport::spawn(ws_url) });

        let input_state = input::InputState::new(sender);

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.decoder = Some(video_decoder);
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

                // Log every frame for AV1 diagnosis; throttle to every 30 after the first 10.
                let log_this = self.frame_count <= 10 || self.frame_count % 30 == 0;
                if log_this {
                    eprintln!("[frame #{}] kf={} pts={} data_bytes={} dim={}x{}  codec={}",
                        self.frame_count, frame.keyframe, frame.pts,
                        frame.data.len(), frame.width, frame.height,
                        self.codec);
                }

                let decoder = match &mut self.decoder {
                    Some(d) => d,
                    None => return,
                };

                match decoder.decode(frame.data) {
                    Ok(Some(decoded)) => {
                        if let Some(input) = &mut self.input_state {
                            input.set_remote_size(decoded.width, decoded.height);
                        }
                        if let Some(renderer) = &mut self.renderer {
                            renderer.upload_frame(&decoded.rgba, decoded.width, decoded.height);
                        }
                    }
                    Ok(None) => {} // decoder buffering
                    Err(e) => {
                        eprintln!("[frame #{}] decode error (codec={}): {}",
                            self.frame_count, self.codec, e);
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
                        match decoder::VideoDecoder::for_codec(&info.codec) {
                            Ok(dec) => {
                                self.decoder = Some(dec);
                                self.codec = info.codec.clone();
                                eprintln!("[session] decoder ready: {}", self.codec);
                            }
                            Err(e) => {
                                eprintln!("[session] FAILED to create {} decoder: {}", info.codec, e);
                            }
                        }
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
            ServerMessage::Unknown(t) => {
                eprintln!("[proto] unknown message type 0x{:02x}", t);
            }
        }
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
        decoder: None,
        input_state: None,
        transport_rx: None,
        rt: rt.handle().clone(),
        codec: "h264".to_string(),
        frame_count: 0,
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[FATAL] Event loop error: {}", e);
        std::process::exit(1);
    }
}
