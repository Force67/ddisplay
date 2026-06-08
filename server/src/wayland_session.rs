//! Mutter RemoteDesktop + ScreenCast D-Bus session plumbing for the Wayland
//! backend.
//!
//! Mirrors what gnome-remote-desktop does (no portal, no permission dialogs):
//!   1. org.gnome.Mutter.RemoteDesktop.CreateSession() → RemoteDesktop session
//!   2. org.gnome.Mutter.ScreenCast.CreateSession({remote-desktop-session-id})
//!   3. ScreenCast session .RecordMonitor(connector, {cursor-mode: EMBEDDED})
//!   4. RemoteDesktop session .Start() → Stream emits PipeWireStreamAdded(node)
//!
//! The server must run with DBUS_SESSION_BUS_ADDRESS pointing at the headless
//! GNOME session's bus (see scripts/start-virtual-gnome-wayland-session.sh,
//! which writes ~/.local/state/ddisplay/wayland-session.env).
//!
//! The session proxies stay alive for the server lifetime; input injection
//! goes through the RemoteDesktop session's Notify* methods. Drop() stops the
//! session.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use zbus::blocking::{fdo::DBusProxy, Connection, Proxy};
use zbus::names::BusName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const RD_DEST: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_IFACE: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_SESSION_IFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";

const SC_DEST: &str = "org.gnome.Mutter.ScreenCast";
const SC_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SC_IFACE: &str = "org.gnome.Mutter.ScreenCast";
const SC_SESSION_IFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM_IFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";

const DC_DEST: &str = "org.gnome.Mutter.DisplayConfig";
const DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const DC_IFACE: &str = "org.gnome.Mutter.DisplayConfig";

/// Mutter ScreenCast cursor mode: cursor composited into the frames.
const CURSOR_MODE_EMBEDDED: u32 = 1;

// GetCurrentState return shape (org.gnome.Mutter.DisplayConfig).
type MonitorSpec = (String, String, String, String);
type MonitorMode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type MonitorInfo = (MonitorSpec, Vec<MonitorMode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, HashMap<String, OwnedValue>);
type CurrentState = (u32, Vec<MonitorInfo>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);

/// Backend-agnostic view of a live remote-desktop session, shared (via
/// `Arc<dyn RemoteSessionApi>`) between the Wayland capturer and the Wayland
/// input injector. Implemented by [`MutterRemoteSession`] (GNOME native) and
/// [`crate::portal_session::PortalRemoteSession`] (XDG portal: KDE/wlroots).
pub trait RemoteSessionApi: Send + Sync {
    /// PipeWire node id of the screen-cast stream.
    fn node_id(&self) -> u32;

    /// PipeWire remote fd to connect through (portal flavor). `None` means
    /// connect to the user's default PipeWire socket. The fd can only be
    /// taken once (by the capturer).
    fn take_pipewire_fd(&self) -> Option<std::os::fd::OwnedFd> {
        None
    }

    /// `keycode` is a Linux evdev keycode (X11 keycode − 8).
    fn notify_keyboard_keycode(&self, keycode: u32, pressed: bool) -> Result<()>;

    /// `button` is a Linux evdev button code (BTN_LEFT = 0x110, ...).
    fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()>;

    /// Absolute motion in stream (= monitor) pixel coordinates.
    fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()>;

    /// Discrete scroll. axis: 0 = vertical, 1 = horizontal.
    /// Positive vertical steps scroll down (libinput convention).
    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<()>;
}

/// Probe whether the Mutter ScreenCast/RemoteDesktop D-Bus services are
/// reachable on the session bus (used by `--backend auto`).
pub fn mutter_available() -> bool {
    let Ok(conn) = Connection::session() else {
        return false;
    };
    let Ok(dbus) = DBusProxy::new(&conn) else {
        return false;
    };
    let has = |name: &str| {
        BusName::try_from(name)
            .ok()
            .and_then(|n| dbus.name_has_owner(n).ok())
            .unwrap_or(false)
    };
    has(SC_DEST) && has(RD_DEST)
}

/// A live Mutter RemoteDesktop + ScreenCast session.
///
/// Shared (via `Arc`) between the Wayland capturer (which consumes the
/// PipeWire stream) and the Wayland input injector (which calls the
/// RemoteDesktop Notify* methods).
pub struct MutterRemoteSession {
    rd_session: Proxy<'static>,
    #[allow(dead_code)]
    sc_session: Proxy<'static>,
    /// ScreenCast stream object path (needed by NotifyPointerMotionAbsolute).
    stream_path: OwnedObjectPath,
    /// PipeWire node id of the screen-cast stream.
    pub node_id: u32,
    /// Size of the recorded monitor at session start (from DisplayConfig).
    #[allow(dead_code)]
    pub initial_size: Option<(u32, u32)>,
}

