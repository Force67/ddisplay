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
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;
use crate::protocol::{self, ClientEvent};

/// Broadcast sender for encoded frames (server -> all clients).
pub type FrameSender = broadcast::Sender<Vec<u8>>;
/// Receiver end for input events from clients (client -> server).
pub type InputReceiver = mpsc::Receiver<protocol::ClientEvent>;
/// Shared cache for the latest keyframe.
pub type KeyframeCache = Arc<Mutex<Option<Vec<u8>>>>;

pub struct AppState {
    pub frame_tx: FrameSender,
    input_tx: mpsc::Sender<ClientEvent>,
    keyframe_cache: KeyframeCache,
    metadata: ServerMetadata,
    status: Mutex<ServerStatusState>,
    runtime: ServerRuntimeConfig,
    clipboard_set_tx: Option<std::sync::mpsc::SyncSender<String>>,
    pub shared_dir: Option<PathBuf>,
}

#[derive(Default)]
struct ConnectionInputState {
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u8>,
    last_pointer: (u16, u16),
}

#[derive(Clone)]
pub struct ServerMetadata {
    pub session_name: String,
    pub display: String,
    pub xauthority: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub codec: String,
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
) -> anyhow::Result<(FrameSender, InputReceiver, KeyframeCache)> {
    let (frame_tx, _) = broadcast::channel::<Vec<u8>>(2);
    let (input_tx, input_rx) = mpsc::channel::<ClientEvent>(1024);
    let keyframe_cache: KeyframeCache = Arc::new(Mutex::new(None));

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
        keyframe_cache: keyframe_cache.clone(),
        metadata,
        status: Mutex::new(ServerStatusState::default()),
        runtime,
        clipboard_set_tx,
        shared_dir: shared_dir.clone(),
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
            .route("/files/{name}", get(crate::files::download_handler))
            .route("/files/upload", post(crate::files::upload_handler));
    }

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

    Ok((frame_tx, input_rx, keyframe_cache))
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
    tracing::info!("{}: WebSocket connected (readonly={})", peer, readonly);
    update_client_counts(&state, readonly, true);

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let input_state = Arc::new(Mutex::new(ConnectionInputState::default()));

    // Send session info (codec, resolution, etc.) so client can configure decoder.
    let session_info = protocol::encode_session_info(&protocol::SessionInfo {
        width: state.metadata.width,
        height: state.metadata.height,
        fps: state.metadata.fps,
        codec: state.metadata.codec.clone(),
    });
    if ws_sender.send(Message::Binary(session_info.into())).await.is_err() {
        return;
    }

    // Send cached keyframe immediately so the client can start decoding.
    let cached_kf = state.keyframe_cache.lock().clone();
    if let Some(kf) = cached_kf {
        if ws_sender.send(Message::Binary(kf.into())).await.is_err() {
            return;
        }
        tracing::debug!("Sent cached keyframe to new client");
    }

    // Subscribe to the frame broadcast so this client receives all future frames.
    let mut frame_rx = state.frame_tx.subscribe();
    let input_tx = state.input_tx.clone();
    let recv_input_tx = input_tx.clone();
    let recv_input_state = input_state.clone();
    let clipboard_set_tx = state.clipboard_set_tx.clone();

    // Task: broadcast frames -> this WebSocket client
    let mut send_task = tokio::spawn(async move {
        loop {
            match frame_rx.recv().await {
                Ok(data) => {
                    if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                        // Client disconnected
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("{}: dropped {} frames (slow client)", peer, n);
                    // Continue – the receiver automatically advances past the gap.
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Server is shutting down.
                    break;
                }
            }
        }
    });

    // Task: this WebSocket client -> input channel
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Binary(data) => {
                    if readonly {
                        continue;
                    }
                    if let Some(event) = protocol::parse_client_message(&data) {
                        // Clipboard data is handled locally; never forwarded to the input injector.
                        if let ClientEvent::ClipboardData { text } = event {
                            if let Some(ref tx) = clipboard_set_tx {
                                let _ = tx.try_send(text);
                            }
                            continue;
                        }
                        let forwarded_events = normalize_input_events(&recv_input_state, event);
                        for forwarded_event in forwarded_events {
                            tracing::debug!("Input event: {:?}", forwarded_event);
                            if recv_input_tx.send(forwarded_event).await.is_err() {
                                return;
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

    // When either task finishes the connection is done; cancel the other.
    tokio::select! {
        _ = &mut send_task => {},
        _ = &mut recv_task => {},
    }

    // Detach neither task; stop the other side promptly before releasing input.
    // Aborting a completed task is a no-op.
    // This keeps disconnected clients from leaving background websocket tasks around.
    send_task.abort();
    recv_task.abort();

    if !readonly {
        cleanup_connection_input(&input_tx, &input_state).await;
    }
    update_client_counts(&state, readonly, false);

    tracing::info!("WebSocket disconnected");
}

async fn status_handler(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let status = state.status.lock();
    Json(StatusResponse {
        session_name: state.metadata.session_name.clone(),
        display: state.metadata.display.clone(),
        xauthority: state.metadata.xauthority.clone(),
        width: state.metadata.width,
        height: state.metadata.height,
        fps: state.metadata.fps,
        bitrate: state.metadata.bitrate,
        codec: state.metadata.codec.clone(),
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
                if already_pressed {
                    tracing::warn!("Key {} was already pressed; injecting release+press to resync", keycode);
                    vec![
                        ClientEvent::KeyEvent { keycode, pressed: false },
                        ClientEvent::KeyEvent { keycode, pressed: true },
                    ]
                } else {
                    vec![ClientEvent::KeyEvent { keycode, pressed: true }]
                }
            } else {
                state.pressed_keys.remove(&keycode);
                vec![ClientEvent::KeyEvent { keycode, pressed: false }]
            }
        }
        ClientEvent::ClientReady => vec![ClientEvent::ClientReady],
        ClientEvent::PasteText { text } => vec![ClientEvent::PasteText { text }],
        ClientEvent::ReleaseKeys => {
            state.pressed_keys.clear();
            vec![ClientEvent::ReleaseKeys]
        }
        ClientEvent::ReleaseMouse => {
            state.pressed_buttons.clear();
            vec![ClientEvent::ReleaseMouse]
        }
        ClientEvent::ReleaseAll => {
            state.pressed_keys.clear();
            state.pressed_buttons.clear();
            vec![ClientEvent::ReleaseAll]
        }
        ClientEvent::ClipboardData { .. } => {
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
