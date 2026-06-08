//! XDG Desktop Portal RemoteDesktop + ScreenCast session plumbing for the
//! Wayland backend (the KDE Plasma / wlroots path; GNOME also implements it).
//!
//! Unlike the Mutter-native flow (`crate::wayland_session`), the portal flow
//! is permission-gated: the first Start() pops an interactive dialog in the
//! target session. We request `persist_mode: persistent` and store the
//! returned `restore_token` at ~/.local/state/ddisplay/portal-restore-token
//! (0600) so the dialog is one-time per machine. KDE Plasma >= 6.3 can also
//! pre-authorize headless setups via the `kde-authorized` permission table
//! (`flatpak permission-set kde-authorized remote-desktop "" yes`).
//!
//! Flow (all on the session bus, service org.freedesktop.portal.Desktop):
//!   1. RemoteDesktop.CreateSession({handle_token, session_handle_token})
//!   2. RemoteDesktop.SelectDevices(session, {types: KEYBOARD|POINTER,
//!      persist_mode: 2, restore_token?})
//!   3. ScreenCast.SelectSources(session, {types: MONITOR, cursor_mode:
//!      EMBEDDED (fallback HIDDEN)})
//!   4. RemoteDesktop.Start(session, "", {}) → streams: a(ua{sv}) (PipeWire
//!      node id) + restore_token
//!   5. ScreenCast.OpenPipeWireRemote(session, {}) → PipeWire remote fd; the
//!      capturer connects through this fd instead of the default socket.
//!
//! Every request-style portal method returns an org.freedesktop.portal.Request
//! object; the real result arrives as its `Response(u code, a{sv} results)`
//! signal. The request path is precomputed from our unique bus name + the
//! handle_token and subscribed BEFORE the method call to avoid the race.
//! Response codes: 0 = success, 1 = user cancelled, 2 = other error.

use std::collections::HashMap;
use std::os::fd::OwnedFd as StdOwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use zbus::blocking::{fdo::DBusProxy, Connection, Proxy};
use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::wayland_session::RemoteSessionApi;

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const RD_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const SC_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

// RemoteDesktop.AvailableDeviceTypes bits.
const DEVICE_KEYBOARD: u32 = 1;
const DEVICE_POINTER: u32 = 2;
// ScreenCast.AvailableSourceTypes bits.
const SOURCE_MONITOR: u32 = 1;
// ScreenCast.AvailableCursorModes bits.
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
// SelectDevices persist_mode: remember until explicitly revoked.
const PERSIST_PERSISTENT: u32 = 2;

/// Probe whether an xdg-desktop-portal with the RemoteDesktop interface is
/// reachable on the session bus (used by `--backend auto` / mutter fallback).
pub fn portal_available() -> bool {
    let Ok(conn) = Connection::session() else {
        return false;
    };
    let Ok(dbus) = DBusProxy::new(&conn) else {
        return false;
    };
    let has_owner = BusName::try_from(PORTAL_DEST)
        .ok()
        .and_then(|n| dbus.name_has_owner(n).ok())
        .unwrap_or(false);
    if !has_owner {
        return false;
    }
    // The frontend may be up while no backend implements RemoteDesktop —
    // probe the interface version property.
    let Ok(rd) = Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, RD_IFACE) else {
        return false;
    };
    rd.get_property::<u32>("version").is_ok()
}

/// Path to the persisted portal restore token (one-time permission dialog).
fn restore_token_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local/state")
        });
    state.join("ddisplay/portal-restore-token")
}

fn load_restore_token() -> Option<String> {
    let token = std::fs::read_to_string(restore_token_path()).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

fn save_restore_token(token: &str) {
    let path = restore_token_path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(token.as_bytes())
    };
    match write() {
        Ok(()) => tracing::info!("[portal] restore token saved to {}", path.display()),
        Err(e) => tracing::warn!("[portal] failed to save restore token: {}", e),
    }
}

fn discard_restore_token() {
    let path = restore_token_path();
    if std::fs::remove_file(&path).is_ok() {
        tracing::info!("[portal] discarded stale restore token {}", path.display());
    }
}

/// Unique-per-call portal handle token ([A-Za-z0-9_] only).
fn next_token() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "ddisplay_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Our unique bus name in portal-path form (":1.42" → "1_42").
fn sender_path_part(conn: &Connection) -> Result<String> {
    let name = conn
        .unique_name()
        .context("session bus connection has no unique name")?;
    Ok(name.trim_start_matches(':').replace('.', "_"))
}

