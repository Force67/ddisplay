//! Wayland input injection via a RemoteDesktop D-Bus session (Mutter-native
//! or XDG portal — see `crate::wayland_session::RemoteSessionApi`).
//!
//! Coordinates arrive from clients in absolute remote-pixel coordinates which
//! match the recorded stream/monitor 1:1, so they map directly onto
//! NotifyPointerMotionAbsolute. Keyboard events use evdev keycodes
//! (= shared JS→X11 table − 8). Clipboard (PasteText/ClipboardData) is out of
//! scope on Wayland v1 and logged-and-ignored.

use std::collections::HashSet;
use std::sync::Arc;

use crate::protocol::ClientEvent;
use crate::wayland_session::RemoteSessionApi;

use super::keymap::js_keycode_to_evdev;

// Linux evdev button codes.
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
const BTN_SIDE: i32 = 0x113;
const BTN_EXTRA: i32 = 0x114;

// NotifyPointerAxisDiscrete axes (same for Mutter and portal).
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

/// JS button (0=left,1=middle,2=right,3=back,4=forward) → evdev BTN_*.
fn js_button_to_evdev(js: u8) -> Option<i32> {
    Some(match js {
        0 => BTN_LEFT,
        1 => BTN_MIDDLE,
        2 => BTN_RIGHT,
        3 => BTN_SIDE,
        4 => BTN_EXTRA,
        _ => return None,
    })
}

pub struct WaylandInputInjector {
    session: Arc<dyn RemoteSessionApi>,
    /// Currently pressed evdev keycodes (for release_stuck_inputs).
    pressed_keys: HashSet<u32>,
    /// Currently pressed evdev buttons.
    pressed_buttons: HashSet<i32>,
}

impl WaylandInputInjector {
    pub fn new(session: Arc<dyn RemoteSessionApi>) -> Self {
        tracing::info!("[wayland] input injector ready (RemoteDesktop session)");
        Self {
            session,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        }
    }

    fn send_key(&mut self, js_keycode: u32, pressed: bool) -> anyhow::Result<()> {
        let Some(evdev) = js_keycode_to_evdev(js_keycode) else {
            tracing::warn!("[wayland] no evdev keycode for JS keyCode {}", js_keycode);
            return Ok(());
        };
        self.session.notify_keyboard_keycode(evdev, pressed)?;
        if pressed {
            self.pressed_keys.insert(evdev);
        } else {
            self.pressed_keys.remove(&evdev);
        }
        Ok(())
    }

    fn send_button(&mut self, js_button: u8, pressed: bool) -> anyhow::Result<()> {
        let Some(btn) = js_button_to_evdev(js_button) else {
            tracing::warn!("[wayland] unknown JS mouse button {}", js_button);
            return Ok(());
        };
        self.session.notify_pointer_button(btn, pressed)?;
        if pressed {
            self.pressed_buttons.insert(btn);
        } else {
            self.pressed_buttons.remove(&btn);
        }
        Ok(())
    }

    fn release_keys(&mut self) -> anyhow::Result<()> {
        for key in self.pressed_keys.drain() {
            let _ = self.session.notify_keyboard_keycode(key, false);
        }
        // Also release common modifiers in case state was lost (evdev codes):
        // LCtrl, LShift, LAlt, LMeta, RShift, RCtrl, RAlt.
        for key in [29_u32, 42, 56, 125, 54, 97, 100] {
            let _ = self.session.notify_keyboard_keycode(key, false);
        }
        Ok(())
    }

    fn release_buttons(&mut self) -> anyhow::Result<()> {
        for btn in self.pressed_buttons.drain() {
            let _ = self.session.notify_pointer_button(btn, false);
        }
        for btn in [BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, BTN_EXTRA] {
            let _ = self.session.notify_pointer_button(btn, false);
        }
        Ok(())
    }
}

impl super::InputInjector for WaylandInputInjector {
    fn inject_event(&mut self, event: &ClientEvent) -> anyhow::Result<()> {
        match event {
            ClientEvent::MouseMove { x, y } => {
                self.session
                    .notify_pointer_motion_absolute(*x as f64, *y as f64)?;
            }
            ClientEvent::MouseButton { button, pressed, x, y } => {
                self.session
                    .notify_pointer_motion_absolute(*x as f64, *y as f64)?;
                self.send_button(*button, *pressed)?;
            }
            ClientEvent::MouseScroll { dx, dy, x, y } => {
                self.session
                    .notify_pointer_motion_absolute(*x as f64, *y as f64)?;
                // Client dy>0 = scroll down = positive vertical steps
                // (libinput/Mutter convention; matches the X11 backend's
                // dy>0 → button 5 mapping).
                if *dy != 0 {
                    self.session
                        .notify_pointer_axis_discrete(AXIS_VERTICAL, *dy as i32)?;
                }
                if *dx != 0 {
                    self.session
                        .notify_pointer_axis_discrete(AXIS_HORIZONTAL, *dx as i32)?;
                }
            }
            ClientEvent::KeyEvent { keycode, pressed } => {
                self.send_key(*keycode, *pressed)?;
            }
            ClientEvent::PasteText { .. } => {
                // Clipboard integration is out of scope for the Wayland v1
                // backend (Mutter RemoteDesktop clipboard API: TODO).
                tracing::info!("[wayland] PasteText ignored (clipboard unsupported on wayland backend)");
            }
            ClientEvent::ReleaseKeys => self.release_keys()?,
            ClientEvent::ReleaseMouse => self.release_buttons()?,
            ClientEvent::ReleaseAll => {
                self.release_keys()?;
                self.release_buttons()?;
            }
            ClientEvent::ClipboardData { .. } => {
                tracing::debug!("[wayland] ClipboardData ignored (clipboard unsupported on wayland backend)");
            }
            ClientEvent::ClientReady
            | ClientEvent::RequestKeyframe { .. }
            | ClientEvent::Caps(_)
            | ClientEvent::Stats(_)
            | ClientEvent::Ping { .. }
            | ClientEvent::RequestAddMonitor
            | ClientEvent::RequestRemoveMonitor
            | ClientEvent::TermOpen(_)
            | ClientEvent::TermData { .. }
            | ClientEvent::TermResize { .. } => {}
        }
        Ok(())
    }

    fn release_stuck_inputs(&mut self) -> anyhow::Result<()> {
        self.release_keys()?;
        self.release_buttons()?;
        Ok(())
    }
}
