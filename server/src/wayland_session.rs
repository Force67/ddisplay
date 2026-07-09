//! Mutter RemoteDesktop + ScreenCast D-Bus session plumbing for the Wayland
//! backend.
//!
//! Mirrors what gnome-remote-desktop does (no portal, no permission dialogs):
//!   1. org.gnome.Mutter.RemoteDesktop.CreateSession() → RemoteDesktop session
//!   2. org.gnome.Mutter.ScreenCast.CreateSession({remote-desktop-session-id})
//!   3. ScreenCast session .RecordMonitor(connector, {cursor-mode}) (see cursor_mode)
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

// Mutter ScreenCast cursor modes.
const CURSOR_MODE_HIDDEN: u32 = 0;
const CURSOR_MODE_EMBEDDED: u32 = 1;
const CURSOR_MODE_METADATA: u32 = 2;

/// Cursor mode for the recorded streams. Default HIDDEN: the OS cursor is left
/// out of the stream so it does not lag a round-trip behind the client's own
/// local cursor (the two-cursor artifact). The client draws its cursor and
/// `WaylandCapturer::embeds_cursor` stays true, so the server sends no cursor
/// of its own. DDISPLAY_CURSOR=embedded composites the OS cursor into the
/// frames instead (for a client that draws no local cursor).
fn cursor_mode() -> u32 {
    match std::env::var("DDISPLAY_CURSOR").as_deref() {
        Ok("embedded") => CURSOR_MODE_EMBEDDED,
        Ok("metadata") => CURSOR_MODE_METADATA,
        _ => CURSOR_MODE_HIDDEN,
    }
}

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

    // ── Multi-head (Wayland/Mutter virtual monitors) ─────────────────────
    // Defaults make single-head backends (XDG portal) a no-op.

    /// Rebuild the session to drive `count` heads (head 0 + count-1 virtual
    /// monitors), returning the new per-head PipeWire node ids. Backends that
    /// can't add virtual monitors return an error (the caller keeps the
    /// current layout). `count >= 1`.
    fn set_head_count(&self, count: usize) -> Result<Vec<u32>> {
        anyhow::bail!("multi-head not supported on this backend (requested {count})")
    }

    /// Tell the session the client-space rectangle of each head so pointer
    /// motion in absolute (whole-layout) coordinates can be routed to the head
    /// it lands on. Single-head backends ignore this.
    fn set_head_layout(&self, _rects: Vec<(u32, u32, u32, u32)>) {}
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

/// One head's ScreenCast stream. Head 0 is a RecordMonitor of the primary
/// virtual monitor; heads 1+ are RecordVirtual monitors.
struct Head {
    stream_path: OwnedObjectPath,
    node_id: u32,
}

/// Mutable session state, replaced wholesale when the head count changes
/// (Mutter only accepts Record* before Start(), so add/remove rebuilds the
/// RemoteDesktop + ScreenCast sessions).
struct Inner {
    rd_session: Proxy<'static>,
    #[allow(dead_code)]
    sc_session: Proxy<'static>,
    heads: Vec<Head>,
    /// Client-space rect (x,y,w,h) of each head, for pointer routing.
    layout: Vec<(u32, u32, u32, u32)>,
}

/// A live Mutter RemoteDesktop + ScreenCast session, possibly driving several
/// heads (the primary monitor plus RecordVirtual virtual monitors).
///
/// Shared (via `Arc`) between the Wayland capturer(s) and the input injector.
/// The head set is behind a `Mutex` so [`set_head_count`](Self::set_head_count)
/// can rebuild it while the shared `Arc` stays valid for both consumers.
pub struct MutterRemoteSession {
    conn: Connection,
    /// Connector of the primary (real) virtual monitor, e.g. "Meta-0".
    primary_connector: String,
    inner: parking_lot::Mutex<Inner>,
}