/// Unwrap one level of variant nesting (a{sv} values inside containers).
fn unwrap_variant<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    match v {
        Value::Value(inner) => inner,
        other => other,
    }
}

type ResponseResults = HashMap<String, OwnedValue>;

/// Call a request-style portal method and wait for the matching
/// org.freedesktop.portal.Request::Response signal.
///
/// `token` MUST be the handle_token already present in the method's options.
/// Returns the `results` vardict; bails on code != 0.
fn portal_request_call<A>(
    conn: &Connection,
    proxy: &Proxy<'_>,
    method: &str,
    token: &str,
    args: &A,
    timeout: Duration,
) -> Result<ResponseResults>
where
    A: serde::Serialize + zbus::zvariant::DynamicType,
{
    let request_path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender_path_part(conn)?,
        token
    );

    // Subscribe BEFORE the method call so the Response can't be missed.
    let request_proxy = Proxy::new(
        conn,
        PORTAL_DEST,
        request_path.clone(),
        REQUEST_IFACE,
    )?;
    let signals = request_proxy
        .receive_signal("Response")
        .with_context(|| format!("subscribe Response for {}", method))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("portal-response".into())
        .spawn(move || {
            let mut signals = signals;
            if let Some(msg) = signals.next() {
                let _ = tx.send(msg);
            }
        })
        .context("spawn portal response-wait thread")?;

    let returned_path: OwnedObjectPath = proxy
        .call(method, args)
        .with_context(|| format!("{}.{} failed", proxy.interface(), method))?;

    // Very old portals (< 0.9) return a server-chosen request path that
    // differs from the precomputed one; re-subscribe there (small race,
    // acceptable for legacy portals only).
    let msg = if returned_path.as_str() != request_path {
        tracing::warn!(
            "[portal] request path mismatch (expected {}, got {}) — old portal? re-subscribing",
            request_path,
            returned_path
        );
        let legacy_proxy =
            Proxy::new(conn, PORTAL_DEST, returned_path.clone(), REQUEST_IFACE)?;
        let mut legacy = legacy_proxy.receive_signal("Response")?;
        let (ltx, lrx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("portal-response-legacy".into())
            .spawn(move || {
                if let Some(m) = legacy.next() {
                    let _ = ltx.send(m);
                }
            })?;
        lrx.recv_timeout(timeout)
    } else {
        rx.recv_timeout(timeout)
    }
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out after {:?} waiting for the portal Response to {} \
             (request {})",
            timeout,
            method,
            returned_path
        )
    })?;

    let (code, results): (u32, ResponseResults) = msg
        .body()
        .deserialize()
        .with_context(|| format!("decode Response body for {}", method))?;
    match code {
        0 => Ok(results),
        1 => bail!("{}: cancelled by the user (portal response code 1)", method),
        n => bail!("{}: portal request failed (response code {})", method, n),
    }
}

/// A live XDG Desktop Portal RemoteDesktop + ScreenCast session.
pub struct PortalRemoteSession {
    #[allow(dead_code)]
    conn: Connection,
    /// org.freedesktop.portal.RemoteDesktop proxy (Notify* input injection).
    rd: Proxy<'static>,
    /// Portal session object path (first Notify* argument).
    session_path: OwnedObjectPath,
    /// PipeWire node id of the screen-cast stream.
    pub node_id: u32,
    /// Stream size as reported by the portal Start() results, if any.
    #[allow(dead_code)]
    pub initial_size: Option<(u32, u32)>,
    /// PipeWire remote fd from OpenPipeWireRemote — taken once by the capturer.
    pw_fd: Mutex<Option<StdOwnedFd>>,
}

impl PortalRemoteSession {
    pub fn new() -> Result<Self> {
        let conn = Connection::session().context(
            "connect to session D-Bus failed — run inside the target Wayland session \
             (DBUS_SESSION_BUS_ADDRESS must point at it)",
        )?;

        let restore_token = load_restore_token();
        if restore_token.is_some() {
            tracing::info!(
                "[portal] using saved restore token from {}",
                restore_token_path().display()
            );
        }

        match Self::setup(&conn, restore_token.clone()) {
            Ok(session) => Ok(session),
            Err(e) if restore_token.is_some() => {
                // Stale/rejected token: drop it and retry once from scratch
                // (the portal then prompts as if it were the first run).
                tracing::warn!(
                    "[portal] session setup with restore token failed ({:#}); \
                     retrying without it",
                    e
                );
                discard_restore_token();
                Self::setup(&conn, None)
            }
            Err(e) => Err(e),
        }
    }

