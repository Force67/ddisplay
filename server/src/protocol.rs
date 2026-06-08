/// Wire protocol for ddisplay server <-> client communication.
///
/// All messages are sent over WebSocket as binary frames.
/// Format: [type: u8] [payload...]
///
/// Server -> Client:
///   0x01 VideoFrame: [keyframe: u8] [pts: u64 LE] [width: u16 LE] [height: u16 LE] [data...]
///   0x02 CursorUpdate: [x: u16 LE] [y: u16 LE] [visible: u8]
///   0x03 SessionInfo: JSON payload
///   0x20 ClipboardData: [utf8 text...]   (bidirectional)
///
/// Client -> Server:
///   0x10 MouseMove: [x: u16 LE] [y: u16 LE]
///   0x11 MouseButton: [button: u8] [pressed: u8] [x: u16 LE] [y: u16 LE]
///   0x12 MouseScroll: [dx: i16 LE] [dy: i16 LE] [x: u16 LE] [y: u16 LE]
///   0x13 KeyEvent: [keycode: u32 LE] [pressed: u8]
///   0x14 ClientReady: (no payload)
///   0x15 PasteText: [utf8 text...]
///   0x16 ReleaseKeys: (no payload)
///   0x17 ReleaseMouse: (no payload)
///   0x18 ReleaseAll: (no payload)
///   0x20 ClipboardData: [utf8 text...]   (bidirectional)
///   0x21 RequestKeyframe: (no payload)
///   0x22 ClientCaps: JSON {codecs, width, height} — decoder capabilities + native resolution
///   0x23 ClientStats: JSON {received, dropped, decode_ms, rtt_ms} — periodic feedback
///   0x24 Ping: [u64 LE timestamp] — echoed back verbatim by the server (RTT probe)

use serde::{Deserialize, Serialize};

// Message type constants
pub const MSG_VIDEO_FRAME: u8 = 0x01;
pub const MSG_CURSOR_UPDATE: u8 = 0x02;
pub const MSG_SESSION_INFO: u8 = 0x03;

pub const MSG_MOUSE_MOVE: u8 = 0x10;
pub const MSG_MOUSE_BUTTON: u8 = 0x11;
pub const MSG_MOUSE_SCROLL: u8 = 0x12;
pub const MSG_KEY_EVENT: u8 = 0x13;
pub const MSG_CLIENT_READY: u8 = 0x14;
pub const MSG_PASTE_TEXT: u8 = 0x15;
pub const MSG_RELEASE_KEYS: u8 = 0x16;
pub const MSG_RELEASE_MOUSE: u8 = 0x17;
pub const MSG_RELEASE_ALL: u8 = 0x18;
pub const MSG_CLIPBOARD_DATA: u8 = 0x20;
pub const MSG_REQUEST_KEYFRAME: u8 = 0x21;
pub const MSG_CLIENT_CAPS: u8 = 0x22;
pub const MSG_CLIENT_STATS: u8 = 0x23;
pub const MSG_PING: u8 = 0x24;

pub fn encode_clipboard_data(text: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + text.len());
    buf.push(MSG_CLIPBOARD_DATA);
    buf.extend_from_slice(text.as_bytes());
    buf
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    /// Current target bitrate in bits per second (informational for clients).
    #[serde(default)]
    pub bitrate: u32,
}

/// Decoder capabilities + native resolution reported by a client on connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCaps {
    /// Codecs the client can decode, in order of preference (e.g. ["av1", "h264"]).
    #[serde(default)]
    pub codecs: Vec<String>,
    /// Client's native monitor width in pixels (0 = unknown).
    #[serde(default)]
    pub width: u32,
    /// Client's native monitor height in pixels (0 = unknown).
    #[serde(default)]
    pub height: u32,
}

/// Periodic feedback from a client, used for adaptive bitrate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientStats {
    /// Frames received since last report.
    #[serde(default)]
    pub received: u32,
    /// Frames the client had to drop (decode backlog) since last report.
    #[serde(default)]
    pub dropped: u32,
    /// Average decode time per frame in ms.
    #[serde(default)]
    pub decode_ms: f32,
    /// Last measured round-trip time in ms (0 = not measured yet).
    #[serde(default)]
    pub rtt_ms: f32,
}

