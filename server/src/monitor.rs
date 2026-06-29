//! Virtual multi-monitor support for the captured X session.
//!
//! "Plugging in" a second monitor means exposing a genuine second head to the
//! desktop (so the window manager treats it as a real display: its own work
//! area, maximize target, etc.), not just a wider single screen. We do that by
//! widening the framebuffer to hold the heads side by side and then declaring
//! one RandR *monitor* per head with `xrandr --setmonitor`, which Mutter/KDE
//! honour as separate displays.
//!
//! The capture still grabs the whole root as one stream; the client crops each
//! head's rect ([`MonitorRect`]) into its own window.
//!
//! Requires a RandR-capable X server (the Xorg "dummy" driver in the usual
//! ddisplay setup). Xvfb can't resize, so multi-monitor is unavailable there.

use anyhow::Result;

use crate::protocol::MonitorRect;
use crate::resize;

/// Hard cap on virtual heads (keeps the framebuffer and encoder sane).
pub const MAX_MONITORS: usize = 4;

/// Name of the i-th logical monitor we declare via xrandr.
fn monitor_name(i: usize) -> String {
    format!("ddisplay-{i}")
}

/// Split a `w`×`h` framebuffer into `count` equal-width heads. The last head
/// absorbs any remainder so the heads tile the framebuffer exactly (the client
/// derives crop UVs and input offsets from these, so they must cover the real
/// captured size with no gap). This is the authoritative client-facing layout,
/// recomputed from the *actual* captured size after every resolution change.
pub fn equal_columns(count: usize, w: u32, h: u32) -> Vec<MonitorRect> {
    let n = count.clamp(1, MAX_MONITORS);
    let col_w = ((w / n as u32) & !1).max(2);
    (0..n)
        .map(|i| {
            let x = i as u32 * col_w;
            // The rightmost head takes whatever is left, so the union == w.
            let width = if i == n - 1 { w.saturating_sub(x).max(2) } else { col_w };
            MonitorRect { id: i as u32, x, y: 0, width, height: h }
        })
        .collect()
}

/// Pixel size to millimetres at a nominal 96 dpi (xrandr wants a physical size).
fn mm(px: u32) -> u32 {
    (px * 254 / 960).max(1)
}

/// Lay the session out as `count` side-by-side heads, each `base_w`×`base_h`.
///
/// Widens the framebuffer to `base_w * count`×`base_h`, declares one logical
/// monitor per head, and returns their rects. `count == 1` tears the extra
/// heads down and restores the single-head framebuffer.
pub fn apply_layout(count: usize, base_w: u32, base_h: u32) -> Result<Vec<MonitorRect>> {
    let count = count.clamp(1, MAX_MONITORS);

    // The framebuffer must hold every head side by side. resize_display caps
    // width at 7680, beyond which the heads would fall outside it and
    // --setmonitor would fail — reject up front with a clear error.
    let total_w = base_w as u64 * count as u64;
    if total_w > 7680 {
        anyhow::bail!(
            "{} heads of {}px exceed the {}px framebuffer limit",
            count, base_w, 7680,
        );
    }

    // Always clear any heads we declared on a previous call before re-laying out
    // (xrandr rejects a --setmonitor whose region falls outside the framebuffer,
    // so the geometry must be torn down before a shrink).
    for i in 0..MAX_MONITORS {
        let _ = resize::run_xrandr(&["--delmonitor", &monitor_name(i)]);
    }

    // Size the framebuffer to hold every head.
    resize::resize_display(base_w * count as u32, base_h)?;

    if count == 1 {
        // Single head: we declared no user monitors (the delmonitor loop above
        // cleared any from a previous multi-head layout). With no user-defined
        // RandR monitors, the X server reports the output's automatic monitor
        // again, restoring the normal single-display view. The client-facing
        // rects are recomputed from the real captured size by the caller, so we
        // return a placeholder here.
        return Ok(equal_columns(1, base_w, base_h));
    }
    let mut rects = Vec::with_capacity(count);

    let output = resize::connected_output()?;
    for i in 0..count {
        let x = i as u32 * base_w;
        // geometry: <w>/<mmw>x<h>/<mmh>+<x>+<y>
        let geom = format!(
            "{}/{}x{}/{}+{}+{}",
            base_w, mm(base_w), base_h, mm(base_h), x, 0,
        );
        // The first logical monitor claims the real output; the rest are
        // free-standing heads (`none`) carved out of the same framebuffer.
        let outputs = if i == 0 { output.as_str() } else { "none" };
        resize::run_xrandr(&["--setmonitor", &monitor_name(i), &geom, outputs])?;
        rects.push(MonitorRect { id: i as u32, x, y: 0, width: base_w, height: base_h });
    }

    tracing::info!("[monitor] laid out {} head(s) at {}x{} each", count, base_w, base_h);
    Ok(rects)
}
