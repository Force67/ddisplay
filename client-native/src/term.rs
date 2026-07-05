/// In-client terminal window (toggle with F4): a PTY on the server rendered
/// alongside the video stream (docs/terminal.md).
///
/// The remote shell produces the byte stream; vt100 turns it into a cell
/// grid drawn as one LayoutJob per row. Keystrokes go to the PTY only while
/// the grid widget has egui focus; everything else keeps reaching the
/// remote desktop.

use crate::protocol;
use crate::transport::TransportSender;

const FONT_SIZE: f32 = 15.0;
const MIN_COLS: u16 = 10;
const MIN_ROWS: u16 = 4;
const MAX_DIM: u16 = 500;
const BG: egui::Color32 = egui::Color32::from_rgb(14, 14, 20);
const FG: egui::Color32 = egui::Color32::from_gray(220);

#[derive(PartialEq, Clone, Copy)]
enum Session {
    Closed,
    Open,
    Exited(u8),
}

pub struct TermUi {
    sender: TransportSender,
    parser: vt100::Parser,
    session: Session,
    pub visible: bool,
    cols: u16,
    rows: u16,
}

impl TermUi {
    pub fn new(sender: TransportSender) -> Self {
        Self {
            sender,
            parser: vt100::Parser::new(24, 80, 0),
            session: Session::Closed,
            visible: false,
            cols: 80,
            rows: 24,
        }
    }