/// Encode a video frame message into a binary buffer.
pub fn encode_video_frame(
    keyframe: bool,
    pts: u64,
    width: u16,
    height: u16,
    data: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 1 + 8 + 2 + 2 + data.len());
    buf.push(MSG_VIDEO_FRAME);
    buf.push(keyframe as u8);
    buf.extend_from_slice(&pts.to_le_bytes());
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(data);
    buf
}

/// Encode a cursor update message.
pub fn encode_cursor_update(x: u16, y: u16, visible: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(6);
    buf.push(MSG_CURSOR_UPDATE);
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf.push(visible as u8);
    buf
}

/// Encode session info as a JSON message.
pub fn encode_session_info(info: &SessionInfo) -> Vec<u8> {
    let json = serde_json::to_vec(info).unwrap();
    let mut buf = Vec::with_capacity(1 + json.len());
    buf.push(MSG_SESSION_INFO);
    buf.extend_from_slice(&json);
    buf
}

/// Parsed client input event.
#[derive(Debug)]
pub enum ClientEvent {
    MouseMove { x: u16, y: u16 },
    MouseButton { button: u8, pressed: bool, x: u16, y: u16 },
    MouseScroll { dx: i16, dy: i16, x: u16, y: u16 },
    KeyEvent { keycode: u32, pressed: bool },
    ClientReady,
    PasteText { text: String },
    ReleaseKeys,
    ReleaseMouse,
    ReleaseAll,
    ClipboardData { text: String },
    RequestKeyframe,
    /// Decoder capabilities (handled by the transport layer, never injected).
    Caps(ClientCaps),
    /// Periodic client feedback (handled by the transport layer, never injected).
    Stats(ClientStats),
    /// RTT probe — the transport layer echoes the raw payload back.
    Ping { payload: Vec<u8> },
}

/// Parse a binary message from the client.
pub fn parse_client_message(data: &[u8]) -> Option<ClientEvent> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        MSG_MOUSE_MOVE if data.len() >= 5 => {
            let x = u16::from_le_bytes([data[1], data[2]]);
            let y = u16::from_le_bytes([data[3], data[4]]);
            Some(ClientEvent::MouseMove { x, y })
        }
        MSG_MOUSE_BUTTON if data.len() >= 7 => {
            let button = data[1];
            let pressed = data[2] != 0;
            let x = u16::from_le_bytes([data[3], data[4]]);
            let y = u16::from_le_bytes([data[5], data[6]]);
            Some(ClientEvent::MouseButton { button, pressed, x, y })
        }
        MSG_MOUSE_SCROLL if data.len() >= 9 => {
            let dx = i16::from_le_bytes([data[1], data[2]]);
            let dy = i16::from_le_bytes([data[3], data[4]]);
            let x = u16::from_le_bytes([data[5], data[6]]);
            let y = u16::from_le_bytes([data[7], data[8]]);
            Some(ClientEvent::MouseScroll { dx, dy, x, y })
        }
        MSG_KEY_EVENT if data.len() >= 6 => {
            let keycode = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
            let pressed = data[5] != 0;
            Some(ClientEvent::KeyEvent { keycode, pressed })
        }
        MSG_CLIENT_READY => Some(ClientEvent::ClientReady),
        MSG_PASTE_TEXT if data.len() >= 2 => {
            let text = String::from_utf8(data[1..].to_vec()).ok()?;
            Some(ClientEvent::PasteText { text })
        }
        MSG_RELEASE_KEYS => Some(ClientEvent::ReleaseKeys),
        MSG_RELEASE_MOUSE => Some(ClientEvent::ReleaseMouse),
        MSG_RELEASE_ALL => Some(ClientEvent::ReleaseAll),
        MSG_CLIPBOARD_DATA => {
            let text = String::from_utf8_lossy(data.get(1..).unwrap_or_default()).into_owned();
            Some(ClientEvent::ClipboardData { text })
        }
        MSG_REQUEST_KEYFRAME => Some(ClientEvent::RequestKeyframe),
        MSG_CLIENT_CAPS => {
            let caps = serde_json::from_slice::<ClientCaps>(data.get(1..)?).ok()?;
            Some(ClientEvent::Caps(caps))
        }
        MSG_CLIENT_STATS => {
            let stats = serde_json::from_slice::<ClientStats>(data.get(1..)?).ok()?;
            Some(ClientEvent::Stats(stats))
        }
        MSG_PING => Some(ClientEvent::Ping { payload: data.to_vec() }),
        _ => None,
    }
}
