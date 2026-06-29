/// Egui-based overlay menu (toggle with F2) and stats HUD (toggle with F3).
///
/// Overlay shows: connection info, display mode selector, file transfer
/// panel. The stats HUD renders rolling histograms of latency, fps and
/// network throughput in the top-right corner.

use std::collections::VecDeque;

use egui_winit::winit;

use crate::files::FileTransferState;

/// Samples kept per metric (one per second → two minutes of history).
const STATS_CAPACITY: usize = 120;

/// Rolling per-second metric history for the stats HUD. The app pushes one
/// sample per stats tick; the HUD draws each series as a bar histogram.
pub struct StatsHistory {
    pub rtt_ms: VecDeque<f32>,
    pub fps: VecDeque<f32>,
    pub mbps: VecDeque<f32>,
    /// Latest decode time in ms (single value, shown as text).
    pub decode_ms: f32,
}

impl StatsHistory {
    pub fn new() -> Self {
        Self {
            rtt_ms: VecDeque::with_capacity(STATS_CAPACITY),
            fps: VecDeque::with_capacity(STATS_CAPACITY),
            mbps: VecDeque::with_capacity(STATS_CAPACITY),
            decode_ms: 0.0,
        }
    }

    pub fn push(&mut self, rtt_ms: f32, fps: f32, mbps: f32, decode_ms: f32) {
        fn push_capped(buf: &mut VecDeque<f32>, v: f32) {
            if buf.len() == STATS_CAPACITY {
                buf.pop_front();
            }
            buf.push_back(v);
        }
        push_capped(&mut self.rtt_ms, rtt_ms);
        push_capped(&mut self.fps, fps);
        push_capped(&mut self.mbps, mbps);
        self.decode_ms = decode_ms;
    }
}

/// Pre-tessellated egui frame ready for GPU upload and rendering.
/// Produced by [`OverlayState::run_ui`] so the renderer never needs to
/// call `tessellate()` on its own context.
pub struct EguiRenderData {
    pub textures_delta: egui::TexturesDelta,
    pub clipped: Vec<egui::ClippedPrimitive>,
    pub pixels_per_point: f32,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum DisplayMode {
    #[default]
    Letterbox,
    Stretch,
}

/// Actions triggered by the overlay UI that must be handled by the caller.
#[derive(Default)]
pub enum OverlayAction {
    #[default]
    None,
    /// User clicked "Upload" — caller should open a native file dialog.
    RequestUpload,
    /// User clicked download on a specific file.
    Download(String),
    /// User clicked refresh.
    RefreshFiles,
    /// User asked to plug in a virtual second monitor.
    AddMonitor,
    /// User asked to unplug the last virtual monitor.
    RemoveMonitor,
}

pub struct OverlayState {
    pub ctx: egui::Context,
    pub winit_state: egui_winit::State,
    pub visible: bool,
    /// Stats HUD (histograms) visibility — independent of the menu.
    pub stats_visible: bool,
    pub display_mode: DisplayMode,
    /// Number of monitors (heads) the session currently exposes, mirrored from
    /// the app each frame so the panel can show and gate the add/remove buttons.
    pub monitor_count: usize,
}

impl OverlayState {
    pub fn new(window: &winit::window::Window) -> Self {
        let ctx = egui::Context::default();
        // Slightly larger default font for readability at high-DPI
        let mut style = (*ctx.style()).clone();
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::proportional(16.0),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::proportional(16.0),
        );
        ctx.set_style(style);

