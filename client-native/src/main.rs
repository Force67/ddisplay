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

#[derive(Parser)]
#[command(name = "ddisplay-client", about = "Native remote display client")]
struct Args {
    /// Server address (e.g., 192.168.1.100:9550)
    #[arg(short, long)]
    server: String,

    /// Window title
    #[arg(long, default_value = "ddisplay")]
    title: String,
}

/// Application state.
struct App {
    args: Args,
    window: Option<Arc<Window>>,
    renderer: Option<renderer::Renderer>,
    decoder: Option<decoder::H264Decoder>,
    input_state: Option<input::InputState>,
    transport_rx: Option<tokio::sync::mpsc::UnboundedReceiver<TransportEvent>>,
    rt: tokio::runtime::Handle,
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

        eprintln!("[init] Initializing H.264 decoder...");

        let h264_decoder = match decoder::H264Decoder::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[FATAL] Failed to create decoder: {}", e);
                event_loop.exit();
                return;
            }
        };

        eprintln!("[init] Connecting to ws://{}...", self.args.server);

        // Start transport on the tokio runtime
        let ws_url = format!("ws://{}/ws", self.args.server);
        let (sender, rx) = self.rt.block_on(async { transport::spawn(ws_url) });

        let input_state = input::InputState::new(sender);

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.decoder = Some(h264_decoder);
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
                        tracing::warn!("Decode error: {}", e);
                    }
                }
            }
            ServerMessage::CursorUpdate(_cursor) => {
                // TODO: render remote cursor overlay
            }
            ServerMessage::Unknown(_) => {}
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
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[FATAL] Event loop error: {}", e);
        std::process::exit(1);
    }
}
