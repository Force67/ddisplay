/// Shared runtime control state between the WebSocket transport and the
/// capture/encode loop. The transport layer writes requests here (codec
/// switches, bitrate targets, display resizes); the encode loop applies them
/// between frames and publishes the live session info back.

use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use crate::protocol::SessionInfo;

pub struct StreamControl {
    /// Set by clients (keyframe request) or after a reconfiguration.
    pub force_keyframe: AtomicBool,
    /// Activity flag: input arrived or a client connected since the encode
    /// loop last checked. Paired with the wake condvar so the loop can leave
    /// idle pacing immediately instead of finishing a (up to 100ms) sleep.
    pub input_pending: AtomicBool,
    wake_lock: Mutex<()>,
    wake_cv: Condvar,
    /// Codec switch requested by client capability arbitration.
    /// `Some("h264")` means: rebuild the encoder with this codec.
    pub desired_codec: Mutex<Option<String>>,
    /// Current adaptive bitrate target in bps. The encode loop applies changes
    /// via live encoder reconfiguration (NVENC) or an encoder rebuild.
    pub target_bitrate: AtomicU32,
    /// Upper bound for the adaptive controller (explicit --bitrate, or the
    /// auto formula for the current resolution/codec).
    pub bitrate_ceiling: AtomicU32,
    /// Display resize requested by a client (`--resize-to-client`).
    pub resize_request: Mutex<Option<(u32, u32)>>,
    /// Live session parameters, kept up to date by the encode loop.
    pub session: Mutex<SessionInfo>,
    /// Last time congestion was observed (lagging client or reported drops).
    pub last_congestion: Mutex<Option<Instant>>,
}

impl StreamControl {
    pub fn new(session: SessionInfo, bitrate_ceiling: u32) -> Self {
        Self {
            force_keyframe: AtomicBool::new(false),
            input_pending: AtomicBool::new(false),
            wake_lock: Mutex::new(()),
            wake_cv: Condvar::new(),
            desired_codec: Mutex::new(None),
            target_bitrate: AtomicU32::new(session.bitrate),
            bitrate_ceiling: AtomicU32::new(bitrate_ceiling),
            resize_request: Mutex::new(None),
            session: Mutex::new(session),
            last_congestion: Mutex::new(None),
        }
    }

    /// Signal activity (input event, new client) to the encode loop: sets the
    /// pending flag and wakes the loop if it is sleeping on idle pacing.
    pub fn notify_activity(&self) {
        self.input_pending.store(true, Ordering::Release);
        let _guard = self.wake_lock.lock();
        self.wake_cv.notify_one();
    }

    /// Block until `deadline`, returning early (true) if activity is signalled.
    /// Spurious wakeups simply re-check the clock.
    pub fn wait_activity_until(&self, deadline: Instant) -> bool {
        let mut guard = self.wake_lock.lock();
        loop {
            if self.input_pending.load(Ordering::Acquire) {
                return true;
            }
            if self.wake_cv.wait_until(&mut guard, deadline).timed_out() {
                return self.input_pending.load(Ordering::Acquire);
            }
        }
    }

    /// Record congestion and back the bitrate target off by 25%.
    /// Never drops below 1 Mbps so the stream stays usable.
    pub fn record_congestion(&self) {
        *self.last_congestion.lock() = Some(Instant::now());
        let current = self.target_bitrate.load(Ordering::Acquire);
        let reduced = ((current as u64 * 3) / 4).max(1_000_000) as u32;
        if reduced < current {
            self.target_bitrate.store(reduced, Ordering::Release);
            tracing::info!(
                "[abr] congestion — bitrate target {} -> {} bps",
                current, reduced,
            );
        }
    }

    /// Called periodically: if the link has been clean for a while, ramp the
    /// bitrate back up towards the ceiling (15% steps).
    pub fn maybe_recover_bitrate(&self) {
        let clean = self
            .last_congestion
            .lock()
            .map_or(true, |t| t.elapsed().as_secs() >= 10);
        if !clean {
            return;
        }
        let ceiling = self.bitrate_ceiling.load(Ordering::Acquire);
        let current = self.target_bitrate.load(Ordering::Acquire);
        if current < ceiling {
            let raised = ((current as u64 * 115) / 100).min(ceiling as u64) as u32;
            self.target_bitrate.store(raised, Ordering::Release);
            tracing::debug!("[abr] link clean — bitrate target {} -> {} bps", current, raised);
        }
    }
}

/// Default bitrate for a given resolution/fps/codec (bits per second).
///
/// Tuned for desktop/screen content on a LAN, where bandwidth is cheap and
/// crispness is the point: ~0.12 bits per pixel per frame for H.264, AV1
/// gets ~40% less for similar quality. This is a ceiling — the adaptive
/// controller backs off when a link can't keep up. 1080p60 H.264 ≈ 15 Mbps,
/// 1080p60 AV1 ≈ 9.3 Mbps, 4K60 AV1 ≈ 37 Mbps.
pub fn auto_bitrate(width: u32, height: u32, fps: u32, codec: &str) -> u32 {
    let bpp = if codec == "av1" { 0.075 } else { 0.12 };
    let bps = width as f64 * height as f64 * fps as f64 * bpp;
    bps.clamp(6_000_000.0, 80_000_000.0) as u32
}
