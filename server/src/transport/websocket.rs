use axum::{
    serve::ListenerExt,
    Router,
    Json,
    extract::Query,
    extract::Path,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use tokio::sync::{broadcast, mpsc};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use crate::control::StreamControl;
use crate::protocol::{self, ClientCaps, ClientEvent};

/// Broadcast sender for encoded frames (server -> all clients).
pub type FrameSender = broadcast::Sender<Vec<u8>>;
/// Receiver end for input events from clients (client -> server).
pub type InputReceiver = mpsc::Receiver<protocol::ClientEvent>;

pub struct AppState {
    pub frame_tx: FrameSender,
    input_tx: mpsc::Sender<ClientEvent>,
    metadata: ServerMetadata,
    status: Mutex<ServerStatusState>,
    runtime: ServerRuntimeConfig,
    clipboard_set_tx: Option<std::sync::mpsc::SyncSender<String>>,
    pub shared_dir: Option<PathBuf>,
    /// Path to the Windows client binary served at /download/client.
    pub client_bin: Option<PathBuf>,
    /// Shared control plane with the capture/encode loop.
    control: Arc<StreamControl>,
    /// Codecs this server can encode, in preference order.
    server_codecs: Vec<String>,
    /// Resize the X display to a connecting client's native resolution.
    resize_to_client: bool,
    /// Capabilities reported by currently connected clients, by connection id.
    client_caps: Mutex<HashMap<u64, ClientCaps>>,
    next_conn_id: AtomicU64,
}

impl AppState {
    /// Pick the best codec every caps-reporting client can decode and request
    /// a switch when it differs from the live stream. Clients that never sent
    /// caps (e.g. the web client) don't constrain the choice.
    fn arbitrate_codec(&self) {
        let caps = self.client_caps.lock();
        let chosen = self
            .server_codecs
            .iter()
            .find(|codec| {
                caps.values()
                    .all(|c| c.codecs.is_empty() || c.codecs.iter().any(|cc| cc == *codec))
            })
            .cloned();
        drop(caps);

        let Some(chosen) = chosen else {
            tracing::warn!("[codec] no codec supported by all clients; keeping current");
            return;
        };
        let current = self.control.session.lock().codec.clone();
        if chosen != current {
            tracing::info!("[codec] arbitration: {} -> {}", current, chosen);
            *self.control.desired_codec.lock() = Some(chosen);
        }
    }
}

#[derive(Default)]
struct ConnectionInputState {
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u8>,
    last_pointer: (u16, u16),
    /// Last time we saw a keydown for each held key (refreshed by OS auto-repeat).
    /// Used by the stuck-key watchdog to distinguish a genuinely held key (refreshed
    /// every repeat interval) from one stranded by client focus loss (never refreshed).
    key_last_seen: HashMap<u32, Instant>,
}

/// Release a held key whose last keydown is older than this. The client's OS
/// auto-repeat refreshes held keys roughly every 30ms, so a key untouched this
/// long is stuck (e.g. the client lost focus without sending the release).
const STUCK_KEY_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone)]
pub struct ServerMetadata {
    pub session_name: String,
    pub display: String,
    pub xauthority: String,
}

#[derive(Clone)]
pub struct ServerRuntimeConfig {
    pub bind_addr: String,
    pub client_dir: PathBuf,
    pub session_name: String,
    pub display: String,
    pub xauthority: String,
    pub fps: u32,
    pub bitrate: u32,
}

#[derive(Default)]
struct ServerStatusState {
    total_clients: usize,
    writable_clients: usize,
    readonly_clients: usize,
}

#[derive(Default, Deserialize)]
struct ConnectionQuery {
    readonly: Option<bool>,
}

#[derive(Serialize)]
struct StatusResponse {
    session_name: String,
    display: String,
    xauthority: String,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    codec: String,
    total_clients: usize,
    writable_clients: usize,
    readonly_clients: usize,
    multiple_writers: bool,
}

#[derive(Serialize)]
struct ClipboardResponse {
    selection: String,
    text: String,
}

#[derive(Deserialize)]
struct RestartRequest {
    bitrate: Option<u32>,
    fps: Option<u32>,
}

#[derive(Serialize)]
struct RestartResponse {
    restarting: bool,
    bind_addr: String,
    bitrate: u32,
    fps: u32,
}

