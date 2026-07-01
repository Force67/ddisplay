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
mod decode_pipeline;
#[cfg(windows)]
mod decoder_mf;
mod renderer;
mod input;
mod clipboard;
mod overlay;
mod files;

use overlay::{OverlayAction, OverlayState};

use decode_pipeline::DecodePipeline;
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

/// One monitor (head) and its pixel rect inside the captured framebuffer,
/// parsed from MSG_MONITOR_LAYOUT.
#[derive(serde::Deserialize, Clone, Default, PartialEq)]
struct MonitorInfo {
    #[serde(default)]
    id: u32,
    #[serde(default)]
    x: u32,
    #[serde(default)]
    y: u32,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(serde::Deserialize)]
struct MonitorLayoutMsg {
    #[serde(default)]
    monitors: Vec<MonitorInfo>,
}

/// A secondary-monitor window: its own OS window, GPU surface, decode pipeline
/// and input map. Renders one extra head's independent stream.
struct MonitorWindow {
    /// The head's id, matched against incoming MonitorFrames.
    id: u8,
    window: Arc<Window>,
    renderer: renderer::Renderer,
    decode: DecodePipeline,
    input: input::InputState,
}

/// Application state.
struct App {
    args: Args,
    window: Option<Arc<Window>>,
    renderer: Option<renderer::Renderer>,
    /// Primary head's decode pipeline (fed by VideoFrame). None until resumed().
    decode: Option<DecodePipeline>,
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
    /// Egui overlay state.
    overlay: Option<OverlayState>,
    /// File transfer state (HTTP, talks to server's /files/* endpoints).
    files_state: Option<files::FileTransferState>,
    /// Frames received since the last stats report.
    stats_received: u32,
    /// Frames dropped (decode backlog) since the last stats report.
    stats_dropped: u32,
    /// When the last stats report + RTT probe was sent.
    last_stats_sent: std::time::Instant,
    /// Monotonic base for ping timestamps.
    app_start: std::time::Instant,
    /// Last measured round-trip time in ms (0 until first pong).
    last_rtt_ms: f32,
    /// Hardware (GPU) decode support found at startup: (h264, av1).
    hw_decode: (bool, bool),
    /// Rolling per-second metric history for the F3 stats HUD.
    stats: overlay::StatsHistory,
    /// Wire bytes received since the last stats tick (all message types).
    stats_bytes: u64,
    /// Current monitor layout from the server (one entry per head). Empty until
    /// the first MSG_MONITOR_LAYOUT; treated as a single full-frame monitor then.
    monitors: Vec<MonitorInfo>,
    /// Set when `monitors` changed so `about_to_wait` reconciles windows (open
    /// or close the secondary head), since winit window creation needs the event loop.
    layout_dirty: bool,
    /// Extra-monitor window (second head). Phase 1 supports one beyond primary.
    secondary: Option<MonitorWindow>,
    /// Clone of the transport sender, used to build a second window's input map.
    transport_sender: Option<transport::TransportSender>,
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

