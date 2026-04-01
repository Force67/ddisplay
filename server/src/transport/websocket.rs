use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use tower_http::services::ServeDir;
use tokio::sync::{broadcast, mpsc};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::Mutex;
use crate::protocol::{self, ClientEvent};

/// Broadcast sender for encoded frames (server -> all clients).
pub type FrameSender = broadcast::Sender<Vec<u8>>;
/// Receiver end for input events from clients (client -> server).
pub type InputReceiver = mpsc::Receiver<protocol::ClientEvent>;
/// Shared cache for the latest keyframe.
pub type KeyframeCache = Arc<Mutex<Option<Vec<u8>>>>;

struct AppState {
    frame_tx: FrameSender,
    input_tx: mpsc::Sender<ClientEvent>,
    keyframe_cache: KeyframeCache,
}

#[derive(Default)]
struct ConnectionInputState {
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u8>,
    last_pointer: (u16, u16),
}

/// Start the WebSocket server.
///
/// Returns a `FrameSender` for broadcasting encoded frames and an
/// `InputReceiver` for consuming client input events.
pub async fn start_server(
    bind_addr: String,
    client_dir: PathBuf,
) -> anyhow::Result<(FrameSender, InputReceiver, KeyframeCache)> {
    let (frame_tx, _) = broadcast::channel::<Vec<u8>>(120);
    let (input_tx, input_rx) = mpsc::channel::<ClientEvent>(1024);
    let keyframe_cache: KeyframeCache = Arc::new(Mutex::new(None));

    let state = Arc::new(AppState {
        frame_tx: frame_tx.clone(),
        input_tx,
        keyframe_cache: keyframe_cache.clone(),
    });

    let app = Router::new()
        .route("/ws", get(ws_upgrade_handler))
        .with_state(state)
        .fallback_service(ServeDir::new(client_dir));

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("WebSocket server listening on {}", bind_addr);

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("Server error: {}", e);
        }
    });

    Ok((frame_tx, input_rx, keyframe_cache))
}

/// Axum handler that upgrades an HTTP request to a WebSocket connection.
async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_websocket(socket, state))
}

/// Manage the lifetime of a single WebSocket client.
async fn handle_websocket(socket: WebSocket, state: Arc<AppState>) {
    let peer = "client"; // axum 0.8 doesn't expose peer addr on the ws directly
    tracing::info!("{}: WebSocket connected", peer);

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let input_state = Arc::new(Mutex::new(ConnectionInputState::default()));

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
                    if let Some(event) = protocol::parse_client_message(&data) {
                        track_input_state(&recv_input_state, &event);
                        tracing::debug!("Input event: {:?}", event);
                        if recv_input_tx.send(event).await.is_err() {
                            break;
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

    cleanup_connection_input(&input_tx, &input_state).await;

    tracing::info!("WebSocket disconnected");
}

fn track_input_state(state: &Arc<Mutex<ConnectionInputState>>, event: &ClientEvent) {
    let mut state = state.lock();
    match event {
        ClientEvent::MouseMove { x, y } => {
            state.last_pointer = (*x, *y);
        }
        ClientEvent::MouseButton { button, pressed, x, y } => {
            state.last_pointer = (*x, *y);
            if *pressed {
                state.pressed_buttons.insert(*button);
            } else {
                state.pressed_buttons.remove(button);
            }
        }
        ClientEvent::MouseScroll { x, y, .. } => {
            state.last_pointer = (*x, *y);
        }
        ClientEvent::KeyEvent { keycode, pressed } => {
            if *pressed {
                state.pressed_keys.insert(*keycode);
            } else {
                state.pressed_keys.remove(keycode);
            }
        }
        ClientEvent::ClientReady => {}
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
