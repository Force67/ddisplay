//! Virtual multi-monitor support for the captured X session.
//!
//! "Plugging in" monitors means exposing genuine extra heads to the desktop
//! (so the window manager treats each as a real display: its own work area,
//! maximize target, etc.), not just a wider single screen. We do that by
//! sizing the framebuffer to hold the heads in a grid and then declaring one
//! RandR *monitor* per head with `xrandr --setmonitor`, which Mutter/KDE
//! honour as separate displays.
//!
//! The capture still grabs the whole root as one frame; each head is then
//! encoded as its own independent stream from its rect ([`MonitorRect`]).
//!
//! Requires a RandR-capable X server (the Xorg "dummy" driver in the usual
//! ddisplay setup). Xvfb can't resize, so multi-monitor is unavailable there.

use anyhow::Result;

use crate::protocol::MonitorRect;
use crate::resize;

/// Hard cap on virtual heads. 16 = a 4x4 grid of 1920x1080 heads, which is
/// exactly the 7680x4320 framebuffer limit; also comfortably inside the u64
/// keyframe bitmask and the u8 wire monitor id.
pub const MAX_MONITORS: usize = 16;

/// Framebuffer limits (must match the clamp in [`resize::resize_display`]).
const MAX_FB_W: u32 = 7680;
const MAX_FB_H: u32 = 4320;

/// Name of the i-th logical monitor we declare via xrandr.
fn monitor_name(i: usize) -> String {
    format!("ddisplay-{i}")
}

/// Grid shape for `count` heads: as square as possible, preferring extra
/// columns over extra rows (users read multi-monitor setups as a wide row
/// first). Pure function of `count` so the server can recompute the same
/// shape when reconciling against the real captured size.
///
/// 1→1x1, 2→2x1, 3→3x1, 4→2x2, 5..6→3x2, 7..8→4x2, 9→3x3, 16→4x4.
pub fn grid_dims(count: usize) -> (usize, usize) {
    let count = count.clamp(1, MAX_MONITORS);
    let rows = (count as f64).sqrt().floor() as usize;
    let cols = count.div_ceil(rows);
    (cols, rows)
}

/// Split a `w`×`h` framebuffer into the grid for `count` heads. Cell sizes are
/// forced even (codecs want even dimensions); the last column/row absorbs any
/// remainder so the heads tile the framebuffer exactly (the client derives
/// input offsets from these, so they must cover the real captured size with no
/// gap). This is the authoritative client-facing layout, recomputed from the
/// *actual* captured size after every resolution change.
pub fn layout_rects(count: usize, w: u32, h: u32) -> Vec<MonitorRect> {
    let n = count.clamp(1, MAX_MONITORS);
    let (cols, rows) = grid_dims(n);
    let cell_w = ((w / cols as u32) & !1).max(2);
    let cell_h = ((h / rows as u32) & !1).max(2);
    (0..n)
        .map(|i| {
            let (col, row) = (i % cols, i / cols);
            let x = col as u32 * cell_w;
            let y = row as u32 * cell_h;
            // The rightmost column / bottom row take whatever is left, so the
            // grid's union covers the whole framebuffer.
            let width = if col == cols - 1 { w.saturating_sub(x).max(2) } else { cell_w };
            let height = if row == rows - 1 { h.saturating_sub(y).max(2) } else { cell_h };
            MonitorRect { id: i as u32, x, y, width, height }
        })
        .collect()
}

/// Pixel size to millimetres at a nominal 96 dpi (xrandr wants a physical size).
fn mm(px: u32) -> u32 {
    (px * 254 / 960).max(1)
}

/// The ddisplay-declared logical monitors currently present, parsed from
/// `xrandr --listmonitors` (so teardown only spawns xrandr for heads that
/// actually exist, not once per possible name).
fn declared_monitors() -> Vec<String> {
    let Ok(out) = resize::run_xrandr(&["--listmonitors"]) else {
        // Can't query — fall back to trying every possible name.
        return (0..MAX_MONITORS).map(monitor_name).collect();
    };
    out.lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(|name| name.trim_start_matches(['*', '+']).to_string())
        .filter(|name| name.starts_with("ddisplay-"))
        .collect()
}