/// Start the WebSocket server.
///
/// Returns a `FrameSender` for broadcasting encoded frames and an
/// `InputReceiver` for consuming client input events.
pub async fn start_server(
    bind_addr: String,
    client_dir: PathBuf,
    metadata: ServerMetadata,
    runtime: ServerRuntimeConfig,
    shared_dir: Option<PathBuf>,
    client_bin: Option<PathBuf>,
    control: Arc<StreamControl>,
    server_codecs: Vec<String>,
    resize_to_client: bool,
) -> anyhow::Result<(FrameSender, InputReceiver)> {
    // Capacity absorbs short TCP send stalls without dropping frames (a drop
    // breaks the H.264/AV1 reference chain and costs a full IDR resync).
    // 8 frames ≈ 130ms at 60fps; a client further behind than that is
    // genuinely congested and handled by the Lagged path below.
    let (frame_tx, _) = broadcast::channel::<Vec<u8>>(8);
    let (input_tx, input_rx) = mpsc::channel::<ClientEvent>(1024);

    // Start clipboard monitor (best-effort; server still runs if it fails).
    let clipboard_set_tx = match crate::clipboard::start(frame_tx.clone()) {
        Ok(tx) => {
            tracing::info!("[clipboard] monitor started");
            Some(tx)
        }
        Err(e) => {
            tracing::warn!("[clipboard] failed to start monitor: {}", e);
            None
        }
    };

    let state = Arc::new(AppState {
        frame_tx: frame_tx.clone(),
        input_tx,
        metadata,
        status: Mutex::new(ServerStatusState::default()),
        runtime,
        clipboard_set_tx,
        shared_dir: shared_dir.clone(),
        client_bin,
        control: control.clone(),
        server_codecs,
        resize_to_client,
        client_caps: Mutex::new(HashMap::new()),
        next_conn_id: AtomicU64::new(1),
    });

    // Adaptive bitrate recovery: ramp the target back towards the ceiling
    // once the link has been congestion-free for a while.
    let abr_control = control;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3));
        loop {
            ticker.tick().await;
            abr_control.maybe_recover_bitrate();
        }
    });

    let mut app = Router::new()
        .route("/ws", get(ws_upgrade_handler))
        .route("/api/status", get(status_handler))
        .route("/api/clipboard/{selection}", get(clipboard_handler))
        .route("/api/control/restart", post(restart_handler))
        .route("/health", get(health_handler));

    if shared_dir.is_some() {
        app = app
            .route("/files", get(crate::files::list_handler))
            .route("/files/", get(crate::files::list_handler))
            .route("/files/list", get(crate::files::list_json_handler))
            .route("/files/upload", post(crate::files::upload_handler))
            .route("/files/{name}", get(crate::files::download_handler));
    }

    app = app.route("/download/client", get(crate::files::client_download_handler));

    let app = app
        .with_state(state)
        .fallback_service(ServeDir::new(client_dir));

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("WebSocket server listening on {}", bind_addr);

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener.tap_io(|s| { let _ = s.set_nodelay(true); }), app).await {
            tracing::error!("Server error: {}", e);
        }
    });

    Ok((frame_tx, input_rx))
}

/// Axum handler that upgrades an HTTP request to a WebSocket connection.
async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ConnectionQuery>,
) -> impl IntoResponse {
    let readonly = query.readonly.unwrap_or(false);
    ws.on_upgrade(move |socket| handle_websocket(socket, state, readonly))
}