impl MutterRemoteSession {
    pub fn new() -> Result<Self> {
        let conn = Connection::session().context(
            "connect to session D-Bus failed — run inside the headless GNOME session \
             (DBUS_SESSION_BUS_ADDRESS from wayland-session.env)",
        )?;

        // 1. RemoteDesktop session
        let rd = Proxy::new(&conn, RD_DEST, RD_PATH, RD_IFACE)
            .context("create org.gnome.Mutter.RemoteDesktop proxy")?;
        let rd_session_path: OwnedObjectPath = rd
            .call("CreateSession", &())
            .context("RemoteDesktop.CreateSession failed (is gnome-shell running on this bus?)")?;
        let rd_session = Proxy::new(
            &conn,
            RD_DEST,
            rd_session_path.clone(),
            RD_SESSION_IFACE,
        )?;
        let session_id: String = rd_session
            .get_property("SessionId")
            .context("read RemoteDesktop session SessionId")?;
        tracing::info!("[wayland] RemoteDesktop session {} ({})", session_id, rd_session_path);

        // 2. ScreenCast session linked to the RemoteDesktop session
        let sc = Proxy::new(&conn, SC_DEST, SC_PATH, SC_IFACE)?;
        let mut props: HashMap<&str, Value> = HashMap::new();
        props.insert("remote-desktop-session-id", Value::from(session_id.as_str()));
        props.insert("disable-animations", Value::from(true));
        let sc_session_path: OwnedObjectPath = sc
            .call("CreateSession", &(props,))
            .context("ScreenCast.CreateSession failed")?;
        let sc_session = Proxy::new(
            &conn,
            SC_DEST,
            sc_session_path.clone(),
            SC_SESSION_IFACE,
        )?;

        // 3. Record the (virtual) monitor — find its connector name.
        let (connector, initial_size) = current_monitor(&conn)
            .context("DisplayConfig.GetCurrentState: no monitor found")?;
        tracing::info!(
            "[wayland] recording monitor '{}' ({:?})",
            connector,
            initial_size
        );
        let mut rec_props: HashMap<&str, Value> = HashMap::new();
        rec_props.insert("cursor-mode", Value::from(CURSOR_MODE_EMBEDDED));
        let stream_path: OwnedObjectPath = sc_session
            .call("RecordMonitor", &(connector.as_str(), rec_props))
            .with_context(|| format!("ScreenCast RecordMonitor({}) failed", connector))?;
        let stream = Proxy::new(&conn, SC_DEST, stream_path.clone(), SC_STREAM_IFACE)?;

        // 4. Subscribe for PipeWireStreamAdded BEFORE Start() so the signal
        //    can't be missed, then start the (linked) sessions.
        let added = stream
            .receive_signal("PipeWireStreamAdded")
            .context("subscribe PipeWireStreamAdded")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("mutter-stream-added".into())
            .spawn(move || {
                let mut added = added;
                if let Some(msg) = added.next() {
                    let _ = tx.send(msg);
                }
            })
            .context("spawn signal-wait thread")?;

        let _: () = rd_session
            .call("Start", &())
            .context("RemoteDesktop session Start failed")?;

        let msg = rx
            .recv_timeout(Duration::from_secs(15))
            .context("timed out waiting for PipeWireStreamAdded after Start()")?;
        let node_id: u32 = msg
            .body()
            .deserialize()
            .context("decode PipeWireStreamAdded body")?;
        tracing::info!("[wayland] PipeWire stream node id {}", node_id);

        Ok(Self {
            rd_session,
            sc_session,
            stream_path,
            node_id,
            initial_size,
        })
    }

    fn stop(&self) {
        if let Err(e) = self.rd_session.call::<_, _, ()>("Stop", &()) {
            tracing::debug!("[wayland] session Stop failed (already gone?): {}", e);
        } else {
            tracing::info!("[wayland] Mutter remote session stopped");
        }
    }
}

impl Drop for MutterRemoteSession {
    fn drop(&mut self) {
        self.stop();
    }
}

// Input injection goes through the RemoteDesktop session's Notify* methods.
impl RemoteSessionApi for MutterRemoteSession {
    fn node_id(&self) -> u32 {
        self.node_id
    }

    fn notify_keyboard_keycode(&self, keycode: u32, pressed: bool) -> Result<()> {
        let _: () = self
            .rd_session
            .call("NotifyKeyboardKeycode", &(keycode, pressed))?;
        Ok(())
    }

    fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()> {
        let _: () = self
            .rd_session
            .call("NotifyPointerButton", &(button, pressed))?;
        Ok(())
    }

    fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()> {
        // Mutter wants the ScreenCast stream OBJECT PATH (the portal flavor
        // takes the PipeWire node id instead).
        let _: () = self.rd_session.call(
            "NotifyPointerMotionAbsolute",
            &(self.stream_path.as_str(), x, y),
        )?;
        Ok(())
    }

    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<()> {
        let _: () = self
            .rd_session
            .call("NotifyPointerAxisDiscrete", &(axis, steps))?;
        Ok(())
    }
}

/// First monitor's connector name + current mode size from
/// org.gnome.Mutter.DisplayConfig.GetCurrentState.
fn current_monitor(conn: &Connection) -> Result<(String, Option<(u32, u32)>)> {
    let dc = Proxy::new(conn, DC_DEST, DC_PATH, DC_IFACE)?;
    let (_serial, monitors, _logical, _props): CurrentState =
        dc.call("GetCurrentState", &()).context("GetCurrentState failed")?;

    let mon = monitors.first().context("no monitors in GetCurrentState")?;
    let connector = mon.0 .0.clone();

    // Find the current mode (property "is-current": true), else the first.
    let mut size = None;
    for mode in &mon.1 {
        let is_current = mode
            .6
            .get("is-current")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);
        if is_current || size.is_none() {
            size = Some((mode.1 as u32, mode.2 as u32));
        }
        if is_current {
            break;
        }
    }
    Ok((connector, size))
}