        let winit_state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            None,
            None,
            None,
        );
        Self {
            ctx,
            winit_state,
            visible: false,
            // F3 toggles at runtime; DDISPLAY_STATS=1 starts with the HUD on.
            stats_visible: std::env::var("DDISPLAY_STATS").map(|v| v == "1").unwrap_or(false),
            display_mode: DisplayMode::default(),
            monitor_count: 1,
        }
    }

    /// Returns pre-tessellated egui render data (or `None` when hidden) and any
    /// action the caller must perform (e.g. open a native file dialog).
    ///
    /// Tessellation is done here using `self.ctx` so the renderer never needs to
    /// create its own context (which would panic — "No fonts loaded").
    pub fn run_ui(
        &mut self,
        server: &str,
        codec: &str,
        fps: u32,
        rtt_ms: f32,
        stats: &StatsHistory,
        window: &winit::window::Window,
        files: Option<&mut FileTransferState>,
    ) -> (Option<EguiRenderData>, OverlayAction) {
        // Always drain accumulated winit input so it doesn't pile up while hidden.
        let raw_input = self.winit_state.take_egui_input(window);

        if !self.visible && !self.stats_visible {
            return (None, OverlayAction::None);
        }

        self.ctx.begin_pass(raw_input);

        let screen = self.ctx.screen_rect();
        let panel_w = 380.0f32;
        let panel_x = (screen.width() - panel_w) * 0.5;
        let panel_y = (screen.height() - 360.0f32) * 0.5;

        let mut action = OverlayAction::None;

        if self.stats_visible {
            draw_stats_hud(&self.ctx, codec, stats);
        }

        if self.visible {
        egui::Area::new(egui::Id::new("overlay_panel"))
            .fixed_pos(egui::pos2(panel_x, panel_y))
            .order(egui::Order::Foreground)
            .show(&self.ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_premultiplied(20, 20, 28, 220))
                    .corner_radius(12.0)
                    .inner_margin(egui::Margin::same(20))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(70)))
                    .show(ui, |ui| {
                        ui.set_min_width(panel_w - 40.0);

                        ui.heading("ddisplay settings");
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(8.0);

                        ui.label(format!("Server: {server}"));
                        if rtt_ms > 0.0 {
                            ui.label(format!("Codec: {codec}  |  FPS: {fps}  |  RTT: {rtt_ms:.0} ms"));
                        } else {
                            ui.label(format!("Codec: {codec}  |  FPS: {fps}"));
                        }
                        ui.add_space(12.0);

                        ui.label("Display mode:");
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut self.display_mode,
                                DisplayMode::Letterbox,
                                "⬛ Letterbox",
                            );
                            ui.selectable_value(
                                &mut self.display_mode,
                                DisplayMode::Stretch,
                                "⤢ Stretch",
                            );
                        });

                        // ── Monitors ──────────────────────────────────────────
                        ui.add_space(12.0);
                        ui.label(format!("🖥 Monitors: {}", self.monitor_count.max(1)));
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    self.monitor_count < 2,
                                    egui::Button::new("➕ Add monitor"),
                                )
                                .on_hover_text("Plug in a second virtual monitor")
                                .clicked()
                            {
                                action = OverlayAction::AddMonitor;
                            }
                            if ui
                                .add_enabled(
                                    self.monitor_count > 1,
                                    egui::Button::new("➖ Remove"),
                                )
                                .clicked()
                            {
                                action = OverlayAction::RemoveMonitor;
                            }
                        });

                        // ── File transfer panel ──────────────────────────────
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);

                        ui.horizontal(|ui| {
                            ui.label("📁 Files");
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("↑ Upload").clicked() {
                                        action = OverlayAction::RequestUpload;
                                    }
                                    if ui.small_button("↻").on_hover_text("Refresh").clicked() {
                                        action = OverlayAction::RefreshFiles;
                                    }
                                },
                            );
                        });

                        ui.add_space(4.0);

                        if let Some(files) = files {
                            egui::ScrollArea::vertical()
                                .max_height(160.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_min_width(panel_w - 40.0);
                                    if files.loading {
                                        ui.weak("Loading…");
                                    } else if files.files.is_empty() {
                                        ui.weak("No files — server may not have --shared-dir set.");
                                    } else {
                                        for entry in &files.files {
                                            ui.horizontal(|ui| {
                                                ui.label(&entry.name);
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if ui
                                                            .small_button("↓")
                                                            .on_hover_text("Download")
                                                            .clicked()
                                                        {
                                                            action = OverlayAction::Download(
                                                                entry.name.clone(),
                                                            );
                                                        }
                                                        ui.weak(entry.size_human());
                                                    },
                                                );
                                            });
                                        }
                                    }
                                });

                            if !files.status.is_empty() {
                                ui.add_space(4.0);
                                ui.weak(&files.status);
                            }
                        } else {
                            ui.weak("File sharing unavailable.");
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.weak("F2 close · F3 stats");
                    });
            });
        } // self.visible

        let full_output = self.ctx.end_pass();
        let pixels_per_point = full_output.pixels_per_point;
        let clipped = self.ctx.tessellate(full_output.shapes, pixels_per_point);
        (
            Some(EguiRenderData {
                textures_delta: full_output.textures_delta,
                clipped,
                pixels_per_point,
            }),
            action,
        )
    }
}