    fn setup(conn: &Connection, restore_token: Option<String>) -> Result<Self> {
        let rd: Proxy<'static> = Proxy::new(conn, PORTAL_DEST, PORTAL_PATH, RD_IFACE)
            .context("create org.freedesktop.portal.RemoteDesktop proxy")?;
        let sc: Proxy<'static> = Proxy::new(conn, PORTAL_DEST, PORTAL_PATH, SC_IFACE)
            .context("create org.freedesktop.portal.ScreenCast proxy")?;

        let rd_version: u32 = rd
            .get_property("version")
            .context("portal RemoteDesktop interface not available (is xdg-desktop-portal + a backend running on this bus?)")?;
        tracing::info!("[portal] RemoteDesktop interface version {}", rd_version);

        // 1. CreateSession
        let token = next_token();
        let session_token = next_token();
        let mut opts: HashMap<&str, Value> = HashMap::new();
        opts.insert("handle_token", Value::from(token.as_str()));
        opts.insert("session_handle_token", Value::from(session_token.as_str()));
        let results = portal_request_call(
            conn,
            &rd,
            "CreateSession",
            &token,
            &(opts,),
            Duration::from_secs(30),
        )?;
        let session_path: OwnedObjectPath = match results.get("session_handle") {
            // The spec stores it as a string variant; some backends use 'o'.
            Some(v) => match &**v {
                Value::Str(s) => ObjectPath::try_from(s.as_str())
                    .context("invalid session_handle path")?
                    .into(),
                Value::ObjectPath(p) => p.clone().into(),
                other => bail!("unexpected session_handle type: {:?}", other),
            },
            None => ObjectPath::try_from(format!(
                "/org/freedesktop/portal/desktop/session/{}/{}",
                sender_path_part(conn)?,
                session_token
            ))?
            .into(),
        };
        tracing::info!("[portal] session created: {}", session_path);

        // 2. SelectDevices (keyboard + pointer, persistent permission)
        let token = next_token();
        let mut opts: HashMap<&str, Value> = HashMap::new();
        opts.insert("handle_token", Value::from(token.as_str()));
        opts.insert("types", Value::from(DEVICE_KEYBOARD | DEVICE_POINTER));
        if rd_version >= 2 {
            opts.insert("persist_mode", Value::from(PERSIST_PERSISTENT));
            if let Some(ref t) = restore_token {
                opts.insert("restore_token", Value::from(t.as_str()));
            }
        } else if restore_token.is_some() {
            tracing::warn!(
                "[portal] RemoteDesktop v{} does not support session persistence; \
                 ignoring restore token",
                rd_version
            );
        }
        portal_request_call(
            conn,
            &rd,
            "SelectDevices",
            &token,
            &(&session_path, opts),
            Duration::from_secs(30),
        )?;
        tracing::info!("[portal] devices selected (keyboard + pointer)");

        // 3. SelectSources (monitor; cursor embedded into frames if supported)
        let cursor_modes: u32 = sc.get_property("AvailableCursorModes").unwrap_or(CURSOR_HIDDEN);
        let cursor_mode = if cursor_modes & CURSOR_EMBEDDED != 0 {
            CURSOR_EMBEDDED
        } else {
            tracing::warn!(
                "[portal] EMBEDDED cursor mode unsupported (available: {:#x}); \
                 falling back to HIDDEN — no cursor in the stream",
                cursor_modes
            );
            CURSOR_HIDDEN
        };
        let token = next_token();
        let mut opts: HashMap<&str, Value> = HashMap::new();
        opts.insert("handle_token", Value::from(token.as_str()));
        opts.insert("types", Value::from(SOURCE_MONITOR));
        opts.insert("multiple", Value::from(false));
        opts.insert("cursor_mode", Value::from(cursor_mode));
        portal_request_call(
            conn,
            &sc,
            "SelectSources",
            &token,
            &(&session_path, opts),
            Duration::from_secs(30),
        )?;
        tracing::info!("[portal] sources selected (monitor, cursor_mode {})", cursor_mode);

        // 4. Start — this is where the interactive permission dialog appears
        //    (unless a valid restore token or KDE's kde-authorized
        //    pre-authorization skips it).
        let start_timeout = std::env::var("DDISPLAY_PORTAL_START_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(120);
        tracing::info!(
            "[portal] Start(): waiting up to {}s for portal approval — if this is \
             the first run, CONFIRM THE PERMISSION DIALOG in the target session \
             (the grant persists via restore token afterwards)",
            start_timeout
        );
        let token = next_token();
        let mut opts: HashMap<&str, Value> = HashMap::new();
        opts.insert("handle_token", Value::from(token.as_str()));
        let results = portal_request_call(
            conn,
            &rd,
            "Start",
            &token,
            &(&session_path, "", opts),
            Duration::from_secs(start_timeout),
        )?;

        if let Some(v) = results.get("restore_token") {
            if let Value::Str(t) = &**v {
                save_restore_token(t.as_str());
            }
        }

        let (node_id, initial_size) = parse_first_stream(&results)
            .context("portal Start() returned no usable streams")?;
        tracing::info!(
            "[portal] started: PipeWire node {} ({:?})",
            node_id,
            initial_size
        );

        // 5. OpenPipeWireRemote — fd the capturer connects PipeWire through.
        let empty: HashMap<&str, Value> = HashMap::new();
        let fd: zbus::zvariant::OwnedFd = sc
            .call("OpenPipeWireRemote", &(&session_path, empty))
            .context("ScreenCast.OpenPipeWireRemote failed")?;
        let fd: StdOwnedFd = fd.into();
        tracing::info!("[portal] PipeWire remote fd obtained");

        Ok(Self {
            conn: conn.clone(),
            rd,
            session_path,
            node_id,
            initial_size,
            pw_fd: Mutex::new(Some(fd)),
        })
    }

    fn empty_opts() -> HashMap<&'static str, Value<'static>> {
        HashMap::new()
    }

    fn stop(&self) {
        // Portal sessions are closed via org.freedesktop.portal.Session.Close
        // on the session object.
        let close = Proxy::new(
            &self.conn,
            PORTAL_DEST,
            self.session_path.clone(),
            SESSION_IFACE,
        )
        .and_then(|p| p.call::<_, _, ()>("Close", &()));
        match close {
            Ok(()) => tracing::info!("[portal] session closed"),
            Err(e) => tracing::debug!("[portal] session Close failed (already gone?): {}", e),
        }
    }
}

impl Drop for PortalRemoteSession {
    fn drop(&mut self) {
        self.stop();
    }
}

impl RemoteSessionApi for PortalRemoteSession {
    fn node_id(&self) -> u32 {
        self.node_id
    }