impl MutterRemoteSession {
    pub fn new() -> Result<Self> {
        let conn = Connection::session().context(
            "connect to session D-Bus failed — run inside the headless GNOME session \
             (DBUS_SESSION_BUS_ADDRESS from wayland-session.env)",
        )?;
        let (connector, initial_size) = current_monitor(&conn)
            .context("DisplayConfig.GetCurrentState: no monitor found")?;
        tracing::info!("[wayland] primary monitor '{}' ({:?})", connector, initial_size);
        let inner = build_session(&conn, &connector, 1)?;
        Ok(Self {
            conn,
            primary_connector: connector,
            inner: parking_lot::Mutex::new(inner),
        })
    }
}

/// Build a fresh RemoteDesktop + ScreenCast session driving `count` heads
/// (head 0 = RecordMonitor(primary), heads 1.. = RecordVirtual). All streams
/// are recorded and their PipeWireStreamAdded subscribed BEFORE Start(),
/// because Mutter rejects Record* after Start and would otherwise race the
/// node-id signal.
fn build_session(conn: &Connection, connector: &str, count: usize) -> Result<Inner> {
    let count = count.max(1);

    let rd = Proxy::new(conn, RD_DEST, RD_PATH, RD_IFACE)
        .context("create org.gnome.Mutter.RemoteDesktop proxy")?;
    let rd_session_path: OwnedObjectPath = rd
        .call("CreateSession", &())
        .context("RemoteDesktop.CreateSession failed (is gnome-shell running on this bus?)")?;
    let rd_session = Proxy::new(conn, RD_DEST, rd_session_path.clone(), RD_SESSION_IFACE)?;
    let session_id: String = rd_session
        .get_property("SessionId")
        .context("read RemoteDesktop session SessionId")?;

    let sc = Proxy::new(conn, SC_DEST, SC_PATH, SC_IFACE)?;
    let mut props: HashMap<&str, Value> = HashMap::new();
    props.insert("remote-desktop-session-id", Value::from(session_id.as_str()));
    props.insert("disable-animations", Value::from(true));
    let sc_session_path: OwnedObjectPath = sc
        .call("CreateSession", &(props,))
        .context("ScreenCast.CreateSession failed")?;
    let sc_session = Proxy::new(conn, SC_DEST, sc_session_path.clone(), SC_SESSION_IFACE)?;

    // Record head 0 (the real primary monitor) + count-1 virtual monitors.
    let cmode = cursor_mode();
    tracing::info!("[wayland] cursor-mode {} ({})", cmode,
        match cmode { 0 => "hidden", 1 => "embedded", _ => "metadata" });
    let mut stream_paths: Vec<OwnedObjectPath> = Vec::with_capacity(count);
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert("cursor-mode", Value::from(cmode));
    let primary: OwnedObjectPath = sc_session
        .call("RecordMonitor", &(connector, rec))
        .with_context(|| format!("ScreenCast RecordMonitor({connector}) failed"))?;
    stream_paths.push(primary);
    for i in 1..count {
        let mut vp: HashMap<&str, Value> = HashMap::new();
        vp.insert("cursor-mode", Value::from(cmode));
        let v: OwnedObjectPath = sc_session
            .call("RecordVirtual", &(vp,))
            .with_context(|| format!("ScreenCast RecordVirtual (head {i}) failed"))?;
        stream_paths.push(v);
    }

    // Subscribe PipeWireStreamAdded for every stream BEFORE Start().
    let mut receivers = Vec::with_capacity(count);
    for path in &stream_paths {
        let stream = Proxy::new(conn, SC_DEST, path.clone(), SC_STREAM_IFACE)?;
        let added = stream
            .receive_signal("PipeWireStreamAdded")
            .context("subscribe PipeWireStreamAdded")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("mutter-stream-added".into())
            .spawn(move || {
                let mut added = added;
                if let Some(msg) = added.next() {
                    let node: u32 = msg.body().deserialize().unwrap_or(0);
                    let _ = tx.send(node);
                }
            })
            .context("spawn signal-wait thread")?;
        receivers.push(rx);
    }

    let _: () = rd_session
        .call("Start", &())
        .context("RemoteDesktop session Start failed")?;

    let mut heads = Vec::with_capacity(count);
    for (i, (path, rx)) in stream_paths.into_iter().zip(receivers).enumerate() {
        let node_id = rx
            .recv_timeout(Duration::from_secs(15))
            .with_context(|| format!("timed out waiting for PipeWireStreamAdded (head {i})"))?;
        tracing::info!("[wayland] head {} stream node {} ({})", i, node_id, path);
        heads.push(Head { stream_path: path, node_id });
    }

    Ok(Inner { rd_session, sc_session, heads, layout: Vec::new() })
}

