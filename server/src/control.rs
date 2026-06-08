/// Shared runtime control state between the WebSocket transport and the
/// capture/encode loop. The transport layer writes requests here (codec
/// switches, bitrate targets, display resizes); the encode loop applies them
/// between frames and publishes the live session info back.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use crate::protocol::SessionInfo;

pub struct StreamControl {
    /// Set by clients (keyframe request) or after a reconfiguration.
    pub force_keyframe: AtomicBool,
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
            desired_codec: Mutex::new(None),
            target_bitrate: AtomicU32::new(session.bitrate),
            bitrate_ceiling: AtomicU32::new(bitrate_ceiling),
            resize_request: Mutex::new(None),
            session: Mutex::new(session),
            last_congestion: Mutex::new(None),
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
/// Tuned for desktop/screen content: ~0.07 bits per pixel per frame for
/// H.264, AV1 gets ~40% less for similar quality. Clamped to sane bounds —
/// 1080p60 H.264 ≈ 8.7 Mbps, 4K60 AV1 ≈ 20 Mbps.
pub fn auto_bitrate(width: u32, height: u32, fps: u32, codec: &str) -> u32 {
    let bpp = if codec == "av1" { 0.042 } else { 0.07 };
    let bps = width as f64 * height as f64 * fps as f64 * bpp;
    bps.clamp(3_000_000.0, 60_000_000.0) as u32
}