    fn take_pipewire_fd(&self) -> Option<StdOwnedFd> {
        self.pw_fd.lock().take()
    }

    fn notify_keyboard_keycode(&self, keycode: u32, pressed: bool) -> Result<()> {
        let _: () = self.rd.call(
            "NotifyKeyboardKeycode",
            &(&self.session_path, Self::empty_opts(), keycode as i32, pressed as u32),
        )?;
        Ok(())
    }

    fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()> {
        let _: () = self.rd.call(
            "NotifyPointerButton",
            &(&self.session_path, Self::empty_opts(), button, pressed as u32),
        )?;
        Ok(())
    }

    fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()> {
        // Portal takes the stream NODE ID (u32), unlike Mutter's object path.
        let _: () = self.rd.call(
            "NotifyPointerMotionAbsolute",
            &(&self.session_path, Self::empty_opts(), self.node_id, x, y),
        )?;
        Ok(())
    }

    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<()> {
        let _: () = self.rd.call(
            "NotifyPointerAxisDiscrete",
            &(&self.session_path, Self::empty_opts(), axis, steps),
        )?;
        Ok(())
    }
}

/// Extract (node_id, size) of the first stream from Start() results
/// (`streams: a(ua{sv})`, size prop is `(ii)`).
fn parse_first_stream(results: &ResponseResults) -> Result<(u32, Option<(u32, u32)>)> {
    let streams = results.get("streams").context("no `streams` in results")?;
    let Value::Array(arr) = unwrap_variant(streams) else {
        bail!("`streams` is not an array: {:?}", streams);
    };
    for item in arr.iter() {
        let Value::Structure(st) = unwrap_variant(item) else {
            continue;
        };
        let fields = st.fields();
        let Some(Value::U32(node_id)) = fields.first().map(unwrap_variant) else {
            continue;
        };
        let mut size = None;
        if let Some(Value::Dict(props)) = fields.get(1).map(unwrap_variant) {
            for (k, v) in props.iter() {
                if matches!(unwrap_variant(k), Value::Str(s) if s == "size") {
                    if let Value::Structure(s) = unwrap_variant(v) {
                        if let (Some(Value::I32(w)), Some(Value::I32(h))) = (
                            s.fields().first().map(unwrap_variant),
                            s.fields().get(1).map(unwrap_variant),
                        ) {
                            size = Some((*w as u32, *h as u32));
                        }
                    }
                }
            }
        }
        return Ok((*node_id, size));
    }
    bail!("`streams` array is empty")
}
