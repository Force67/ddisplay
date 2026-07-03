/// Input capture: translates winit events into ddisplay protocol messages.

use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, PhysicalKey};

use crate::protocol;
use crate::transport::TransportSender;

pub struct InputState {
    sender: TransportSender,
    remote_width: u32,
    remote_height: u32,
    /// This window's monitor offset within the server framebuffer. Added to the
    /// scaled local coordinates so input from a second-monitor window lands on
    /// the right head. (0,0) for the primary monitor.
    remote_offset_x: u32,
    remote_offset_y: u32,
    window_width: u32,
    window_height: u32,
    cursor_x: f64,
    cursor_y: f64,
}

impl InputState {
    pub fn new(sender: TransportSender) -> Self {
        Self {
            sender,
            remote_width: 0,
            remote_height: 0,
            remote_offset_x: 0,
            remote_offset_y: 0,
            window_width: 1,
            window_height: 1,
            cursor_x: 0.0,
            cursor_y: 0.0,
        }
    }

    pub fn set_remote_size(&mut self, w: u32, h: u32) {
        self.remote_width = w;
        self.remote_height = h;
    }

    /// Set this window's monitor offset within the server framebuffer.
    pub fn set_remote_offset(&mut self, x: u32, y: u32) {
        self.remote_offset_x = x;
        self.remote_offset_y = y;
    }

    pub fn set_window_size(&mut self, w: u32, h: u32) {
        self.window_width = w.max(1);
        self.window_height = h.max(1);
    }

    pub fn on_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor_x = x;
        self.cursor_y = y;

        if self.remote_width == 0 {
            return;
        }

        let (rx, ry) = self.scale_coords(x, y);
        self.sender.send(protocol::encode_mouse_move(rx, ry));
    }

    pub fn on_mouse_button(&mut self, button: MouseButton, state: ElementState) {
        if self.remote_width == 0 {
            return;
        }

        let js_button = match button {
            MouseButton::Left => 0u8,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::Back => 3,
            MouseButton::Forward => 4,
            MouseButton::Other(n) => n as u8,
        };

        let pressed = state == ElementState::Pressed;
        let (rx, ry) = self.scale_coords(self.cursor_x, self.cursor_y);
        self.sender.send(protocol::encode_mouse_button(js_button, pressed, rx, ry));
    }

    pub fn on_scroll(&mut self, delta: MouseScrollDelta) {
        if self.remote_width == 0 {
            return;
        }

        let (dx, dy) = match delta {
            // winit: +y = scroll up (wheel away from user).
            // Server X11 convention: dy < 0 → button 4 (up), dy > 0 → button 5 (down).
            // Negate y to match.
            MouseScrollDelta::LineDelta(x, y) => (x as i16, -(y as i16)),
            MouseScrollDelta::PixelDelta(pos) => {
                ((pos.x / 48.0) as i16, -(pos.y / 48.0) as i16)
            }
        };

        if dx == 0 && dy == 0 {
            return;
        }

        let (rx, ry) = self.scale_coords(self.cursor_x, self.cursor_y);
        self.sender.send(protocol::encode_mouse_scroll(dx, dy, rx, ry));
    }

    pub fn on_key(&mut self, physical_key: PhysicalKey, state: ElementState) {
        let keycode = match physical_key {
            PhysicalKey::Code(code) => physical_to_js_keycode(code),
            PhysicalKey::Unidentified(_) => return,
        };

        let Some(js_keycode) = keycode else { return };
        let pressed = state == ElementState::Pressed;
        self.sender.send(protocol::encode_key_event(js_keycode, pressed));
    }

    pub fn send_client_ready(&self) {
        self.sender.send(protocol::encode_client_ready());
    }

    /// Send a pre-encoded protocol message (caps, stats, pings, keyframe requests).
    pub fn send_raw(&self, data: Vec<u8>) {
        self.sender.send(data);
    }

    pub fn release_all(&self) {
        self.sender.send(protocol::encode_release_all());
    }

    fn scale_coords(&self, x: f64, y: f64) -> (u16, u16) {
        let ww = self.window_width as f64;
        let wh = self.window_height as f64;
        let rw = self.remote_width as f64;
        let rh = self.remote_height as f64;

        // Mirror the letterbox math in renderer.rs:
        //   scale = min(ww/rw, wh/rh)
        //   rendered area = rw*scale × rh*scale, centred in the window
        // Clicks inside the black bars clamp to the nearest edge.
        let scale = (ww / rw).min(wh / rh);
        let offset_x = (ww - rw * scale) * 0.5;
        let offset_y = (wh - rh * scale) * 0.5;

        let lx = ((x - offset_x) / scale).clamp(0.0, rw - 1.0) as u32;
        let ly = ((y - offset_y) / scale).clamp(0.0, rh - 1.0) as u32;
        // Translate into the server framebuffer by this monitor's offset.
        let rx = (lx + self.remote_offset_x).min(u16::MAX as u32) as u16;
        let ry = (ly + self.remote_offset_y).min(u16::MAX as u32) as u16;
        (rx, ry)
    }
}