/// Lay the session out as `count` heads of `base_w`×`base_h` each, arranged
/// in the [`grid_dims`] grid.
///
/// Sizes the framebuffer to the grid, declares one logical monitor per head,
/// and returns their rects. `count == 1` tears the extra heads down and
/// restores the single-head framebuffer.
pub fn apply_layout(count: usize, base_w: u32, base_h: u32) -> Result<Vec<MonitorRect>> {
    let count = count.clamp(1, MAX_MONITORS);
    let (cols, rows) = grid_dims(count);
    let total_w = cols as u32 * base_w;
    let total_h = rows as u32 * base_h;

    // The framebuffer must hold the whole grid. resize_display silently clamps
    // to the limits, beyond which heads would fall outside the framebuffer and
    // --setmonitor would fail, so reject up front with a clear error.
    if total_w > MAX_FB_W || total_h > MAX_FB_H {
        anyhow::bail!(
            "{count} heads of {base_w}x{base_h} need a {total_w}x{total_h} framebuffer \
             (limit {MAX_FB_W}x{MAX_FB_H}); use fewer or smaller heads",
        );
    }

    // Always clear any heads we declared on a previous call before re-laying out
    // (xrandr rejects a --setmonitor whose region falls outside the framebuffer,
    // so the geometry must be torn down before a shrink).
    for name in declared_monitors() {
        let _ = resize::run_xrandr(&["--delmonitor", &name]);
    }

    // Size the framebuffer to hold the grid.
    resize::resize_display(total_w, total_h)?;

    if count == 1 {
        // Single head: we declared no user monitors (the delmonitor loop above
        // cleared any from a previous multi-head layout). With no user-defined
        // RandR monitors, the X server reports the output's automatic monitor
        // again, restoring the normal single-display view. The client-facing
        // rects are recomputed from the real captured size by the caller, so we
        // return a placeholder here.
        return Ok(layout_rects(1, base_w, base_h));
    }

    let mut rects = Vec::with_capacity(count);
    let output = resize::connected_output()?;
    for i in 0..count {
        let x = (i % cols) as u32 * base_w;
        let y = (i / cols) as u32 * base_h;
        // geometry: <w>/<mmw>x<h>/<mmh>+<x>+<y>
        let geom = format!(
            "{}/{}x{}/{}+{}+{}",
            base_w, mm(base_w), base_h, mm(base_h), x, y,
        );
        // The first logical monitor claims the real output; the rest are
        // free-standing heads (`none`) carved out of the same framebuffer.
        let outputs = if i == 0 { output.as_str() } else { "none" };
        resize::run_xrandr(&["--setmonitor", &monitor_name(i), &geom, outputs])?;
        rects.push(MonitorRect { id: i as u32, x, y, width: base_w, height: base_h });
    }

    tracing::info!(
        "[monitor] laid out {} head(s) as {}x{} grid, {}x{} each",
        count, cols, rows, base_w, base_h,
    );
    Ok(rects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_shapes() {
        assert_eq!(grid_dims(1), (1, 1));
        assert_eq!(grid_dims(2), (2, 1));
        assert_eq!(grid_dims(3), (3, 1));
        assert_eq!(grid_dims(4), (2, 2));
        assert_eq!(grid_dims(6), (3, 2));
        assert_eq!(grid_dims(9), (3, 3));
        assert_eq!(grid_dims(16), (4, 4));
    }

    #[test]
    fn rects_tile_framebuffer() {
        for count in 1..=MAX_MONITORS {
            let (w, h) = (7682 / 2 * 2, 4318); // odd-ish sizes still tile
            let rects = layout_rects(count, w, h);
            assert_eq!(rects.len(), count);
            let (cols, rows) = grid_dims(count);
            for r in &rects {
                let (col, row) = (r.id as usize % cols, r.id as usize / cols);
                if col == cols - 1 {
                    assert_eq!(r.x + r.width, w, "head {} must reach right edge", r.id);
                }
                if row == rows - 1 {
                    assert_eq!(r.y + r.height, h, "head {} must reach bottom edge", r.id);
                }
                assert!(r.width >= 2 && r.height >= 2);
            }
        }
    }
}
