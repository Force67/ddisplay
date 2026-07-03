pub mod wayland;
pub mod x11;

/// Backend-agnostic screen capture.
///
/// Implemented by the X11 (MIT-SHM) and Wayland (Mutter ScreenCast/PipeWire)
/// backends. The capture loop drives this through a `Box<dyn ScreenCapturer>`.
pub trait ScreenCapturer: Send {
    /// Current capture width in pixels.
    fn width(&self) -> u32;
    /// Current capture height in pixels.
    fn height(&self) -> u32;
    /// Whether the screen content changed since the last `frame_ref()`.
    /// Backends without change tracking return `true` (capture every frame).
    fn has_new_frame(&mut self) -> bool;
    /// Borrow the latest frame (BGRA/BGRx, 4 bytes per pixel, row stride in
    /// bytes). Valid until the next call on this capturer.
    fn frame_ref(&mut self) -> anyhow::Result<CapturedFrameRef<'_>>;
    /// Current cursor position. Only used when `embeds_cursor()` is false.
    fn cursor_info(&mut self) -> anyhow::Result<CursorInfo>;
    /// Returns the new size when the captured display changed resolution.
    /// The caller should then call `reinit()` and rebuild the encoder.
    fn size_changed(&mut self) -> Option<(u32, u32)>;
    /// Re-initialize after a resolution change reported by `size_changed()`.
    /// Backends that adopt the new size internally make this a no-op.
    fn reinit(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// True when the cursor is composited into captured frames (Wayland
    /// EMBEDDED cursor mode) — separate cursor updates are skipped then.
    fn embeds_cursor(&self) -> bool;
    /// What changed since the last call (cleared on read). Lets the encode
    /// loop skip heads whose region is untouched. Backends without region
    /// tracking keep the default: everything may have changed.
    fn take_damage_hint(&mut self) -> DamageHint {
        DamageHint::Unknown
    }
}

/// Summary of which screen region changed since the previous captured frame.
#[derive(Debug, Clone, Copy)]
pub enum DamageHint {
    /// No region information — treat the whole frame as dirty.
    Unknown,
    /// All changes fall inside this bounding box.
    Bbox { x: u32, y: u32, width: u32, height: u32 },
}

impl DamageHint {
    /// Whether the damaged region touches the given rectangle.
    pub fn intersects(&self, rx: u32, ry: u32, rw: u32, rh: u32) -> bool {
        match *self {
            DamageHint::Unknown => true,
            DamageHint::Bbox { x, y, width, height } => {
                x < rx + rw && rx < x + width && y < ry + rh && ry < y + height
            }
        }
    }
}

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

/// A zero-copy reference to pixel data in shared memory.
pub struct CapturedFrameRef<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Cursor position information.
#[derive(Debug, Clone, Copy)]
pub struct CursorInfo {
    pub x: i16,
    pub y: i16,
    pub visible: bool,
}