/// Map winit PhysicalKey (KeyCode) to JavaScript keyCode values
/// (matching what the web client sends).
fn physical_to_js_keycode(code: KeyCode) -> Option<u32> {
    Some(match code {
        // Letters
        KeyCode::KeyA => 65, KeyCode::KeyB => 66, KeyCode::KeyC => 67,
        KeyCode::KeyD => 68, KeyCode::KeyE => 69, KeyCode::KeyF => 70,
        KeyCode::KeyG => 71, KeyCode::KeyH => 72, KeyCode::KeyI => 73,
        KeyCode::KeyJ => 74, KeyCode::KeyK => 75, KeyCode::KeyL => 76,
        KeyCode::KeyM => 77, KeyCode::KeyN => 78, KeyCode::KeyO => 79,
        KeyCode::KeyP => 80, KeyCode::KeyQ => 81, KeyCode::KeyR => 82,
        KeyCode::KeyS => 83, KeyCode::KeyT => 84, KeyCode::KeyU => 85,
        KeyCode::KeyV => 86, KeyCode::KeyW => 87, KeyCode::KeyX => 88,
        KeyCode::KeyY => 89, KeyCode::KeyZ => 90,

        // Digits
        KeyCode::Digit0 => 48, KeyCode::Digit1 => 49, KeyCode::Digit2 => 50,
        KeyCode::Digit3 => 51, KeyCode::Digit4 => 52, KeyCode::Digit5 => 53,
        KeyCode::Digit6 => 54, KeyCode::Digit7 => 55, KeyCode::Digit8 => 56,
        KeyCode::Digit9 => 57,

        // Function keys
        KeyCode::F1 => 112, KeyCode::F2 => 113, KeyCode::F3 => 114,
        KeyCode::F4 => 115, KeyCode::F5 => 116, KeyCode::F6 => 117,
        KeyCode::F7 => 118, KeyCode::F8 => 119, KeyCode::F9 => 120,
        KeyCode::F10 => 121, KeyCode::F11 => 122, KeyCode::F12 => 123,

        // Editing / Navigation
        KeyCode::Backspace => 8, KeyCode::Tab => 9, KeyCode::Enter => 13,
        KeyCode::ShiftLeft | KeyCode::ShiftRight => 16,
        KeyCode::ControlLeft | KeyCode::ControlRight => 17,
        KeyCode::AltLeft | KeyCode::AltRight => 18,
        KeyCode::Pause => 19, KeyCode::CapsLock => 20, KeyCode::Escape => 27,
        KeyCode::Space => 32, KeyCode::PageUp => 33, KeyCode::PageDown => 34,
        KeyCode::End => 35, KeyCode::Home => 36,
        KeyCode::ArrowLeft => 37, KeyCode::ArrowUp => 38,
        KeyCode::ArrowRight => 39, KeyCode::ArrowDown => 40,
        KeyCode::Insert => 45, KeyCode::Delete => 46,

        // Meta
        KeyCode::SuperLeft => 91, KeyCode::SuperRight => 92,
        KeyCode::ContextMenu => 93,

        // Numpad
        KeyCode::Numpad0 => 96, KeyCode::Numpad1 => 97, KeyCode::Numpad2 => 98,
        KeyCode::Numpad3 => 99, KeyCode::Numpad4 => 100, KeyCode::Numpad5 => 101,
        KeyCode::Numpad6 => 102, KeyCode::Numpad7 => 103, KeyCode::Numpad8 => 104,
        KeyCode::Numpad9 => 105,
        KeyCode::NumpadMultiply => 106, KeyCode::NumpadAdd => 107,
        KeyCode::NumpadSubtract => 109, KeyCode::NumpadDecimal => 110,
        KeyCode::NumpadDivide => 111,
        KeyCode::NumLock => 144, KeyCode::ScrollLock => 145,

        // Punctuation (US layout keyCodes — layout-independent via physical keys)
        KeyCode::Semicolon => 186, KeyCode::Equal => 187,
        KeyCode::Comma => 188, KeyCode::Minus => 189,
        KeyCode::Period => 190, KeyCode::Slash => 191,
        KeyCode::Backquote => 192,
        KeyCode::BracketLeft => 219, KeyCode::Backslash => 220,
        KeyCode::BracketRight => 221, KeyCode::Quote => 222,

        _ => return None,
    })
}