/// Manage the lifetime of a single WebSocket client.
async fn handle_websocket(socket: WebSocket, state: Arc<AppState>, readonly: bool) {
    let peer = "client"; // axum 0.8 doesn't expose peer addr on the ws directly
    let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);
    tracing::info!("{}#{}: WebSocket connected (readonly={})", peer, conn_id, readonly);
    update_client_counts(&state, readonly, true);

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let input_state = Arc::new(Mutex::new(ConnectionInputState::default()));

    // Per-client channel for direct (non-broadcast) replies, e.g. pong echoes.
    let (direct_tx, mut direct_rx) = mpsc::channel::<Vec<u8>>(32);

    // Per-client channel for forwarded USB traffic. Bounded sends backpressure
    // the vhci socket reads instead of dropping URB bytes (a lost chunk
    // desyncs the usbip stream for good).
    let (usb_tx, mut usb_rx) = mpsc::channel::<Vec<u8>>(64);

    // Send live session info (codec, resolution, etc.) so client can configure decoder.
    let session_info = protocol::encode_session_info(&state.control.session.lock().clone());
    if ws_sender.send(Message::Binary(session_info.into())).await.is_err() {
        return;
    }

    // Send the current monitor layout so the client opens one window per head
    // and crops each to its rect.
    let layout = protocol::encode_monitor_layout(&state.control.monitor_layout());
    if ws_sender.send(Message::Binary(layout.into())).await.is_err() {
        return;
    }

    // Subscribe to the frame broadcast, then ask the encode loop for a fresh
    // IDR. With an infinite GOP this is what bootstraps the new decoder —
    // the IDR arrives within a frame interval (the wake cuts idle pacing
    // short), so there's no value in caching stale keyframes.
    let mut frame_rx = state.frame_tx.subscribe();
    state.control.request_keyframe_all();
    state.control.notify_activity();
    let input_tx = state.input_tx.clone();
    let recv_input_tx = input_tx.clone();
    let recv_input_state = input_state.clone();
    let clipboard_set_tx = state.clipboard_set_tx.clone();
    let send_control = state.control.clone();
    let recv_state = state.clone();

    // Task: broadcast frames (+ direct replies) -> this WebSocket client
    let mut send_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                // Direct replies first — tiny and latency-sensitive (RTT probes).
                biased;

                direct = direct_rx.recv() => {
                    match direct {
                        Some(data) => {
                            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }

                // USB URB traffic: small, ordered, must not be dropped.
                usb = usb_rx.recv() => {
                    match usb {
                        Some(data) => {
                            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }

                frame = frame_rx.recv() => {
                    match frame {
                        Ok(data) => {
                            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                                // Client disconnected
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("{}: dropped {} frames (slow client)", peer, n);
                            // The receiver advances past the gap, which breaks the
                            // codec reference chain — without a fresh IDR the client
                            // shows smeared/pixelated regions indefinitely (infinite
                            // GOP). Also tell the ABR controller to back off.
                            send_control.record_congestion();
                            send_control.request_keyframe_all();
                            send_control.notify_activity();
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Server is shutting down.
                            break;
                        }
                    }
                }
            }
        }
    });

    // Task: this WebSocket client -> input channel
    let mut recv_task = tokio::spawn(async move {
        // Forwarded USB devices live and die with this connection: dropping
        // this (normal exit or abort) detaches every vhci port.
        let mut usb = crate::usb::ConnectionUsb::new(usb_tx);
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Binary(data) => {
                    let Some(event) = protocol::parse_client_message(&data) else {
                        continue;
                    };
                    // Control-plane messages are handled here for every client
                    // (including readonly viewers — they still decode video).
                    match event {
                        ClientEvent::Ping { payload } => {
                            // Echo verbatim; client measures RTT.
                            let _ = direct_tx.try_send(payload);
                        }
                        ClientEvent::Caps(caps) => {
                            tracing::info!(
                                "{}#{}: caps: codecs={:?} native={}x{}",
                                peer, conn_id, caps.codecs, caps.width, caps.height,
                            );
                            if recv_state.resize_to_client
                                && !readonly
                                && caps.width > 0
                                && caps.height > 0
                            {
                                *recv_state.control.resize_request.lock() =
                                    Some((caps.width, caps.height));
                            }
                            recv_state.client_caps.lock().insert(conn_id, caps);
                            recv_state.arbitrate_codec();
                        }
                        ClientEvent::Stats(stats) => {
                            if stats.dropped > 0 {
                                tracing::debug!(
                                    "{}#{}: client dropped {} frames (decode backlog)",
                                    peer, conn_id, stats.dropped,
                                );
                                recv_state.control.record_congestion();
                            }
                        }
                        // Clipboard data is handled locally; never forwarded to the input injector.
                        ClientEvent::ClipboardData { text } => {
                            if readonly {
                                continue;
                            }
                            if let Some(ref tx) = clipboard_set_tx {
                                let _ = tx.try_send(text);
                            }
                        }
                        // USB forwarding is a per-connection side channel, like
                        // the clipboard: never injected as input. Gated for
                        // readonly viewers (attaching hardware is write access).
                        ClientEvent::UsbAttach(req) => {
                            if readonly {
                                continue;
                            }
                            usb.attach(req).await;
                        }
                        ClientEvent::UsbData { token, data } => {
                            if readonly {
                                continue;
                            }
                            usb.data(token, data).await;
                        }
                        ClientEvent::UsbDetach { token } => {
                            usb.detach(token);
                        }
                        // Plug/unplug a virtual head. The encode loop applies the
                        // new count (widen framebuffer + declare heads) and
                        // broadcasts the layout. Gated like input.
                        ev @ (ClientEvent::RequestAddMonitor
                        | ClientEvent::RequestRemoveMonitor) => {
                            if readonly {
                                continue;
                            }
                            let current = recv_state.control.monitors.lock().len();
                            let desired = match ev {
                                ClientEvent::RequestAddMonitor => {
                                    (current + 1).min(crate::monitor::MAX_MONITORS)
                                }
                                _ => current.saturating_sub(1).max(1),
                            };
                            *recv_state.control.monitor_request.lock() = Some(desired);
                            recv_state.control.notify_activity();
                        }
                        other => {
                            if readonly {
                                continue;
                            }
                            let forwarded_events = normalize_input_events(&recv_input_state, other);
                            for forwarded_event in forwarded_events {
                                tracing::debug!("Input event: {:?}", forwarded_event);
                                if recv_input_tx.send(forwarded_event).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                Message::Close(_) => {
                    break;
                }
                // Ignore text, ping, pong – axum handles pong automatically.
                _ => {}
            }
        }
    });

    // Task: release keys that get stranded mid-connection. A wedged client can stop
    // sending the keyup for a held key (e.g. on focus loss) while staying connected, so
    // the per-connection cleanup on disconnect never fires. Genuinely held keys keep
    // refreshing key_last_seen via OS auto-repeat; a stuck one goes stale and is released.
    let watchdog_state = input_state.clone();
    let watchdog_tx = input_tx.clone();
    let mut watchdog_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        loop {
            ticker.tick().await;
            let stale: Vec<u32> = {
                let mut st = watchdog_state.lock();
                let now = Instant::now();
                let stale: Vec<u32> = st
                    .key_last_seen
                    .iter()
                    .filter(|(_, seen)| now.duration_since(**seen) > STUCK_KEY_TIMEOUT)
                    .map(|(kc, _)| *kc)
                    .collect();
                for kc in &stale {
                    st.pressed_keys.remove(kc);
                    st.key_last_seen.remove(kc);
                }
                stale
            };
            for keycode in stale {
                tracing::warn!(
                    "Key {} held with no refresh for >{}ms; releasing (stuck-key watchdog)",
                    keycode,
                    STUCK_KEY_TIMEOUT.as_millis(),
                );
                if watchdog_tx
                    .send(ClientEvent::KeyEvent { keycode, pressed: false })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    });

    // When either task finishes the connection is done; cancel the others.
    tokio::select! {
        _ = &mut send_task => {},
        _ = &mut recv_task => {},
    }

    // Detach neither task; stop the other side promptly before releasing input.
    // Aborting a completed task is a no-op.
    // This keeps disconnected clients from leaving background websocket tasks around.
    send_task.abort();
    recv_task.abort();
    watchdog_task.abort();

    if !readonly {
        cleanup_connection_input(&input_tx, &input_state).await;
    }
    update_client_counts(&state, readonly, false);

    // Forget this client's caps and re-arbitrate — e.g. when the only
    // H.264-limited client leaves, the server switches back to AV1.
    if state.client_caps.lock().remove(&conn_id).is_some() {
        state.arbitrate_codec();
    }

    // When no client that can manage monitors remains, unplug any virtual
    // monitors so the session isn't left altered (and a lone read-only viewer
    // isn't stranded with a head it can't remove). Keyed on writable clients,
    // since read-only clients can't add or remove monitors.
    let no_writers = state.status.lock().writable_clients == 0;
    if no_writers && state.control.monitors.lock().len() > 1 {
        tracing::info!("[monitor] no writable clients left; restoring single head");
        *state.control.monitor_request.lock() = Some(1);
        state.control.notify_activity();
    }

    tracing::info!("{}#{}: WebSocket disconnected", peer, conn_id);
}

async fn status_handler(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let session = state.control.session.lock().clone();
    let status = state.status.lock();
    Json(StatusResponse {
        session_name: state.metadata.session_name.clone(),
        display: state.metadata.display.clone(),
        xauthority: state.metadata.xauthority.clone(),
        width: session.width,
        height: session.height,
        fps: session.fps,
        bitrate: session.bitrate,
        codec: session.codec,
        total_clients: status.total_clients,
        writable_clients: status.writable_clients,
        readonly_clients: status.readonly_clients,
        multiple_writers: status.writable_clients > 1,
    })
}

async fn clipboard_handler(
    Path(selection): Path<String>,
) -> Result<Json<ClipboardResponse>, (axum::http::StatusCode, String)> {
    let selection = selection.to_lowercase();
    let text = crate::input::x11::read_selection_text(&selection)
        .map_err(|err| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .unwrap_or_default();
    Ok(Json(ClipboardResponse { selection, text }))
}

async fn restart_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RestartRequest>,
) -> Result<Json<RestartResponse>, (axum::http::StatusCode, String)> {
    let mut runtime = state.runtime.clone();
    if let Some(bitrate) = request.bitrate {
        runtime.bitrate = bitrate.clamp(1_000_000, 50_000_000);
    }
    if let Some(fps) = request.fps {
        runtime.fps = fps.clamp(5, 120);
    }

    spawn_replacement(&runtime)
        .map_err(|err| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        std::process::exit(0);
    });

    Ok(Json(RestartResponse {
        restarting: true,
        bind_addr: runtime.bind_addr.clone(),
        bitrate: runtime.bitrate,
        fps: runtime.fps,
    }))
}

