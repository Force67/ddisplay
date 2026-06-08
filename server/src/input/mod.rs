pub mod keymap;
pub mod wayland;
pub mod x11;

use crate::protocol::ClientEvent;

/// Backend-agnostic input injection.
///
/// Implemented by the X11 (XTest) and Wayland (Mutter RemoteDesktop) backends.
pub trait InputInjector: Send {
    /// Inject a single client input event into the session.
    fn inject_event(&mut self, ev: &ClientEvent) -> anyhow::Result<()>;

    /// Best-effort reset of keys/buttons that may have been left logically
    /// pressed by an interrupted remote session.
    fn release_stuck_inputs(&mut self) -> anyhow::Result<()>;
}
