use anyhow::{Context, Result};
use std::ptr;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::damage;
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self};
use x11rb::protocol::xproto::{self, ImageFormat};
use x11rb::rust_connection::RustConnection;

use super::{CapturedFrame, CapturedFrameRef, CursorInfo};

/// X11 screen capturer using MIT-SHM for zero-copy frame grabs.
///
/// Uses System V shared memory to share a buffer between this process
/// and the X server, enabling fast full-screen captures without copying
/// pixel data over the socket.
pub struct X11Capturer {
    conn: RustConnection,
    #[allow(dead_code)]
    screen_num: usize,
    root: u32,
    capture_window: u32,
    overlay_window: Option<u32>,
    width: u16,
    height: u16,
    shm_seg: u32,
    #[allow(dead_code)]
    shm_id: i32,
    shm_ptr: *mut u8,
    shm_size: usize,
    /// X11 Damage object for change tracking (None if damage init failed).
    damage_id: Option<u32>,
    /// Whether the screen has been damaged since last capture.
    damaged: bool,
    /// Union of DamageNotify areas since the last `take_damage_hint()`,
    /// as (left, top, right, bottom) edges.
    damage_bbox: Option<(u32, u32, u32, u32)>,
    /// Set when a non-damage event arrived — region info is then unreliable
    /// and the next hint falls back to "everything changed".
    damage_unknown: bool,
}

// SAFETY: The shared memory pointer is only accessed from capture_frame()
// which takes &self. The X11 connection (RustConnection) is Send-safe and
// we serialize access at a higher level.
unsafe impl Send for X11Capturer {}

