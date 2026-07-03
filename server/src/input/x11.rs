use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::protocol::ClientEvent;

/// X11 event-type constants used with XTest's `fake_input`.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

pub struct X11InputInjector {
    conn: RustConnection,
    root: u32,
}

impl X11InputInjector {
    /// Connect to the X11 server.
    pub fn new() -> anyhow::Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let root = conn.setup().roots[screen_num].root;

        tracing::info!("X11 input injector ready (root=0x{:x})", root);

        Ok(Self { conn, root })
    }

    /// Best-effort reset of common modifiers/buttons that may have been left
    /// logically pressed by an interrupted remote session.
    pub fn release_stuck_inputs(&self) -> anyhow::Result<()> {
        self.release_all_keys()?;
        self.release_all_mouse_buttons()?;
        Ok(())
    }

    pub fn release_all_keys(&self) -> anyhow::Result<()> {
        // Common browser keyCodes for modifiers, locks, navigation and text editing.
        for keycode in [
            8_u32, 9, 13, 16, 17, 18, 20, 27, 32, 33, 34, 35, 36, 37, 38, 39, 40, 45, 46, 91, 92, 93,
            144, 145,
        ] {
            let _ = self.send_key(keycode, false);
        }

        for keycode in 48_u32..=57 {
            let _ = self.send_key(keycode, false);
        }
        for keycode in 65_u32..=90 {
            let _ = self.send_key(keycode, false);
        }
        for keycode in 96_u32..=111 {
            let _ = self.send_key(keycode, false);
        }
        for keycode in 112_u32..=123 {
            let _ = self.send_key(keycode, false);
        }
        for keycode in 186_u32..=192 {
            let _ = self.send_key(keycode, false);
        }
        for keycode in 219_u32..=222 {
            let _ = self.send_key(keycode, false);
        }

        self.conn.flush()?;
        Ok(())
    }

    pub fn release_all_mouse_buttons(&self) -> anyhow::Result<()> {
        for button in [0_u8, 1, 2, 3, 4] {
            let _ = self.send_button(button, false);
        }

        self.conn.flush()?;
        Ok(())
    }

    /// Dispatch a client input event to the appropriate X11 injection method.
    ///
    /// Batches all X11 requests and issues a single flush at the end.
    pub fn inject_event(&self, event: &ClientEvent) -> anyhow::Result<()> {
        match event {
            ClientEvent::MouseMove { x, y } => self.send_move(*x, *y)?,
            ClientEvent::MouseButton {
                button,
                pressed,
                x,
                y,
            } => {
                self.send_move(*x, *y)?;
                self.send_button(*button, *pressed)?;
            }
            ClientEvent::MouseScroll { dx, dy, x, y } => {
                self.send_move(*x, *y)?;
                self.send_scroll(*dx, *dy)?;
            }
            ClientEvent::KeyEvent { keycode, pressed } => self.send_key(*keycode, *pressed)?,
            ClientEvent::ClientReady => return Ok(()),
            ClientEvent::PasteText { text } => {
                self.paste_text(text)?;
                return Ok(()); // paste_text manages its own flushes
            }
            ClientEvent::ReleaseKeys => {
                self.release_all_keys()?;
                return Ok(());
            }
            ClientEvent::ReleaseMouse => {
                self.release_all_mouse_buttons()?;
                return Ok(());
            }
            ClientEvent::ReleaseAll => {
                self.release_stuck_inputs()?;
                return Ok(());
            }
            ClientEvent::ClipboardData { .. } => {
                // Handled by the WebSocket layer; never forwarded to the input injector.
                return Ok(());
            }
            ClientEvent::RequestKeyframe { .. } => {
                // Handled by spawn_input_handler before reaching inject_event.
                return Ok(());
            }
            ClientEvent::Caps(_)
            | ClientEvent::Stats(_)
            | ClientEvent::Ping { .. }
            | ClientEvent::RequestAddMonitor
            | ClientEvent::RequestRemoveMonitor => {
                // Handled by the WebSocket layer; never forwarded to the input injector.
                return Ok(());
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    // -
    // Private helpers
    // -

    /// Send a motion event without flushing.
    fn send_move(&self, x: u16, y: u16) -> anyhow::Result<()> {
        xtest::fake_input(
            &self.conn,
            MOTION_NOTIFY,
            0,
            0,
            self.root,
            x as i16,
            y as i16,
            0,
        )?;
        Ok(())
    }

    /// Send a button press/release without flushing.
    fn send_button(&self, js_button: u8, pressed: bool) -> anyhow::Result<()> {
        let x11_button = match js_button {
            0 => 1,
            1 => 2,
            2 => 3,
            3 => 8,
            4 => 9,
            other => other + 1,
        };
        let event_type = if pressed { BUTTON_PRESS } else { BUTTON_RELEASE };
        xtest::fake_input(&self.conn, event_type, x11_button, 0, self.root, 0, 0, 0)?;
        Ok(())
    }

    /// Send scroll events without flushing.
    fn send_scroll(&self, dx: i16, dy: i16) -> anyhow::Result<()> {
        if dy != 0 {
            let button: u8 = if dy < 0 { 4 } else { 5 };
            let clicks = (dy.unsigned_abs() as u16).max(1);
            for _ in 0..clicks {
                xtest::fake_input(&self.conn, BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                xtest::fake_input(&self.conn, BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)?;
            }
        }
        if dx != 0 {
            let button: u8 = if dx < 0 { 6 } else { 7 };
            let clicks = (dx.unsigned_abs() as u16).max(1);
            for _ in 0..clicks {
                xtest::fake_input(&self.conn, BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                xtest::fake_input(&self.conn, BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)?;
            }
        }
        Ok(())
    }

    /// Send a key press/release without flushing.
    fn send_key(&self, js_keycode: u32, pressed: bool) -> anyhow::Result<()> {
        let x11_keycode = match self.js_keycode_to_x11_keycode(js_keycode) {
            Some(kc) => kc,
            None => {
                tracing::warn!("No X11 keycode for JS keyCode {}", js_keycode);
                return Ok(());
            }
        };
        let event_type = if pressed { KEY_PRESS } else { KEY_RELEASE };
        xtest::fake_input(&self.conn, event_type, x11_keycode, 0, self.root, 0, 0, 0)?;
        Ok(())
    }

    /// Paste text by setting the X11 CLIPBOARD selection and simulating Ctrl+V.
    ///
    /// This is keyboard-layout independent — the text goes through the clipboard
    /// rather than being typed as individual keypresses.
    fn paste_text(&self, text: &str) -> anyhow::Result<()> {
        self.release_stuck_inputs()?;

        // Open a separate connection for clipboard ownership so we can serve
        // SelectionRequest events without interfering with the main connection.
        let text = text.to_string();
        let display = std::env::var("DISPLAY").ok();

        // Spawn a thread that owns the clipboard and serves requests for ~5 seconds
        let handle = std::thread::spawn(move || -> anyhow::Result<()> {
            let (conn, screen_num) = if let Some(d) = &display {
                unsafe { std::env::set_var("DISPLAY", d) };
                x11rb::connect(None)?
            } else {
                x11rb::connect(None)?
            };
            let screen = &conn.setup().roots[screen_num];

            let window = conn.generate_id()?;
            conn.create_window(
                0, window, screen.root,
                0, 0, 1, 1, 0,
                x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &x11rb::protocol::xproto::CreateWindowAux::new(),
            )?.check()?;

            let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
            let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
            let targets_atom = conn.intern_atom(false, b"TARGETS")?.reply()?.atom;

            // Take ownership of CLIPBOARD
            conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)?.check()?;
            conn.flush()?;

            // Verify we got ownership
            let owner = conn.get_selection_owner(clipboard)?.reply()?.owner;
            if owner != window {
                anyhow::bail!("Failed to acquire CLIPBOARD ownership");
            }

            let text_bytes = text.as_bytes();
            let deadline = Instant::now() + Duration::from_secs(5);

            // Serve SelectionRequest events until timeout
            while Instant::now() < deadline {
                let event = conn.poll_for_event()?;
                match event {
                    Some(Event::SelectionRequest(req)) => {
                        let mut notify = x11rb::protocol::xproto::SelectionNotifyEvent {
                            response_type: 31, // SelectionNotify
                            sequence: 0,
                            time: req.time,
                            requestor: req.requestor,
                            selection: req.selection,
                            target: req.target,
                            property: req.property,
                        };

                        if req.target == targets_atom {
                            // Respond with supported targets
                            let targets = [utf8_string, targets_atom];
                            conn.change_property32(
                                x11rb::protocol::xproto::PropMode::REPLACE,
                                req.requestor,
                                req.property,
                                x11rb::protocol::xproto::AtomEnum::ATOM,
                                &targets,
                            )?;
                        } else if req.target == utf8_string {
                            // Respond with the text
                            conn.change_property8(
                                x11rb::protocol::xproto::PropMode::REPLACE,
                                req.requestor,
                                req.property,
                                utf8_string,
                                text_bytes,
                            )?;
                        } else {
                            // Unsupported target
                            notify.property = x11rb::NONE;
                        }

                        conn.send_event(false, req.requestor, x11rb::protocol::xproto::EventMask::NO_EVENT, notify)?;
                        conn.flush()?;
                    }
                    Some(Event::SelectionClear(_)) => {
                        // Another app took clipboard ownership, we're done
                        break;
                    }
                    _ => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }

            let _ = conn.destroy_window(window);
            let _ = conn.flush();
            Ok(())
        });

        // Give the clipboard thread a moment to acquire ownership
        std::thread::sleep(Duration::from_millis(50));

        // Simulate Ctrl+V
        self.send_key(17, true)?;  // Ctrl down
        self.send_key(86, true)?;  // V down
        self.send_key(86, false)?; // V up
        self.send_key(17, false)?; // Ctrl up
        self.conn.flush()?;

        // Detach the clipboard thread — it'll serve requests for a few seconds then exit
        drop(handle);

        Ok(())
    }

    fn js_keycode_to_x11_keycode(&self, js_key: u32) -> Option<u8> {
        // Map JS keycodes (physical/position-based) directly to X11 hardware
        // keycodes (evdev + 8).  This is layout-independent: the same physical
        // key always maps to the same X11 keycode regardless of whether the
        // server uses a US, German, French, or any other layout.  The server's
        // X11 layout then translates the hardware keycode to the correct
        // character for the user.
        super::keymap::js_keycode_to_x11_hw(js_key)
    }
}

impl super::InputInjector for X11InputInjector {
    fn inject_event(&mut self, event: &ClientEvent) -> anyhow::Result<()> {
        X11InputInjector::inject_event(self, event)
    }

    fn release_stuck_inputs(&mut self) -> anyhow::Result<()> {
        X11InputInjector::release_stuck_inputs(self)
    }
}

pub fn read_selection_text(selection_name: &str) -> anyhow::Result<Option<String>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let window = conn.generate_id()?;

    conn.create_window(
        0,
        window,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &x11rb::protocol::xproto::CreateWindowAux::new(),
    )?
    .check()?;

    let selection_atom = intern_atom(&conn, selection_name)?;
    let utf8_atom = intern_atom(&conn, "UTF8_STRING")?;
    let string_atom = intern_atom(&conn, "STRING")?;
    let property_atom = intern_atom(&conn, "DDISPLAY_SELECTION")?;

    let result = read_selection_with_target(&conn, window, selection_atom, utf8_atom, property_atom)?
        .or_else(|| read_selection_with_target(&conn, window, selection_atom, string_atom, property_atom).ok().flatten());

    let _ = conn.destroy_window(window);
    let _ = conn.flush();

    Ok(result)
}

fn intern_atom(conn: &RustConnection, name: &str) -> anyhow::Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

fn read_selection_with_target(
    conn: &RustConnection,
    window: u32,
    selection_atom: u32,
    target_atom: u32,
    property_atom: u32,
) -> anyhow::Result<Option<String>> {
    conn.convert_selection(window, selection_atom, target_atom, property_atom, x11rb::CURRENT_TIME)?
        .check()?;
    conn.flush()?;

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if let Some(event) = conn.poll_for_event()? {
            if let Event::SelectionNotify(notify) = event {
                if notify.requestor != window {
                    continue;
                }
                if notify.property == x11rb::NONE {
                    return Ok(None);
                }
                let reply = conn
                    .get_property(false, window, property_atom, target_atom, 0, u32::MAX)?
                    .reply()?;
                if reply.value_len == 0 {
                    return Ok(Some(String::new()));
                }
                return Ok(Some(String::from_utf8_lossy(&reply.value).into_owned()));
            }
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    Ok(None)
}