    /// F4: show/hide the window. Showing with no live shell opens a fresh one;
    /// hiding keeps the shell running.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        if self.visible && self.session != Session::Open {
            self.parser = vt100::Parser::new(self.rows, self.cols, 0);
            self.session = Session::Open;
            self.sender
                .send(protocol::encode_term_open(self.cols, self.rows, "xterm-256color"));
        }
    }

    /// PTY output from the server.
    pub fn on_data(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    /// The remote shell exited.
    pub fn on_exit(&mut self, code: u8) {
        self.session = Session::Exited(code);
    }

    /// The websocket dropped: the server-side PTY died with it.
    pub fn on_disconnect(&mut self) {
        if self.session == Session::Open {
            self.session = Session::Closed;
        }
    }

    /// Draw the terminal window into the current egui pass.
    pub fn ui(&mut self, ctx: &egui::Context) {
        if !self.visible {
            return;
        }
        let font = egui::FontId::monospace(FONT_SIZE);
        let (cell_w, cell_h) =
            ctx.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));

        egui::Window::new("Terminal")
            .default_size([cell_w * 80.0 + 16.0, cell_h * 24.0 + 24.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                self.grid_ui(ui, &font, cell_w, cell_h);
            });
    }

    fn grid_ui(&mut self, ui: &mut egui::Ui, font: &egui::FontId, cell_w: f32, cell_h: f32) {
        // Fit the grid to the window, and tell the PTY when it changes.
        let avail = ui.available_size() - egui::vec2(0.0, cell_h); // room for the footer line
        let cols = ((avail.x / cell_w) as u16).clamp(MIN_COLS, MAX_DIM);
        let rows = ((avail.y / cell_h) as u16).clamp(MIN_ROWS, MAX_DIM);
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.parser.set_size(rows, cols);
            if self.session == Session::Open {
                self.sender.send(protocol::encode_term_resize(cols, rows));
            }
        }

        let size = egui::vec2(cols as f32 * cell_w, rows as f32 * cell_h);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        if response.clicked() {
            response.request_focus();
        } else if response.clicked_elsewhere() {
            // Clicking the desktop (or another panel) hands the keyboard back.
            response.surrender_focus();
        }
        let focused = response.has_focus();
        if focused {
            // Keep Tab/arrows/Esc for the shell instead of egui navigation.
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
            self.forward_input(ui);
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, BG);
        self.draw_grid(&painter, rect, font, cell_w, cell_h, focused);

        match self.session {
            Session::Exited(code) => {
                ui.weak(format!("shell exited ({code}) · F4 twice for a new one"));
            }
            _ if !focused => {
                ui.weak("click the terminal to type · F4 to hide");
            }
            _ => {
                ui.weak(format!("{cols}x{rows} · F4 to hide"));
            }
        }
    }

    /// Translate this frame's egui input into PTY bytes.
    fn forward_input(&mut self, ui: &egui::Ui) {
        let events = ui.input(|i| i.events.clone());
        let app_cursor = self.parser.screen().application_cursor();
        let mut out: Vec<u8> = Vec::new();
        for ev in events {
            match ev {
                egui::Event::Text(t) => out.extend_from_slice(t.as_bytes()),
                egui::Event::Paste(t) => out.extend_from_slice(t.as_bytes()),
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if let Some(bytes) = key_bytes(key, &modifiers, app_cursor) {
                        out.extend_from_slice(&bytes);
                    }
                }
                _ => {}
            }
        }
        if !out.is_empty() && self.session == Session::Open {
            self.sender.send(protocol::encode_term_data(&out));
        }
    }

    fn draw_grid(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        font: &egui::FontId,
        cell_w: f32,
        cell_h: f32,
        focused: bool,
    ) {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        for r in 0..rows {
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = f32::INFINITY;
            for c in 0..cols {
                let Some(cell) = screen.cell(r, c) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                let text = if contents.is_empty() { " " } else { contents.as_str() };
                let mut fg = color32(cell.fgcolor(), FG);
                let mut bg = color32(cell.bgcolor(), BG);
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                job.append(
                    text,
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: fg,
                        background: bg,
                        italics: cell.italic(),
                        underline: if cell.underline() {
                            egui::Stroke::new(1.0, fg)
                        } else {
                            egui::Stroke::NONE
                        },
                        ..Default::default()
                    },
                );
            }
            let galley = painter.layout_job(job);
            painter.galley(
                rect.min + egui::vec2(0.0, r as f32 * cell_h),
                galley,
                FG,
            );
        }

        if !screen.hide_cursor() && self.session == Session::Open {
            let (cr, cc) = screen.cursor_position();
            let cursor = egui::Rect::from_min_size(
                rect.min + egui::vec2(cc as f32 * cell_w, cr as f32 * cell_h),
                egui::vec2(cell_w, cell_h),
            );
            if focused {
                painter.rect_filled(
                    cursor,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(220, 220, 220, 130),
                );
            } else {
                painter.rect_stroke(
                    cursor,
                    0.0,
                    egui::Stroke::new(1.0, egui::Color32::from_gray(160)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

/// Bytes for a non-text key press, or None when the Text event covers it.
fn key_bytes(key: egui::Key, mods: &egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    use egui::Key;
    // Ctrl combos produce no Text event; map them to control codes.
    if mods.ctrl && !mods.alt {
        if let Some(code) = ctrl_code(key) {
            return Some(vec![code]);
        }
    }
    let seq = |s: &str| Some(s.as_bytes().to_vec());
    match key {
        Key::Enter => Some(vec![b'\r']),
        Key::Tab => Some(vec![b'\t']),
        Key::Backspace => Some(vec![0x7f]),
        Key::Escape => Some(vec![0x1b]),
        Key::ArrowUp => seq(if app_cursor { "\x1bOA" } else { "\x1b[A" }),
        Key::ArrowDown => seq(if app_cursor { "\x1bOB" } else { "\x1b[B" }),
        Key::ArrowRight => seq(if app_cursor { "\x1bOC" } else { "\x1b[C" }),
        Key::ArrowLeft => seq(if app_cursor { "\x1bOD" } else { "\x1b[D" }),
        Key::Home => seq("\x1b[H"),
        Key::End => seq("\x1b[F"),
        Key::PageUp => seq("\x1b[5~"),
        Key::PageDown => seq("\x1b[6~"),
        Key::Delete => seq("\x1b[3~"),
        Key::Insert => seq("\x1b[2~"),
        _ => None,
    }
}

fn ctrl_code(key: egui::Key) -> Option<u8> {
    use egui::Key;
    Some(match key {
        Key::Space => 0x00,
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        _ => return None,
    })
}

fn color32(color: vt100::Color, default: egui::Color32) -> egui::Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => idx_color(i),
        vt100::Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
    }
}

/// xterm 256-color palette: 16 base colors, 6x6x6 cube, grayscale ramp.
fn idx_color(i: u8) -> egui::Color32 {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    match i {
        0..=15 => {
            let (r, g, b) = BASE[i as usize];
            egui::Color32::from_rgb(r, g, b)
        }
        16..=231 => {
            let v = i - 16;
            let comp = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
            egui::Color32::from_rgb(comp(v / 36), comp((v % 36) / 6), comp(v % 6))
        }
        232..=255 => egui::Color32::from_gray(8 + 10 * (i - 232)),
    }
}