impl MutterRemoteSession {
    fn stop(&self) {
        let inner = self.inner.lock();
        if let Err(e) = inner.rd_session.call::<_, _, ()>("Stop", &()) {
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
        self.inner.lock().heads[0].node_id
    }

    fn set_head_layout(&self, rects: Vec<(u32, u32, u32, u32)>) {
        self.inner.lock().layout = rects;
    }

    fn set_head_count(&self, count: usize) -> Result<Vec<u32>> {
        let count = count.max(1);
        // Tear down the current session first; Mutter only allows one Record*
        // set per session and rejects Record* after Start().
        {
            let inner = self.inner.lock();
            let _ = inner.rd_session.call::<_, _, ()>("Stop", &());
        }
        let new_inner = build_session(&self.conn, &self.primary_connector, count)
            .with_context(|| format!("rebuild Mutter session for {count} head(s)"))?;
        let node_ids: Vec<u32> = new_inner.heads.iter().map(|h| h.node_id).collect();
        *self.inner.lock() = new_inner;
        tracing::info!("[wayland] session rebuilt for {} head(s): {:?}", count, node_ids);
        Ok(node_ids)
    }

    fn notify_keyboard_keycode(&self, keycode: u32, pressed: bool) -> Result<()> {
        let _: () = self
            .inner
            .lock()
            .rd_session
            .call("NotifyKeyboardKeycode", &(keycode, pressed))?;
        Ok(())
    }

    fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()> {
        let _: () = self
            .inner
            .lock()
            .rd_session
            .call("NotifyPointerButton", &(button, pressed))?;
        Ok(())
    }

    fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()> {
        // Route whole-layout absolute coordinates to the head whose client-space
        // rect contains them, then translate to that head's local coordinates.
        // Mutter's NotifyPointerMotionAbsolute is per-stream (per virtual
        // monitor); the portal flavor takes a node id instead.
        let inner = self.inner.lock();
        let (idx, lx, ly) = route_to_head(&inner.layout, x, y);
        let stream_path = inner
            .heads
            .get(idx)
            .unwrap_or(&inner.heads[0])
            .stream_path
            .clone();
        let _: () = inner.rd_session.call(
            "NotifyPointerMotionAbsolute",
            &(stream_path.as_str(), lx, ly),
        )?;
        Ok(())
    }

    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<()> {
        let _: () = self
            .inner
            .lock()
            .rd_session
            .call("NotifyPointerAxisDiscrete", &(axis, steps))?;
        Ok(())
    }
}

/// Pick the head whose client-space rect contains `(x, y)` and return its
/// index plus head-local coordinates. Falls back to head 0 when no layout is
/// set (single head) or the point is outside every rect (clamped to the
/// nearest by leaving it on head 0's global coords).
fn route_to_head(layout: &[(u32, u32, u32, u32)], x: f64, y: f64) -> (usize, f64, f64) {
    for (i, &(rx, ry, rw, rh)) in layout.iter().enumerate() {
        let (rx, ry, rw, rh) = (rx as f64, ry as f64, rw as f64, rh as f64);
        if x >= rx && x < rx + rw && y >= ry && y < ry + rh {
            return (i, x - rx, y - ry);
        }
    }
    (0, x, y)
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
