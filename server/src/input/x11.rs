use std::collections::HashMap;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

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
    /// JS keyCode -> X11 hardware keycode.
    keymap: HashMap<u32, u8>,
}

impl X11InputInjector {
    /// Connect to the X11 server and build the key mapping table.
    pub fn new() -> anyhow::Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let root = conn.setup().roots[screen_num].root;

        // Build a mapping from JS keyCodes to X11 keycodes by reading the
        // server's full keyboard mapping and associating each keysym with
        // the corresponding hardware keycode.
        let keymap = build_keymap(&conn)?;

        tracing::info!(
            "X11 input injector ready (root=0x{:x}, {} key mappings)",
            root,
            keymap.len()
        );

        Ok(Self { conn, root, keymap })
    }

    /// Best-effort reset of common modifiers/buttons that may have been left
    /// logically pressed by an interrupted remote session.
    pub fn release_stuck_inputs(&self) -> anyhow::Result<()> {
        // Common browser keyCodes for modifiers and lock/meta keys.
        for keycode in [16_u32, 17, 18, 20, 91, 92, 93] {
            let _ = self.key_event(keycode, false);
        }

        // Common mouse buttons a remote client may have left pressed.
        for button in [0_u8, 1, 2, 3, 4] {
            let _ = self.mouse_button(button, false);
        }

        self.conn.flush()?;
        Ok(())
    }

    /// Dispatch a client input event to the appropriate X11 injection method.
    pub fn inject_event(&self, event: &ClientEvent) -> anyhow::Result<()> {
        match event {
            ClientEvent::MouseMove { x, y } => self.move_mouse(*x, *y),
            ClientEvent::MouseButton {
                button,
                pressed,
                x,
                y,
            } => {
                // Move to the position first, then press/release.
                self.move_mouse(*x, *y)?;
                self.mouse_button(*button, *pressed)
            }
            ClientEvent::MouseScroll { dx, dy, x, y } => {
                self.move_mouse(*x, *y)?;
                self.mouse_scroll(*dx, *dy)
            }
            ClientEvent::KeyEvent { keycode, pressed } => self.key_event(*keycode, *pressed),
            ClientEvent::ClientReady => Ok(()), // informational only
            ClientEvent::PasteText { text } => self.paste_text(text),
        }
    }

    // ------------------------------------------------------------------
    // Private helpers
    // ------------------------------------------------------------------

    fn move_mouse(&self, x: u16, y: u16) -> anyhow::Result<()> {
        xtest::fake_input(
            &self.conn,
            MOTION_NOTIFY,
            0,        // detail (unused for motion)
            0,        // time = CurrentTime
            self.root, // root window
            x as i16,
            y as i16,
            0,        // deviceid = default
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn mouse_button(&self, js_button: u8, pressed: bool) -> anyhow::Result<()> {
        // Map JS MouseEvent.button to X11 button numbers.
        // JS: 0=left, 1=middle, 2=right, 3=back, 4=forward
        // X11: 1=left, 2=middle, 3=right
        let x11_button = match js_button {
            0 => 1,
            1 => 2,
            2 => 3,
            3 => 8, // back
            4 => 9, // forward
            other => other + 1,
        };
        let event_type = if pressed { BUTTON_PRESS } else { BUTTON_RELEASE };
        xtest::fake_input(&self.conn, event_type, x11_button, 0, self.root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn mouse_scroll(&self, dx: i16, dy: i16) -> anyhow::Result<()> {
        // Vertical: button 4 = scroll up, button 5 = scroll down.
        // Horizontal: button 6 = scroll left, button 7 = scroll right.
        // Each "click" of the scroll wheel is a press+release pair.
        // The magnitude indicates how many clicks to synthesize.

        if dy != 0 {
            let button: u8 = if dy < 0 { 4 } else { 5 }; // negative dy = up
            let clicks = (dy.unsigned_abs() as u16).max(1);
            for _ in 0..clicks {
                xtest::fake_input(&self.conn, BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                xtest::fake_input(&self.conn, BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)?;
            }
        }

        if dx != 0 {
            let button: u8 = if dx < 0 { 6 } else { 7 }; // negative dx = left
            let clicks = (dx.unsigned_abs() as u16).max(1);
            for _ in 0..clicks {
                xtest::fake_input(&self.conn, BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                xtest::fake_input(&self.conn, BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)?;
            }
        }

        self.conn.flush()?;
        Ok(())
    }

    fn key_event(&self, js_keycode: u32, pressed: bool) -> anyhow::Result<()> {
        let x11_keycode = match self.js_keycode_to_x11_keycode(js_keycode) {
            Some(kc) => kc,
            None => {
                tracing::warn!("No X11 keycode for JS keyCode {}", js_keycode);
                return Ok(());
            }
        };
        let event_type = if pressed { KEY_PRESS } else { KEY_RELEASE };
        xtest::fake_input(&self.conn, event_type, x11_keycode, 0, self.root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn paste_text(&self, text: &str) -> anyhow::Result<()> {
        self.release_stuck_inputs()?;

        for ch in text.chars() {
            self.type_char(ch)?;
        }

        self.conn.flush()?;
        Ok(())
    }

    fn type_char(&self, ch: char) -> anyhow::Result<()> {
        let (keycode, shift) = match char_to_js_keycode(ch) {
            Some(mapping) => mapping,
            None => {
                tracing::warn!("Paste dropped unsupported character {:?}", ch);
                return Ok(());
            }
        };

        if shift {
            self.key_event(16, true)?;
        }

        self.key_event(keycode, true)?;
        self.key_event(keycode, false)?;

        if shift {
            self.key_event(16, false)?;
        }

        Ok(())
    }

    fn js_keycode_to_x11_keycode(&self, js_key: u32) -> Option<u8> {
        // Convert the JS keyCode to an X11 keysym, then look it up in our
        // pre-built keymap.
        let keysym = js_keycode_to_keysym(js_key)?;
        self.keymap.get(&keysym).copied()
    }
}

/// Convert a JavaScript `KeyboardEvent.keyCode` value to an X11 keysym.
fn js_keycode_to_keysym(js_key: u32) -> Option<u32> {
    Some(match js_key {
        // Letters A-Z: JS 65-90, X11 keysyms for lowercase a-z: 0x61-0x7a
        65..=90 => js_key + 32, // 'A'(65) -> 'a'(97 = 0x61)

        // Digits 0-9: JS 48-57, keysyms match ASCII
        48..=57 => js_key,

        // Numpad 0-9: JS 96-105 -> keysyms 0xFFB0-0xFFB9
        96..=105 => 0xFFB0 + (js_key - 96),

        // F1-F12: JS 112-123 -> keysyms 0xFFBE-0xFFC9
        112..=123 => 0xFFBE + (js_key - 112),

        // Common editing / navigation keys
        8 => 0xFF08,   // Backspace
        9 => 0xFF09,   // Tab
        13 => 0xFF0D,  // Enter / Return
        16 => 0xFFE1,  // Shift (left)
        17 => 0xFFE3,  // Control (left)
        18 => 0xFFE9,  // Alt (left)
        19 => 0xFF13,  // Pause
        20 => 0xFFE5,  // Caps Lock
        27 => 0xFF1B,  // Escape
        32 => 0x0020,  // Space
        33 => 0xFF55,  // Page Up
        34 => 0xFF56,  // Page Down
        35 => 0xFF57,  // End
        36 => 0xFF50,  // Home
        37 => 0xFF51,  // Arrow Left
        38 => 0xFF52,  // Arrow Up
        39 => 0xFF53,  // Arrow Right
        40 => 0xFF54,  // Arrow Down
        45 => 0xFF63,  // Insert
        46 => 0xFFFF,  // Delete

        // Punctuation / symbols (US layout)
        186 => 0x003b, // ; semicolon
        187 => 0x003d, // = equals
        188 => 0x002c, // , comma
        189 => 0x002d, // - minus/hyphen
        190 => 0x002e, // . period
        191 => 0x002f, // / slash
        192 => 0x0060, // ` grave accent
        219 => 0x005b, // [ left bracket
        220 => 0x005c, // \ backslash
        221 => 0x005d, // ] right bracket
        222 => 0x0027, // ' apostrophe

        // Numpad operators
        106 => 0xFFAA, // numpad *
        107 => 0xFFAB, // numpad +
        109 => 0xFFAD, // numpad -
        110 => 0xFFAE, // numpad .
        111 => 0xFFAF, // numpad /

        // Meta / OS keys
        91 => 0xFFEB,  // Left Meta / Windows
        92 => 0xFFEC,  // Right Meta / Windows
        93 => 0xFF67,  // Context Menu

        // Scroll / Num / Print
        144 => 0xFF7F, // Num Lock
        145 => 0xFF14, // Scroll Lock

        _ => return None,
    })
}

fn char_to_js_keycode(ch: char) -> Option<(u32, bool)> {
    Some(match ch {
        'a'..='z' => (ch.to_ascii_uppercase() as u32, false),
        'A'..='Z' => (ch as u32, true),
        '0'..='9' => (ch as u32, false),
        ' ' => (32, false),
        '\n' | '\r' => (13, false),
        '\t' => (9, false),
        '!' => (49, true),
        '@' => (50, true),
        '#' => (51, true),
        '$' => (52, true),
        '%' => (53, true),
        '^' => (54, true),
        '&' => (55, true),
        '*' => (56, true),
        '(' => (57, true),
        ')' => (48, true),
        '-' => (189, false),
        '_' => (189, true),
        '=' => (187, false),
        '+' => (187, true),
        '[' => (219, false),
        '{' => (219, true),
        ']' => (221, false),
        '}' => (221, true),
        '\\' => (220, false),
        '|' => (220, true),
        ';' => (186, false),
        ':' => (186, true),
        '\'' => (222, false),
        '"' => (222, true),
        ',' => (188, false),
        '<' => (188, true),
        '.' => (190, false),
        '>' => (190, true),
        '/' => (191, false),
        '?' => (191, true),
        '`' => (192, false),
        '~' => (192, true),
        _ => return None,
    })
}

/// Read the X server keyboard mapping and build a reverse lookup table
/// from keysym -> hardware keycode.  When multiple keycodes map to the
/// same keysym we keep the first (lowest keycode), which is the
/// primary/unshifted mapping.
fn build_keymap(conn: &RustConnection) -> anyhow::Result<HashMap<u32, u8>> {
    let setup = conn.setup();
    let min_keycode = setup.min_keycode;
    let max_keycode = setup.max_keycode;
    let count = (max_keycode - min_keycode) as u8 + 1;

    let reply = conn
        .get_keyboard_mapping(min_keycode, count)?
        .reply()?;

    let keysyms_per_keycode = reply.keysyms_per_keycode as usize;
    let keysyms = &reply.keysyms;

    let mut map: HashMap<u32, u8> = HashMap::new();

    for i in 0..count as usize {
        let keycode = (min_keycode as usize + i) as u8;
        let base = i * keysyms_per_keycode;
        // Look at all columns, but prefer the first (unshifted) binding.
        for col in 0..keysyms_per_keycode {
            if base + col >= keysyms.len() {
                break;
            }
            let sym = keysyms[base + col];
            if sym != 0 {
                // Only insert if we haven't already recorded a mapping for
                // this keysym (prefer lower keycode / earlier column).
                map.entry(sym).or_insert(keycode);
            }
        }
    }

    Ok(map)
}
