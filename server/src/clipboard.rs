//! X11 clipboard monitor and persistent selection owner.
//!
//! Spawns a background thread that:
//!   - Subscribes to CLIPBOARD owner changes via XFixes
//!   - Reads the new content and forwards it to the frame broadcast channel
//!   - Takes and holds CLIPBOARD ownership when a client sets text

use std::sync::mpsc;
use std::time::{Duration, Instant};
use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::{
    xfixes,
    xproto::{self, ConnectionExt as XProtoExt, EventMask},
    Event,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::transport::websocket::FrameSender;

/// Start the clipboard background thread.
///
/// Returns a sender that the WebSocket handler uses to push client clipboard
/// text to the server (which then takes X11 CLIPBOARD ownership).
pub fn start(frame_tx: FrameSender) -> Result<mpsc::SyncSender<String>> {
    let (set_tx, set_rx) = mpsc::sync_channel::<String>(4);
    std::thread::Builder::new()
        .name("clipboard-monitor".into())
        .spawn(move || {
            if let Err(e) = run(frame_tx, set_rx) {
                tracing::error!("[clipboard] thread error: {}", e);
            }
        })
        .context("failed to spawn clipboard thread")?;
    Ok(set_tx)
}

struct X11Clipboard {
    conn: RustConnection,
    window: u32,
    clipboard: u32,
    utf8_string: u32,
    targets: u32,
    sel_prop: u32,
    /// Text we are currently owning (serving to other X11 clients).
    owned_text: Option<String>,
}

impl X11Clipboard {
    fn new() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("X11 clipboard: connect failed")?;
        let root = conn.setup().roots[screen_num].root;

        let wid = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_FROM_PARENT as u8,
            wid,
            root,
            -10,
            -10,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new(),
        )?
        .check()?;
        conn.flush()?;

        let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let targets = conn.intern_atom(false, b"TARGETS")?.reply()?.atom;
        let sel_prop = conn.intern_atom(false, b"_DDISPLAY_CB")?.reply()?.atom;

        Ok(Self {
            conn,
            window: wid,
            clipboard,
            utf8_string,
            targets,
            sel_prop,
            owned_text: None,
        })
    }

    fn enable_monitoring(&self) -> Result<()> {
        xfixes::query_version(&self.conn, 5, 0)?.reply()?;
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY;
        xfixes::select_selection_input(&self.conn, self.window, self.clipboard, mask)?.check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn take_ownership(&mut self, text: String) -> Result<()> {
        self.conn
            .set_selection_owner(self.window, self.clipboard, x11rb::CURRENT_TIME)?
            .check()?;
        self.conn.flush()?;
        self.owned_text = Some(text);
        Ok(())
    }

    /// Request the CLIPBOARD content from the current owner.
    /// Returns None on timeout or if clipboard is empty/unavailable.
    fn read_clipboard(&mut self) -> Option<String> {
        self.conn
            .convert_selection(
                self.window,
                self.clipboard,
                self.utf8_string,
                self.sel_prop,
                x11rb::CURRENT_TIME,
            )
            .ok()?
            .check()
            .ok()?;
        self.conn.flush().ok()?;

        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match self.conn.poll_for_event() {
                Ok(Some(Event::SelectionNotify(n))) => {
                    if n.requestor != self.window {
                        continue;
                    }
                    if n.property == x11rb::NONE {
                        return None;
                    }
                    let reply = self
                        .conn
                        .get_property(
                            false,
                            self.window,
                            self.sel_prop,
                            self.utf8_string,
                            0,
                            u32::MAX,
                        )
                        .ok()?
                        .reply()
                        .ok()?;
                    let _ = self.conn.delete_property(self.window, self.sel_prop);
                    if reply.value_len == 0 {
                        return None;
                    }
                    let text = String::from_utf8_lossy(&reply.value).into_owned();
                    return if text.is_empty() { None } else { Some(text) };
                }
                Ok(Some(other)) => {
                    let _ = self.handle_selection_request(other);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        None
    }

    fn handle_selection_request(&mut self, event: Event) -> Result<()> {
        let Event::SelectionRequest(req) = event else {
            return Ok(());
        };
        let owned = match &self.owned_text {
            Some(t) => t.clone(),
            None => return Ok(()),
        };

        // Per ICCCM: if property is NONE the requestor wants us to pick a name.
        let prop = if req.property == x11rb::NONE { req.target } else { req.property };

        if req.target == self.targets {
            self.conn.change_property32(
                xproto::PropMode::REPLACE,
                req.requestor,
                prop,
                xproto::AtomEnum::ATOM,
                &[self.utf8_string, self.targets],
            )?;
        } else if req.target == self.utf8_string {
            self.conn.change_property8(
                xproto::PropMode::REPLACE,
                req.requestor,
                prop,
                self.utf8_string,
                owned.as_bytes(),
            )?;
        } else {
            // Unsupported format — deny with property=NONE
            let notify = xproto::SelectionNotifyEvent {
                response_type: 31,
                sequence: 0,
                time: req.time,
                requestor: req.requestor,
                selection: req.selection,
                target: req.target,
                property: x11rb::NONE,
            };
            self.conn
                .send_event(false, req.requestor, EventMask::NO_EVENT, notify)?;
            self.conn.flush()?;
            return Ok(());
        }

        let notify = xproto::SelectionNotifyEvent {
            response_type: 31,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property: prop,
        };
        self.conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Poll pending X11 events. Returns true if an XFixes SelectionNotify
    /// arrived indicating a new owner (not us) took the CLIPBOARD.
    fn poll(&mut self) -> Result<bool> {
        let mut changed = false;
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::XfixesSelectionNotify(n) if n.owner != self.window => {
                    changed = true;
                }
                Event::SelectionClear(_) => {
                    self.owned_text = None;
                }
                other => {
                    let _ = self.handle_selection_request(other);
                }
            }
        }
        Ok(changed)
    }
}

fn run(frame_tx: FrameSender, set_rx: mpsc::Receiver<String>) -> Result<()> {
    let mut cb = X11Clipboard::new()?;
    cb.enable_monitoring()?;
    tracing::info!("[clipboard] monitor running");

    // Last text we sent to clients (avoids re-broadcasting our own ownership).
    let mut last_broadcast: Option<String> = None;

    loop {
        // Drain any set-clipboard requests from connected clients.
        loop {
            match set_rx.try_recv() {
                Ok(text) => {
                    tracing::debug!("[clipboard] client set {} chars", text.len());
                    last_broadcast = Some(text.clone());
                    let _ = cb.take_ownership(text);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        match cb.poll() {
            Ok(true) => {
                if let Some(text) = cb.read_clipboard() {
                    if last_broadcast.as_deref() != Some(&text) {
                        tracing::debug!(
                            "[clipboard] broadcasting {} chars to clients",
                            text.len()
                        );
                        last_broadcast = Some(text.clone());
                        let _ = frame_tx.send(crate::protocol::encode_clipboard_data(&text));
                    }
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("[clipboard] poll error: {}", e);
            }
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}
