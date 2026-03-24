pub mod x11;

/// A captured frame from the display.
#[derive(Debug)]
pub struct CapturedFrame {
    /// Raw pixel data in BGRA format (X11 native)
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Bytes per row (may include padding)
    pub stride: u32,
}

/// Cursor position information.
#[derive(Debug, Clone, Copy)]
pub struct CursorInfo {
    pub x: i16,
    pub y: i16,
    pub visible: bool,
}