/// Stats HUD: latency / fps / throughput histograms, top-right corner.
fn draw_stats_hud(ctx: &egui::Context, codec: &str, stats: &StatsHistory) {
    let hud_w = 280.0f32;
    let screen = ctx.screen_rect();

    egui::Area::new(egui::Id::new("stats_hud"))
        .fixed_pos(egui::pos2(screen.width() - hud_w - 12.0, 12.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_premultiplied(12, 12, 18, 200))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::same(10))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
                .show(ui, |ui| {
                    ui.set_width(hud_w - 20.0);

                    histogram(
                        ui,
                        "Latency",
                        &stats.rtt_ms,
                        egui::Color32::from_rgb(250, 179, 135),
                        |v| format!("{v:.0} ms"),
                    );
                    ui.add_space(6.0);
                    histogram(
                        ui,
                        "FPS",
                        &stats.fps,
                        egui::Color32::from_rgb(166, 227, 161),
                        |v| format!("{v:.0}"),
                    );
                    ui.add_space(6.0);
                    histogram(
                        ui,
                        "Data",
                        &stats.mbps,
                        egui::Color32::from_rgb(137, 180, 250),
                        |v| format!("{v:.1} Mbit/s"),
                    );

                    ui.add_space(4.0);
                    ui.weak(format!(
                        "{codec} · decode {:.1} ms · F3 to close",
                        stats.decode_ms
                    ));
                });
        });
}

/// One labelled bar-histogram row: newest sample on the right, bars scaled
/// to the visible maximum (printed in the corner of the plot).
fn histogram(
    ui: &mut egui::Ui,
    title: &str,
    data: &std::collections::VecDeque<f32>,
    color: egui::Color32,
    fmt: impl Fn(f32) -> String,
) {
    let current = data.back().copied().unwrap_or(0.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).strong().size(13.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(fmt(current)).monospace().size(13.0).color(color));
        });
    });

    let height = 46.0f32;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_black_alpha(140));

    if data.is_empty() {
        return;
    }
    let max = data.iter().copied().fold(f32::EPSILON, f32::max);
    let bar_w = rect.width() / STATS_CAPACITY as f32;
    let usable_h = rect.height() - 4.0;
    // Newest sample is flush right; history grows leftwards.
    let offset = STATS_CAPACITY - data.len();
    for (i, v) in data.iter().enumerate() {
        let h = (v / max).clamp(0.0, 1.0) * usable_h;
        if h <= 0.0 {
            continue;
        }
        let x0 = rect.left() + (offset + i) as f32 * bar_w;
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0, rect.bottom() - 2.0 - h),
                egui::pos2(x0 + bar_w * 0.8, rect.bottom() - 2.0),
            ),
            0.0,
            color,
        );
    }
    painter.text(
        rect.left_top() + egui::vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        format!("max {}", fmt(max)),
        egui::FontId::proportional(10.0),
        egui::Color32::from_gray(150),
    );
}
