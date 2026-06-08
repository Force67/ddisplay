/// Egui-based overlay menu (toggle with F2).
///
/// Shows: connection info, display mode selector, file transfer panel.

use egui_winit::winit;

use crate::files::FileTransferState;

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
}

pub struct OverlayState {
    pub ctx: egui::Context,
    pub winit_state: egui_winit::State,
    pub visible: bool,
    pub display_mode: DisplayMode,
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
            display_mode: DisplayMode::default(),
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
        window: &winit::window::Window,
        files: Option<&mut FileTransferState>,
    ) -> (Option<EguiRenderData>, OverlayAction) {
        // Always drain accumulated winit input so it doesn't pile up while hidden.
        let raw_input = self.winit_state.take_egui_input(window);

        if !self.visible {
            return (None, OverlayAction::None);
        }

        self.ctx.begin_pass(raw_input);

        let screen = self.ctx.screen_rect();
        let panel_w = 380.0f32;
        let panel_x = (screen.width() - panel_w) * 0.5;
        let panel_y = (screen.height() - 360.0f32) * 0.5;

        let mut action = OverlayAction::None;

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
                        ui.weak("Press F2 to close");
                    });
            });

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
