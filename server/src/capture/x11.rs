use anyhow::{Context, Result};
use std::ptr;
use x11rb::connection::Connection;
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
    width: u16,
    height: u16,
    shm_seg: u32,
    #[allow(dead_code)]
    shm_id: i32,
    shm_ptr: *mut u8,
    shm_size: usize,
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

        tracing::info!(
            "Connected to X11 display, screen {}: {}x{}",
            screen_num,
            width,
            height
        );

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
            width,
            height,
            shm_seg,
            shm_id,
            shm_ptr,
            shm_size,
        })
    }

    /// Capture a full-screen frame into SHM and return a zero-copy reference.
    ///
    /// The returned `CapturedFrameRef` borrows the SHM buffer directly — no
    /// 8 MB copy. The data is valid until the next call to `capture_frame_ref`.
    pub fn capture_frame_ref(&self) -> Result<CapturedFrameRef<'_>> {
        self.conn
            .shm_get_image(
                self.root,
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
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
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
