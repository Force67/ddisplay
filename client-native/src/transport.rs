/// WebSocket transport with auto-reconnect.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Messages flowing between the transport and the app.
pub enum TransportEvent {
    Connected,
    Disconnected,
    Data(Vec<u8>),
}

/// Handle for sending data to the server.
#[derive(Clone)]
pub struct TransportSender {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl TransportSender {
    pub fn send(&self, data: Vec<u8>) {
        let _ = self.tx.send(data);
    }
}

/// Spawn the WebSocket transport as a background task.
///
/// Returns a sender for outgoing messages and a receiver for incoming events.
pub fn spawn(
    url: String,
) -> (TransportSender, mpsc::UnboundedReceiver<TransportEvent>) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (send_tx, send_rx) = mpsc::unbounded_channel();

    tokio::spawn(transport_loop(url, event_tx, send_rx));

    (TransportSender { tx: send_tx }, event_rx)
}

async fn transport_loop(
    url: String,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    mut send_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let mut backoff = std::time::Duration::from_millis(500);
    let max_backoff = std::time::Duration::from_secs(10);

    loop {
        tracing::info!("Connecting to {}", url);

        match connect_once(&url, &event_tx, &mut send_rx).await {
            Ok(()) => {
                tracing::info!("Connection closed cleanly");
            }
            Err(e) => {
                tracing::warn!("Connection error: {}", e);
            }
        }

        let _ = event_tx.send(TransportEvent::Disconnected);

        tracing::info!("Reconnecting in {:?}", backoff);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn connect_once(
    url: &str,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
    send_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<()> {
    // Manual TCP connect so we can set TCP_NODELAY before the WebSocket handshake.
    // Without it, Nagle's algorithm can silently buffer mouse/keyboard sends for up to 40ms.
    let parsed = url::Url::parse(url).context("invalid WebSocket URL")?;
    let host = parsed.host_str().unwrap_or("localhost");
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr = format!("{}:{}", host, port);

    let tcp = tokio::net::TcpStream::connect(&addr)
        .await
        .with_context(|| format!("TCP connect to {} failed", addr))?;
    tcp.set_nodelay(true).context("TCP_NODELAY failed")?;

    let (ws_stream, _) = tokio_tungstenite::client_async(url, tcp)
        .await
        .context("WebSocket handshake failed")?;

    tracing::info!("Connected (TCP_NODELAY=true)");
    let _ = event_tx.send(TransportEvent::Connected);

    let (mut ws_sink, mut ws_stream_rx) = ws_stream.split();

    loop {
        tokio::select! {
            // biased: outgoing sends are checked first every iteration.
            // Without this, 60fps incoming video can starve mouse/keyboard
            // sends by winning the random poll, adding visible click latency.
            biased;

            // Outgoing to server (mouse / keyboard — latency-sensitive)
            data = send_rx.recv() => {
                match data {
                    Some(buf) => {
                        ws_sink.send(Message::Binary(buf.into())).await?;
                    }
                    None => {
                        // Sender dropped, shutdown
                        return Ok(());
                    }
                }
            }

            // Incoming from server (video frames)
            msg = ws_stream_rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        let _ = event_tx.send(TransportEvent::Data(data.to_vec()));
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(e.into());
                    }
                    _ => {} // ignore text, ping, pong
                }
            }
        }
    }
}