        // winit's default drag-and-drop support calls OleInitialize, which requires
        // the thread to be in an STA COM apartment. The Media Foundation hardware
        // decode probe (decoder_mf::probe) already put the main thread into an MTA
        // apartment via CoInitializeEx, so OleInitialize would fail with
        // RPC_E_CHANGED_MODE and panic window creation. We don't use OS file-drop
        // onto the window (file sharing goes through the shared dir), so disable it.
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_drag_and_drop(false)
        };

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

        self.decode = Some(DecodePipeline::new(&startup_codec, self.hw_decode));
        self.codec = startup_codec;

        eprintln!("[init] Connecting to ws://{}...", self.args.server);

        // Start transport on the tokio runtime
        let ws_url = format!("ws://{}/ws", self.args.server);
        let (sender, rx) = self.rt.block_on(async { transport::spawn(ws_url) });

        let clipboard_last_set = clipboard::spawn_monitor(sender.clone());
        self.clipboard_last_set = clipboard_last_set;

        let input_state = input::InputState::new(sender.clone());
        self.transport_sender = Some(sender);

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

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Route events for the secondary-monitor window to its own handler.
        if self.secondary.as_ref().map_or(false, |s| s.window.id() == id) {
            self.handle_secondary_event(event);
            return;
        }

        // Only let egui consume events while the overlay is actually visible.
        // Otherwise stale egui focus/capture state can swallow remote input.
        if let (Some(overlay), Some(window)) = (&mut self.overlay, &self.window) {
            if overlay.visible {
                let resp = overlay.winit_state.on_window_event(window, &event);
                if resp.consumed {
                    return;
                }
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
                    // F3 toggles the stats HUD (latency/fps/bandwidth histograms)
                    if let winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::F3) = key_event.physical_key {
                        if let Some(overlay) = &mut self.overlay {
                            overlay.stats_visible = !overlay.stats_visible;
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

                // (Decoded frames are uploaded to every monitor window in
                // about_to_wait, so they stay in lockstep.)

                // Mirror the current monitor count so the panel can show and
                // gate the add/remove-monitor buttons.
                if let Some(overlay) = &mut self.overlay {
                    overlay.monitor_count = self.monitors.len().max(1);
                }

                // Run egui UI — only produces output when overlay is visible.
                // When hidden, run_ui() still drains the winit event queue.
                let (egui_output, action) = if let (Some(overlay), Some(window), Some(files)) =
                    (&mut self.overlay, &self.window, &mut self.files_state)
                {
                    let server = &self.args.server;
                    let codec = &self.codec;
                    let fps = self.session_fps;
                    let rtt = self.last_rtt_ms;
                    let (egui_data, act) =
                        overlay.run_ui(server, codec, fps, rtt, &self.stats, window, Some(files));
                    if let Some(renderer) = &mut self.renderer {
                        renderer.set_display_mode(overlay.display_mode);
                    }
                    (egui_data, act)
                } else if let (Some(overlay), Some(window)) = (&mut self.overlay, &self.window) {
                    let server = &self.args.server;
                    let codec = &self.codec;
                    let fps = self.session_fps;
                    let rtt = self.last_rtt_ms;
                    let (egui_data, act) =
                        overlay.run_ui(server, codec, fps, rtt, &self.stats, window, None);
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
                    OverlayAction::AddMonitor => {
                        if let Some(input) = &self.input_state {
                            input.send_raw(protocol::encode_request_add_monitor());
                        }
                    }
                    OverlayAction::RemoveMonitor => {
                        if let Some(input) = &self.input_state {
                            input.send_raw(protocol::encode_request_remove_monitor());
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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Always drain transport events — this must not depend on rendering.
        self.process_transport_events();

        // Open/close the secondary window to match the latest layout (needs the
        // event loop, so it can't run inside the message handler).
        if self.layout_dirty {
            self.reconcile_windows(event_loop);
        }

        // If any head's decoder reset itself, ask for a fresh keyframe (it
        // re-IDRs every head, which resyncs whichever one reset).
        let primary_reset = self.decode.as_ref().map_or(false, |d| d.take_needs_keyframe());
        let secondary_reset = self.secondary.as_ref().map_or(false, |s| s.decode.take_needs_keyframe());
        if primary_reset || secondary_reset {
            if let Some(input) = &self.input_state {
                input.send_keyframe_request();
            }
        }

        // Once per second: RTT probe + stats report (feeds the server's
        // adaptive bitrate controller).
        let stats_elapsed = self.last_stats_sent.elapsed();
        if stats_elapsed >= std::time::Duration::from_secs(1) {
            self.last_stats_sent = std::time::Instant::now();
            let decode_ms = self.decode.as_ref().map_or(0.0, |d| d.decode_ms());
            if let Some(input) = &self.input_state {
                let now_ms = self.app_start.elapsed().as_millis() as u64;
                input.send_raw(protocol::encode_ping(now_ms));
                input.send_raw(protocol::encode_client_stats(
                    self.stats_received,
                    self.stats_dropped,
                    decode_ms,
                    self.last_rtt_ms,
                ));
            }
            // Feed the F3 stats HUD one sample per tick.
            let secs = stats_elapsed.as_secs_f32();
            self.stats.push(
                self.last_rtt_ms,
                self.stats_received as f32 / secs,
                (self.stats_bytes * 8) as f32 / 1_000_000.0 / secs,
                decode_ms,
            );
            self.stats_bytes = 0;
            self.stats_received = 0;
            self.stats_dropped = 0;
        }

        let overlay_open = self
            .overlay
            .as_ref()
            .map_or(false, |o| o.visible || o.stats_visible);

        // Primary head: upload its newest decoded frame, then redraw.
        let primary_frame = self.decode.as_ref().map_or(false, |d| d.take_frame_ready());
        if primary_frame {
            if let Some(frame) = self.decode.as_ref().and_then(|d| d.take_frame()) {
                // With 0 or 1 head the primary IS the whole framebuffer, so track
                // the live frame size (this also follows resize-to-client). With
                // 2+ heads reconcile_windows owns the input map.
                if self.monitors.len() <= 1 {
                    if let Some(input) = &mut self.input_state {
                        input.set_remote_size(frame.width, frame.height);
                        input.set_remote_offset(0, 0);
                    }
                }
                if let Some(renderer) = &mut self.renderer {
                    renderer.upload_frame(&frame);
                }
            }
        }
        if (primary_frame || overlay_open) && self.window_visible {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }

        // Secondary head: its own independent stream, uploaded and redrawn on
        // its own schedule.
        if let Some(sec) = &mut self.secondary {
            if sec.decode.take_frame_ready() {
                if let Some(frame) = sec.decode.take_frame() {
                    sec.renderer.upload_frame(&frame);
                }
                sec.window.request_redraw();
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

                        // Report decoder capabilities + native monitor resolution.
                        // The server uses this for codec arbitration (e.g. falls
                        // back from AV1 to H.264) and --resize-to-client.
                        //
                        // Codec preference favours the GPU: when the GPU can
                        // hardware-decode H.264 but not AV1, report H.264 only
                        // so the server doesn't pick AV1 and push us onto the
                        // (much slower) software path.
                        let (hw_h264, hw_av1) = self.hw_decode;
                        let codecs: Vec<&str> = match self.args.force_codec.as_deref() {
                            Some("av1") => vec!["av1"],
                            Some(_) => vec!["h264"],
                            None if hw_h264 && !hw_av1 => vec!["h264"],
                            None => vec!["av1", "h264"],
                        };
                        let (mon_w, mon_h) = self
                            .window
                            .as_ref()
                            .and_then(|w| w.current_monitor())
                            .map(|m| (m.size().width, m.size().height))
                            .unwrap_or((0, 0));
                        eprintln!(
                            "[caps] reporting codecs={:?} native={}x{}",
                            codecs, mon_w, mon_h,
                        );
                        input.send_raw(protocol::encode_client_caps(&codecs, mon_w, mon_h));
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
        // Count every wire byte (video, cursor, clipboard) for the stats HUD.
        self.stats_bytes += data.len() as u64;

        let msg = match protocol::parse_server_message(data) {
            Some(m) => m,
            None => return,
        };

        match msg {
            ServerMessage::VideoFrame(frame) => {
                self.frame_count += 1;
                self.stats_received += 1;

                // Log first 5 frames + occasional status; suppress per-frame spam after priming.
                if self.frame_count <= 5 || self.frame_count % 300 == 0 {
                    eprintln!("[frame #{}] kf={} pts={} data_bytes={} dim={}x{}  codec={}",
                        self.frame_count, frame.keyframe, frame.pts,
                        frame.data.len(), frame.width, frame.height,
                        self.codec);
                }

                // Feed the primary head's decode pipeline. Skip only when the
                // window is hidden (the second window has its own pipeline).
                if self.window_visible {
                    if let Some(decode) = &mut self.decode {
                        let outcome = decode.submit(frame.data, frame.keyframe);
                        if outcome.dropped {
                            self.stats_dropped += 1;
                        }
                        if outcome.request_keyframe {
                            if let Some(input) = &self.input_state {
                                input.send_keyframe_request();
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
                        eprintln!("[session] switching decoder: {} -> {}", self.codec, info.codec);
                        if let Some(decode) = &mut self.decode {
                            decode.set_codec(&info.codec);
                        }
                        // Extra heads share the codec; rebuild their pipelines too.
                        if let Some(sec) = &mut self.secondary {
                            sec.decode.set_codec(&info.codec);
                        }
                        self.codec = info.codec.clone();
                    } else {
                        eprintln!("[session] codec unchanged ({})", self.codec);
                    }
                } else {
                    eprintln!("[session] WARNING: failed to parse SessionInfo JSON");
                }
            }
            ServerMessage::MonitorLayout(json_bytes) => {
                if let Ok(layout) = serde_json::from_slice::<MonitorLayoutMsg>(json_bytes) {
                    // Drop any malformed zero-size heads so we never open a black
                    // window or divide by zero computing crops.
                    let monitors: Vec<MonitorInfo> = layout
                        .monitors
                        .into_iter()
                        .filter(|m| m.width > 0 && m.height > 0)
                        .collect();
                    if !monitors.is_empty() && monitors != self.monitors {
                        eprintln!("[monitor] layout: {} head(s)", monitors.len());
                        self.monitors = monitors;
                        // Window reconciliation needs the event loop; defer it.
                        self.layout_dirty = true;
                    }
                }
            }
            ServerMessage::MonitorFrame(frame) => {
                self.stats_bytes += frame.data.len() as u64;
                if let Some(sec) = &mut self.secondary {
                    if sec.id == frame.monitor_id {
                        let outcome = sec.decode.submit(frame.data, frame.keyframe);
                        // Keyframe requests are global (they re-IDR every head),
                        // which is enough to resync this one.
                        if outcome.request_keyframe {
                            if let Some(input) = &self.input_state {
                                input.send_keyframe_request();
                            }
                        }
                    }
                }
            }
            ServerMessage::CursorUpdate(_cursor) => {
                // TODO: render remote cursor overlay
            }
            ServerMessage::Pong(sent_ms) => {
                let now_ms = self.app_start.elapsed().as_millis() as u64;
                if now_ms >= sent_ms {
                    self.last_rtt_ms = (now_ms - sent_ms) as f32;
                }
            }
            ServerMessage::ClipboardData(text) => {
                clipboard::set_clipboard(&text, &self.clipboard_last_set);
            }
            ServerMessage::Unknown(t) => {
                eprintln!("[proto] unknown message type 0x{:02x}", t);
            }
        }
    }

    /// Open/close the secondary window and refresh each head's input offset to
    /// match the layout. Each head shows its own stream, so no crop is needed;
    /// input still carries the head's framebuffer offset. Needs the event loop
    /// to create windows, so it runs from `about_to_wait`.
    fn reconcile_windows(&mut self, event_loop: &ActiveEventLoop) {
        self.layout_dirty = false;
        if self.monitors.is_empty() {
            return;
        }

        // Primary window shows head 0.
        let m0 = self.monitors[0].clone();
        if let Some(input) = &mut self.input_state {
            input.set_remote_size(m0.width, m0.height);
            input.set_remote_offset(m0.x, m0.y);
        }

        if self.monitors.len() >= 2 {
            let m1 = self.monitors[1].clone();
            if self.secondary.is_none() {
                eprintln!("[monitor] opening second window for head 1");
                let created = self.create_secondary(event_loop, &m1);
                match created {
                    Ok(sec) => self.secondary = Some(sec),
                    Err(e) => eprintln!("[monitor] failed to open second window: {e:#}"),
                }
            }
            if let Some(sec) = &mut self.secondary {
                sec.input.set_remote_size(m1.width, m1.height);
                sec.input.set_remote_offset(m1.x, m1.y);
            }
        } else if self.secondary.is_some() {
            eprintln!("[monitor] closing second window");
            self.secondary = None;
        }
    }

    /// Create the secondary-monitor window with its own surface, decoder and input.
    fn create_secondary(
        &self,
        event_loop: &ActiveEventLoop,
        head: &MonitorInfo,
    ) -> anyhow::Result<MonitorWindow> {
        let attrs = WindowAttributes::default()
            .with_title(format!("{} (monitor 2)", self.args.title))
            .with_inner_size(winit::dpi::LogicalSize::new(1280, 720));
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_drag_and_drop(false)
        };
        let window = Arc::new(event_loop.create_window(attrs)?);
        let renderer = pollster::block_on(renderer::Renderer::new(window.clone()))?;
        let sender = self
            .transport_sender
            .clone()
            .ok_or_else(|| anyhow::anyhow!("transport sender not ready"))?;
        let mut input = input::InputState::new(sender);
        // Seed the window size so input scaling is correct before the first
        // Resized event (which some platforms don't deliver on creation).
        let size = window.inner_size();
        input.set_window_size(size.width, size.height);
        let decode = DecodePipeline::new(&self.codec, self.hw_decode);
        Ok(MonitorWindow { id: head.id as u8, window, renderer, decode, input })
    }

    /// Handle a window event for the secondary-monitor window.
    fn handle_secondary_event(&mut self, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(sec) = &self.secondary {
                    sec.input.release_all();
                }
                // Closing the second window unplugs the virtual monitor.
                if let Some(sender) = &self.transport_sender {
                    sender.send(protocol::encode_request_remove_monitor());
                }
                self.secondary = None;
            }
            WindowEvent::Resized(size) => {
                if let Some(sec) = &mut self.secondary {
                    sec.renderer.resize(size.width, size.height);
                    sec.input.set_window_size(size.width, size.height);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(sec) = &mut self.secondary {
                    sec.input.on_cursor_moved(position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(sec) = &mut self.secondary {
                    sec.input.on_mouse_button(button, state);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(sec) = &mut self.secondary {
                    sec.input.on_scroll(delta);
                }
            }
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if let Some(sec) = &mut self.secondary {
                    sec.input.on_key(key_event.physical_key, key_event.state);
                }
            }
            WindowEvent::Focused(false) => {
                if let Some(sec) = &self.secondary {
                    sec.input.release_all();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(sec) = &mut self.secondary {
                    if let Err(e) = sec.renderer.render(None) {
                        tracing::warn!("[monitor2] render error: {}", e);
                    }
                }
            }
            _ => {}
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

    // Probe GPU hardware decode once, before connecting — the result decides
    // both which codecs we advertise and which decoder the decode thread uses.
    #[cfg(windows)]
    let hw_decode = decoder_mf::probe();
    #[cfg(not(windows))]
    let hw_decode = (false, false);

    let mut app = App {
        args,
        window: None,
        renderer: None,
        decode: None,
        input_state: None,
        transport_rx: None,
        rt: rt.handle().clone(),
        codec: "h264".to_string(),
        session_fps: 60,
        frame_count: 0,
        clipboard_last_set: Arc::new(std::sync::Mutex::new(None)),
        window_visible: true,
        overlay: None,
        files_state: None,
        stats_received: 0,
        stats_dropped: 0,
        last_stats_sent: std::time::Instant::now(),
        app_start: std::time::Instant::now(),
        last_rtt_ms: 0.0,
        hw_decode,
        stats: overlay::StatsHistory::new(),
        stats_bytes: 0,
        monitors: Vec::new(),
        layout_dirty: false,
        secondary: None,
        transport_sender: None,
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[FATAL] Event loop error: {}", e);
        std::process::exit(1);
    }
}