impl X11Capturer {
    /// Connect to the X11 display, initialize SHM, and prepare for capture.
    ///
    /// This will:
    /// 1. Connect to the default X display (from $DISPLAY)
    /// 2. Query the root window dimensions
    /// 3. Verify MIT-SHM extension support
    /// 4. Allocate a shared memory segment sized for one full frame (BGRA)
    /// 5. Attach the segment to the X server
    /// 6. Initialize the XFixes extension (for cursor queries)
    pub fn new() -> Result<Self> {
        // Connect to X11 display
        let (conn, screen_num) = x11rb::connect(None)
            .context("Failed to connect to X11 display. Is $DISPLAY set?")?;

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let width = screen.width_in_pixels;
        let height = screen.height_in_pixels;
        let overlay_window = Self::detect_overlay_window(&conn, root)?;
        let capture_window = overlay_window.unwrap_or(root);

        tracing::info!(
            "Connected to X11 display, screen {}: {}x{}",
            screen_num,
            width,
            height
        );
        if let Some(overlay_window) = overlay_window {
            tracing::info!(
                "Using XComposite overlay window 0x{:x} for capture (root=0x{:x})",
                overlay_window,
                root
            );
        } else {
            tracing::info!("Using root window 0x{:x} for capture", root);
        }

        // Verify MIT-SHM extension is available
        let shm_version = shm::query_version(&conn)?
            .reply()
            .context("X server does not support MIT-SHM extension")?;

        tracing::debug!(
            "MIT-SHM version {}.{}, shared pixmaps: {}",
            shm_version.major_version,
            shm_version.minor_version,
            shm_version.shared_pixmaps
        );

        // Allocate System V shared memory for one full BGRA frame
        let shm_size = (width as usize) * (height as usize) * 4;

        let shm_id = unsafe {
            libc::shmget(
                libc::IPC_PRIVATE,
                shm_size,
                libc::IPC_CREAT | 0o600,
            )
        };
        if shm_id < 0 {
            anyhow::bail!(
                "shmget failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let shm_ptr = unsafe { libc::shmat(shm_id, ptr::null(), 0) };
        if shm_ptr == (-1_isize) as *mut libc::c_void {
            // Clean up the segment we just created
            unsafe { libc::shmctl(shm_id, libc::IPC_RMID, ptr::null_mut()) };
            anyhow::bail!(
                "shmat failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let shm_ptr = shm_ptr as *mut u8;

        // Generate an X11 resource ID for the SHM segment
        let shm_seg = conn.generate_id()
            .context("Failed to generate X11 ID for SHM segment")?;

        // Attach the shared memory segment to the X server
        conn.shm_attach(shm_seg, shm_id as u32, false)?
            .check()
            .context("Failed to attach SHM segment to X server")?;

        // Mark the segment for deletion once all processes detach.
        // The segment stays alive as long as at least one process (us or the X server)
        // has it attached. This ensures cleanup even if we crash.
        unsafe {
            libc::shmctl(shm_id, libc::IPC_RMID, ptr::null_mut());
        }

        // Initialize XFixes extension (needed for cursor queries)
        xfixes::query_version(&conn, 4, 0)?
            .reply()
            .context("X server does not support XFixes extension v4+")?;

        // Initialize X11 Damage extension for change tracking
        let damage_id = Self::init_damage(&conn, capture_window).ok();
        if damage_id.is_some() {
            tracing::info!("X11 Damage tracking enabled");
        } else {
            tracing::warn!("X11 Damage tracking unavailable, capturing every frame");
        }

        tracing::info!(
            "X11 capturer initialized: {}x{}, SHM segment {} ({} bytes)",
            width,
            height,
            shm_seg,
            shm_size
        );

        Ok(Self {
            conn,
            screen_num,
            root,
            capture_window,
            overlay_window,
            width,
            height,
            shm_seg,
            shm_id,
            shm_ptr,
            shm_size,
            damage_id,
            damaged: true, // assume damaged initially so first frame is captured
            damage_bbox: None,
            damage_unknown: true, // no region info for that first frame either
        })
    }

    /// Capture a full-screen frame into SHM and return a zero-copy reference.
    ///
    /// The returned `CapturedFrameRef` borrows the SHM buffer directly — no
    /// 8 MB copy. The data is valid until the next call to `capture_frame_ref`.
    pub fn capture_frame_ref(&self) -> Result<CapturedFrameRef<'_>> {
        self.conn
            .shm_get_image(
                self.capture_window,
                0,
                0,
                self.width,
                self.height,
                0xFFFFFFFF,
                ImageFormat::Z_PIXMAP.into(),
                self.shm_seg,
                0,
            )?
            .reply()
            .context("shm_get_image failed")?;

        let data = unsafe {
            std::slice::from_raw_parts(self.shm_ptr, self.shm_size)
        };

        Ok(CapturedFrameRef {
            data,
            width: self.width as u32,
            height: self.height as u32,
            stride: self.width as u32 * 4,
        })
    }

    /// Capture a full-screen frame using MIT-SHM (allocating copy).
    pub fn capture_frame(&self) -> Result<CapturedFrame> {
        let frame_ref = self.capture_frame_ref()?;
        Ok(CapturedFrame {
            data: frame_ref.data.to_vec(),
            width: frame_ref.width,
            height: frame_ref.height,
            stride: frame_ref.stride,
        })
    }

    /// Query the current cursor position.
    ///
    /// Uses XQueryPointer to get the cursor coordinates relative to the root window.
    /// The `visible` field is true when the cursor is on the same screen.
    pub fn get_cursor_info(&self) -> Result<CursorInfo> {
        let reply = xproto::query_pointer(&self.conn, self.root)?
            .reply()
            .context("query_pointer failed")?;

        Ok(CursorInfo {
            x: reply.root_x,
            y: reply.root_y,
            visible: reply.same_screen,
        })
    }

    /// Screen width in pixels.
    pub fn screen_width(&self) -> u32 {
        self.width as u32
    }

    /// Screen height in pixels.
    pub fn screen_height(&self) -> u32 {
        self.height as u32
    }

    /// Query the X server for the root window's current size.
    ///
    /// Returns `Some((w, h))` when it differs from the size this capturer was
    /// initialized with (e.g. after a RandR resize) — the caller should then
    /// rebuild the capturer (and the encoder). One round-trip; intended to be
    /// polled around once per second, not per frame.
    pub fn screen_size_changed(&self) -> Option<(u32, u32)> {
        let geom = xproto::get_geometry(&self.conn, self.root).ok()?.reply().ok()?;
        if geom.width != self.width || geom.height != self.height {
            Some((geom.width as u32, geom.height as u32))
        } else {
            None
        }
    }

    /// Poll for damage events and return whether the screen has changed.
    ///
    /// Call this before `capture_frame_ref()`. If it returns `false`, the
    /// screen is unchanged and you can skip capture+encode.
    pub fn has_damage(&mut self) -> bool {
        if self.damage_id.is_none() {
            return true; // no damage tracking, always capture
        }

        // Drain all pending events. DamageNotify carries the damage region's
        // bounding box (BOUNDING_BOX report level) — union them so the encode
        // loop can skip heads whose rect the damage never touched.
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            match event {
                x11rb::protocol::Event::DamageNotify(e) => {
                    self.damaged = true;
                    let (ax, ay) = (e.area.x.max(0) as u32, e.area.y.max(0) as u32);
                    let (ar, ab) = (ax + e.area.width as u32, ay + e.area.height as u32);
                    self.damage_bbox = Some(match self.damage_bbox {
                        None => (ax, ay, ar, ab),
                        Some((x0, y0, x1, y1)) => {
                            (x0.min(ax), y0.min(ay), x1.max(ar), y1.max(ab))
                        }
                    });
                }
                // Anything else is unexpected on this connection; be
                // conservative and treat the whole frame as changed.
                _ => {
                    self.damaged = true;
                    self.damage_unknown = true;
                }
            }
        }

        let was_damaged = self.damaged;
        if was_damaged {
            self.damaged = false;
            // Subtract the damage region so we get notified of future changes
            if let Some(dmg) = self.damage_id {
                let _ = damage::subtract(&self.conn, dmg, 0u32, 0u32);
                let _ = self.conn.flush();
            }
        }
        was_damaged
    }

    fn init_damage(conn: &RustConnection, window: u32) -> Result<u32> {
        let version = damage::query_version(conn, 1, 1)?
            .reply()
            .context("X server does not support Damage extension v1.1+")?;

        tracing::debug!(
            "X11 Damage version {}.{}",
            version.major_version,
            version.minor_version,
        );

        let damage_id = conn.generate_id()
            .context("Failed to generate X11 ID for Damage")?;

        // BoundingBox report level: one event each time the damage region's
        // bounding box grows (not per-rect), carrying the box — enough to
        // tell which monitor heads a change touched without an event flood.
        damage::create(conn, damage_id, window, damage::ReportLevel::BOUNDING_BOX)?
            .check()
            .context("Failed to create Damage object")?;

        Ok(damage_id)
    }

    fn detect_overlay_window(conn: &RustConnection, root: u32) -> Result<Option<u32>> {
        let version = conn
            .composite_query_version(0, 4)?
            .reply()
            .context("X server does not support XComposite extension v0.4+")?;

        tracing::debug!(
            "XComposite version {}.{}",
            version.major_version,
            version.minor_version
        );

        let reply = conn
            .composite_get_overlay_window(root)?
            .reply()
            .context("Failed to query XComposite overlay window")?;

        let overlay_window = reply.overlay_win;
        if overlay_window == 0 || overlay_window == root {
            return Ok(None);
        }

        Ok(Some(overlay_window))
    }
}

impl super::ScreenCapturer for X11Capturer {
    fn width(&self) -> u32 {
        self.screen_width()
    }

    fn height(&self) -> u32 {
        self.screen_height()
    }

    fn has_new_frame(&mut self) -> bool {
        self.has_damage()
    }

    fn frame_ref(&mut self) -> Result<CapturedFrameRef<'_>> {
        self.capture_frame_ref()
    }

    fn cursor_info(&mut self) -> Result<CursorInfo> {
        self.get_cursor_info()
    }

    fn size_changed(&mut self) -> Option<(u32, u32)> {
        self.screen_size_changed()
    }

    fn reinit(&mut self) -> Result<()> {
        // The SHM segment is sized for the old resolution — rebuild from scratch.
        *self = X11Capturer::new()?;
        Ok(())
    }

    fn embeds_cursor(&self) -> bool {
        false
    }

    fn take_damage_hint(&mut self) -> super::DamageHint {
        let unknown = self.damage_unknown || self.damage_id.is_none();
        self.damage_unknown = false;
        let bbox = self.damage_bbox.take();
        match bbox {
            Some((x0, y0, x1, y1)) if !unknown => super::DamageHint::Bbox {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            },
            // No region info (tracking unavailable, a foreign event, or a
            // damaged flag set without events) — assume everything changed.
            _ => super::DamageHint::Unknown,
        }
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        if let Some(dmg) = self.damage_id {
            let _ = damage::destroy(&self.conn, dmg);
        }

        if self.overlay_window.is_some() {
            if let Err(e) = self.conn.composite_release_overlay_window(self.root) {
                tracing::warn!("Failed to release XComposite overlay window: {}", e);
            }
        }

        // Detach from X server first (ignore errors during cleanup)
        if let Err(e) = self.conn.shm_detach(self.shm_seg) {
            tracing::warn!("Failed to detach SHM from X server: {}", e);
        }

        // Detach shared memory from our process address space
        unsafe {
            libc::shmdt(self.shm_ptr as *const libc::c_void);
        }

        tracing::debug!("X11 capturer cleaned up");
    }
}
