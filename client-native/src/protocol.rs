/// Wire protocol for ddisplay server <-> client communication.
///
/// Mirrors the server's protocol.rs. All messages are binary WebSocket frames.
///
/// Server -> Client:
///   0x01 VideoFrame: [keyframe: u8] [pts: u64 LE] [width: u16 LE] [height: u16 LE] [data...]
///   0x02 CursorUpdate: [x: u16 LE] [y: u16 LE] [visible: u8]
///   0x20 ClipboardData: [utf8 text...]   (bidirectional)
///
/// Client -> Server:
///   0x10 MouseMove: [x: u16 LE] [y: u16 LE]
///   0x11 MouseButton: [button: u8] [pressed: u8] [x: u16 LE] [y: u16 LE]
///   0x12 MouseScroll: [dx: i16 LE] [dy: i16 LE] [x: u16 LE] [y: u16 LE]
///   0x13 KeyEvent: [keycode: u32 LE] [pressed: u8]
///   0x14 ClientReady: (no payload)
///   0x15 PasteText: [utf8 text...]
///   0x16 ReleaseKeys
///   0x17 ReleaseMouse
///   0x18 ReleaseAll
///   0x20 ClipboardData: [utf8 text...]   (bidirectional)

// Server message types
pub const MSG_VIDEO_FRAME: u8 = 0x01;
pub const MSG_CURSOR_UPDATE: u8 = 0x02;

// Client message types
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

/// Parsed video frame from the server.
pub struct VideoFrame<'a> {
    pub keyframe: bool,
    pub pts: u64,
    pub width: u16,
    pub height: u16,
    pub data: &'a [u8],
}

/// Parsed cursor update from the server.
pub struct CursorUpdate {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

pub const MSG_SESSION_INFO: u8 = 0x03;

/// Parse a server message.
pub enum ServerMessage<'a> {
    VideoFrame(VideoFrame<'a>),
    CursorUpdate(CursorUpdate),
    SessionInfo(&'a [u8]),
    ClipboardData(String),
    Unknown(u8),
}

pub fn parse_server_message(data: &[u8]) -> Option<ServerMessage<'_>> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        MSG_VIDEO_FRAME if data.len() >= 14 => {
            let keyframe = data[1] != 0;
            let pts = u64::from_le_bytes(data[2..10].try_into().ok()?);
            let width = u16::from_le_bytes(data[10..12].try_into().ok()?);
            let height = u16::from_le_bytes(data[12..14].try_into().ok()?);
            Some(ServerMessage::VideoFrame(VideoFrame {
                keyframe,
                pts,
                width,
                height,
                data: &data[14..],
            }))
        }
        MSG_CURSOR_UPDATE if data.len() >= 6 => {
            let x = u16::from_le_bytes(data[1..3].try_into().ok()?);
            let y = u16::from_le_bytes(data[3..5].try_into().ok()?);
            let visible = data[5] != 0;
            Some(ServerMessage::CursorUpdate(CursorUpdate { x, y, visible }))
        }
        MSG_SESSION_INFO if data.len() >= 2 => {
            Some(ServerMessage::SessionInfo(&data[1..]))
        }
        MSG_CLIPBOARD_DATA => {
            let text = String::from_utf8_lossy(data.get(1..).unwrap_or_default()).into_owned();
            Some(ServerMessage::ClipboardData(text))
        }
        other => Some(ServerMessage::Unknown(other)),
    }
}

// --- Client message encoders ---

pub fn encode_client_ready() -> Vec<u8> {
    vec![MSG_CLIENT_READY]
}

pub fn encode_mouse_move(x: u16, y: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5);
    buf.push(MSG_MOUSE_MOVE);
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf
}

pub fn encode_mouse_button(button: u8, pressed: bool, x: u16, y: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(7);
    buf.push(MSG_MOUSE_BUTTON);
    buf.push(button);
    buf.push(pressed as u8);
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf
}

pub fn encode_mouse_scroll(dx: i16, dy: i16, x: u16, y: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9);
    buf.push(MSG_MOUSE_SCROLL);
    buf.extend_from_slice(&dx.to_le_bytes());
    buf.extend_from_slice(&dy.to_le_bytes());
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf
}

pub fn encode_key_event(keycode: u32, pressed: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(6);
    buf.push(MSG_KEY_EVENT);
    buf.extend_from_slice(&keycode.to_le_bytes());
    buf.push(pressed as u8);
    buf
}

pub fn encode_paste_text(text: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + text.len());
    buf.push(MSG_PASTE_TEXT);
    buf.extend_from_slice(text.as_bytes());
    buf
}

pub fn encode_release_all() -> Vec<u8> {
    vec![MSG_RELEASE_ALL]
}

pub fn encode_clipboard_data(text: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + text.len());
    buf.push(MSG_CLIPBOARD_DATA);
    buf.extend_from_slice(text.as_bytes());
    buf
}

pub fn encode_request_keyframe() -> Vec<u8> {
    vec![MSG_REQUEST_KEYFRAME]
}