async fn health_handler() -> impl IntoResponse {
    "ok"
}

fn spawn_replacement(runtime: &ServerRuntimeConfig) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let log_path = format!("/tmp/ddisplay-{}.log", runtime.session_name);

    let parts = vec![
        shell_escape(exe.as_os_str()),
        "--bind".to_string(),
        shell_escape(OsString::from(&runtime.bind_addr).as_os_str()),
        "--display".to_string(),
        shell_escape(OsString::from(&runtime.display).as_os_str()),
        "--xauthority".to_string(),
        shell_escape(OsString::from(&runtime.xauthority).as_os_str()),
        "--session-name".to_string(),
        shell_escape(OsString::from(&runtime.session_name).as_os_str()),
        "--fps".to_string(),
        runtime.fps.to_string(),
        "--bitrate".to_string(),
        runtime.bitrate.to_string(),
        "--client-dir".to_string(),
        shell_escape(runtime.client_dir.as_os_str()),
    ];
    let command = format!(
        "sleep 0.5; nohup {} >/tmp/{}.stdout 2>{} < /dev/null &",
        parts.join(" "),
        runtime.session_name,
        shell_escape(OsString::from(log_path).as_os_str()),
    );

    Command::new("setsid")
        .arg("bash")
        .arg("-lc")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(())
}

