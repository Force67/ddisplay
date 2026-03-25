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

    // Task: broadcast frames -> this WebSocket client
    let send_task = tokio::spawn(async move {
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
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Binary(data) => {
                    if let Some(event) = protocol::parse_client_message(&data) {
                        if input_tx.send(event).await.is_err() {
                            // Main loop dropped the receiver – nothing to do.
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
        _ = send_task => {},
        _ = recv_task => {},
    }

    tracing::info!("WebSocket disconnected");
}
