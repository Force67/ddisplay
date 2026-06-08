/// Resize the captured X display to match a client's native resolution.
///
/// Shells out to `xrandr` (inherits the DISPLAY/XAUTHORITY env set at
/// startup). If the requested mode doesn't exist on the output, a CVT-RB
/// style modeline is computed and registered first — virtual/dummy drivers
/// (the usual ddisplay setup) accept any consistent timing.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Change the display resolution. No-op when the display already matches.
pub fn resize_display(width: u32, height: u32) -> Result<()> {
    // Sane bounds; codecs and X need even dimensions.
    let width = width.clamp(640, 7680) & !1;
    let height = height.clamp(480, 4320) & !1;

    let query = run_xrandr(&["--query"])?;
    let info = parse_xrandr_query(&query)?;

    if info.current == Some((width, height)) {
        tracing::debug!("[resize] display already {}x{}", width, height);
        return Ok(());
    }

    let native_name = format!("{}x{}", width, height);
    let mode_name = if info.modes.iter().any(|m| m == &native_name) {
        native_name
    } else {
        // Register a new mode with CVT-RB-style timings.
        let custom_name = format!("{}x{}_dd", width, height);
        if !info.modes.iter().any(|m| m == &custom_name) {
            let m = cvt_rb_modeline(width, height, 60);
            let clock = format!("{:.2}", m.clock_mhz);
            let args: Vec<String> = vec![
                "--newmode".into(), custom_name.clone(),
                clock,
                m.h.0.to_string(), m.h.1.to_string(), m.h.2.to_string(), m.h.3.to_string(),
                m.v.0.to_string(), m.v.1.to_string(), m.v.2.to_string(), m.v.3.to_string(),
                "+hsync".into(), "-vsync".into(),
            ];
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            run_xrandr(&arg_refs)?;
            run_xrandr(&["--addmode", &info.output, &custom_name])?;
        }
        custom_name
    };

    run_xrandr(&["--output", &info.output, "--mode", &mode_name])
        .with_context(|| format!("setting mode {} on {}", mode_name, info.output))?;

    tracing::info!(
        "[resize] display resized to {}x{} (output {}, mode {})",
        width, height, info.output, mode_name,
    );
    Ok(())
}

struct XrandrInfo {
    output: String,
    modes: Vec<String>,
    current: Option<(u32, u32)>,
}

fn run_xrandr(args: &[&str]) -> Result<String> {
    let out = Command::new("xrandr")
        .args(args)
        .output()
        .context("failed to run xrandr — is it installed?")?;
    if !out.status.success() {
        bail!(
            "xrandr {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_xrandr_query(query: &str) -> Result<XrandrInfo> {
    let mut output = None;
    let mut modes = Vec::new();
    let mut current = None;
    let mut in_target_output = false;

    for line in query.lines() {
        if let Some(rest) = line.strip_prefix("Screen ") {
            if let Some(cur) = rest.split("current ").nth(1) {
                let mut it = cur.split(&[' ', ','][..]).filter(|s| !s.is_empty());
                if let (Some(w), Some(_x), Some(h)) = (it.next(), it.next(), it.next()) {
                    current = w.parse().ok().zip(h.parse().ok());
                }
            }
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            // Output header line, e.g. "VGA-1 connected primary 1920x1080+0+0 ..."
            in_target_output = false;
            if output.is_none() && line.contains(" connected") {
                output = line.split_whitespace().next().map(str::to_owned);
                in_target_output = true;
            }
        } else if in_target_output {
            if let Some(mode) = line.split_whitespace().next() {
                modes.push(mode.to_string());
            }
        }
    }

    let output = output.context("no connected xrandr output found")?;
    Ok(XrandrInfo { output, modes, current })
}

struct Modeline {
    clock_mhz: f64,
    /// (hdisplay, hsync_start, hsync_end, htotal)
    h: (u32, u32, u32, u32),
    /// (vdisplay, vsync_start, vsync_end, vtotal)
    v: (u32, u32, u32, u32),
}

/// CVT reduced-blanking timings: fixed 160px horizontal blank, vertical
/// blank sized for the standard 460µs minimum. Virtual outputs don't care
/// about exact CVT compliance, but real heads accept these too.
fn cvt_rb_modeline(w: u32, h: u32, refresh: u32) -> Modeline {
    let htotal = w + 160;
    let vtotal = ((h as f64) * 1.0284).ceil() as u32;
    let clock_mhz = (htotal as u64 * vtotal as u64 * refresh as u64) as f64 / 1_000_000.0;
    Modeline {
        clock_mhz,
        h: (w, w + 48, w + 80, htotal),
        v: (h, h + 3, h + 8, vtotal),
    }
}