fn shell_escape(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn update_client_counts(state: &Arc<AppState>, readonly: bool, connected: bool) {
    let mut status = state.status.lock();
    if connected {
        status.total_clients += 1;
        if readonly {
            status.readonly_clients += 1;
        } else {
            status.writable_clients += 1;
        }
        return;
    }

    status.total_clients = status.total_clients.saturating_sub(1);
    if readonly {
        status.readonly_clients = status.readonly_clients.saturating_sub(1);
    } else {
        status.writable_clients = status.writable_clients.saturating_sub(1);
    }
}

fn normalize_input_events(
    state: &Arc<Mutex<ConnectionInputState>>,
    event: ClientEvent,
) -> Vec<ClientEvent> {
    let mut state = state.lock();
    match event {
        ClientEvent::MouseMove { x, y } => {
            state.last_pointer = (x, y);
            vec![ClientEvent::MouseMove { x, y }]
        }
        ClientEvent::MouseButton { button, pressed, x, y } => {
            state.last_pointer = (x, y);
            if pressed {
                let already_pressed = state.pressed_buttons.contains(&button);
                state.pressed_buttons.insert(button);
                if already_pressed {
                    tracing::warn!("Button {} was already pressed; injecting release+press to resync", button);
                    vec![
                        ClientEvent::MouseButton { button, pressed: false, x, y },
                        ClientEvent::MouseButton { button, pressed: true, x, y },
                    ]
                } else {
                    vec![ClientEvent::MouseButton { button, pressed: true, x, y }]
                }
            } else {
                state.pressed_buttons.remove(&button);
                vec![ClientEvent::MouseButton { button, pressed: false, x, y }]
            }
        }
        ClientEvent::MouseScroll { dx, dy, x, y } => {
            state.last_pointer = (x, y);
            vec![ClientEvent::MouseScroll { dx, dy, x, y }]
        }
        ClientEvent::KeyEvent { keycode, pressed } => {
            if pressed {
                let already_pressed = state.pressed_keys.contains(&keycode);
                state.pressed_keys.insert(keycode);
                state.key_last_seen.insert(keycode, Instant::now());
                if already_pressed {
                    // OS auto-repeat: the key is already down in X, and Xvfb runs its
                    // own key-repeat off the held key, so swallow the duplicate keydown
                    // instead of re-injecting release+press. The old resync turned every
                    // repeat into a full keystroke, which is what let a wedged client flood
                    // the session (e.g. stuck modifiers tripping global shortcuts).
                    vec![]
                } else {
                    vec![ClientEvent::KeyEvent { keycode, pressed: true }]
                }
            } else {
                state.pressed_keys.remove(&keycode);
                state.key_last_seen.remove(&keycode);
                vec![ClientEvent::KeyEvent { keycode, pressed: false }]
            }
        }
        ClientEvent::ClientReady => vec![ClientEvent::ClientReady],
        ClientEvent::PasteText { text } => vec![ClientEvent::PasteText { text }],
        ClientEvent::ReleaseKeys => {
            state.pressed_keys.clear();
            state.key_last_seen.clear();
            vec![ClientEvent::ReleaseKeys]
        }
        ClientEvent::ReleaseMouse => {
            state.pressed_buttons.clear();
            vec![ClientEvent::ReleaseMouse]
        }
        ClientEvent::ReleaseAll => {
            state.pressed_keys.clear();
            state.key_last_seen.clear();
            state.pressed_buttons.clear();
            vec![ClientEvent::ReleaseAll]
        }
        ClientEvent::ClipboardData { .. } => {
            // Intercepted in the recv_task before reaching normalization.
            vec![]
        }
        ClientEvent::RequestKeyframe { head } => {
            // Passed through directly to the input handler which flags the head(s).
            vec![ClientEvent::RequestKeyframe { head }]
        }
        ClientEvent::Caps(_)
        | ClientEvent::Stats(_)
        | ClientEvent::Ping { .. }
        | ClientEvent::RequestAddMonitor
        | ClientEvent::RequestRemoveMonitor
        | ClientEvent::UsbAttach(_)
        | ClientEvent::UsbData { .. }
        | ClientEvent::UsbDetach { .. } => {
            // Intercepted in the recv_task before reaching normalization.
            vec![]
        }
    }
}

async fn cleanup_connection_input(
    input_tx: &mpsc::Sender<ClientEvent>,
    state: &Arc<Mutex<ConnectionInputState>>,
) {
    let (mut pressed_keys, mut pressed_buttons, (x, y)) = {
        let mut state = state.lock();
        let keys = state.pressed_keys.drain().collect::<Vec<_>>();
        let buttons = state.pressed_buttons.drain().collect::<Vec<_>>();
        (keys, buttons, state.last_pointer)
    };

    if pressed_keys.is_empty() && pressed_buttons.is_empty() {
        return;
    }

    pressed_keys.sort_unstable();
    pressed_buttons.sort_unstable();

    tracing::warn!(
        "WebSocket disconnected with {} pressed keys and {} pressed buttons; releasing synthetic input",
        pressed_keys.len(),
        pressed_buttons.len(),
    );

    for button in pressed_buttons {
        if input_tx
            .send(ClientEvent::MouseButton {
                button,
                pressed: false,
                x,
                y,
            })
            .await
            .is_err()
        {
            return;
        }
    }

    for keycode in pressed_keys {
        if input_tx
            .send(ClientEvent::KeyEvent {
                keycode,
                pressed: false,
            })
            .await
            .is_err()
        {
            return;
        }
    }
}
