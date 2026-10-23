//! The visual theme of the GUI: a coherent color palette (dark base,
//! blue accent, semantic status colors) plus reusable building blocks —
//! cards, status pills, count badges, quota bars, primary buttons,
//! avatars, and the centered content column every tab lives in.
//!
//! Everything the UI paints goes through here, so the look can be tuned
//! in one place (`palette()` / [`apply`]).

use eframe::egui;
use egui::Button;
use egui::Color32;
use egui::Context;
use egui::Frame;
use egui::Margin;
use egui::Response;
use egui::RichText;
use egui::Stroke;
use egui::Ui;
use egui::Vec2;

/// Font scale applied to the built-in fonts (1.06 reads friendlier at
/// desktop sizes than the tiny egui default).
const FONT_SCALE: f32 = 1.06;

/// Maximum width of the content column: on wide windows the cards stay
/// readable instead of stretching across the whole width.
pub const CONTENT_MAX_WIDTH: f32 = 960.0;

/// The palette of the app.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Brand/accent color (buttons, selection, tabs).
    pub accent: Color32,
    /// Accent with low alpha (avatars, subtle fills).
    pub accent_dim: Color32,
    /// Success (synced, logged in).
    pub success: Color32,
    /// Warnings (paused).
    pub warning: Color32,
    /// Errors and conflicts.
    pub error: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text (paths, timestamps).
    pub text_dim: Color32,
    /// Card background.
    pub card: Color32,
    /// Card border.
    pub card_stroke: Color32,
    /// Window/panel background.
    pub panel: Color32,
    /// Elevated surfaces (dialogs).
    pub window: Color32,
    /// Input fields.
    pub input: Color32,
}

/// The one palette used by the whole UI.
pub fn palette() -> Palette {
    Palette {
        accent: Color32::from_rgb(64, 140, 255),
        accent_dim: Color32::from_rgba_unmultiplied(64, 140, 255, 36),
        success: Color32::from_rgb(70, 190, 120),
        warning: Color32::from_rgb(235, 175, 70),
        error: Color32::from_rgb(235, 100, 105),
        text: Color32::from_rgb(232, 234, 237),
        text_dim: Color32::from_rgb(150, 156, 165),
        card: Color32::from_rgb(33, 36, 42),
        card_stroke: Color32::from_rgb(46, 50, 58),
        panel: Color32::from_rgb(22, 24, 28),
        window: Color32::from_rgb(28, 31, 36),
        input: Color32::from_rgb(17, 19, 22),
    }
}

/// Applies the theme: dark mode, fonts (slightly scaled), rounded widgets,
/// the palette, and comfortable spacing.
pub fn apply(ctx: &Context) {
    ctx.set_theme(egui::Theme::Dark);
    scale_fonts(ctx);

    let theme = palette();
    let mut style = (*ctx.style()).clone();
    let visuals = &mut style.visuals;
    visuals.dark_mode = true;
    visuals.panel_fill = theme.panel;
    visuals.window_fill = theme.window;
    visuals.extreme_bg_color = theme.input;
    visuals.faint_bg_color = theme.card;
    visuals.hyperlink_color = theme.accent;
    visuals.warn_fg_color = theme.warning;
    visuals.error_fg_color = theme.error;
    visuals.selection.bg_fill = theme.accent_dim;
    visuals.selection.stroke = Stroke::new(1.0_f32, theme.accent);
    visuals.window_corner_radius = theme.corner(12);
    visuals.menu_corner_radius = theme.corner(10);

    let card_stroke = Stroke::new(1.0_f32, theme.card_stroke);
    visuals.widgets.noninteractive.weak_bg_fill = theme.card;
    visuals.widgets.noninteractive.bg_fill = theme.card;
    visuals.widgets.noninteractive.bg_stroke = card_stroke;
    visuals.widgets.noninteractive.fg_stroke.color = theme.text_dim;
    visuals.widgets.noninteractive.corner_radius = theme.corner(8);
    visuals.widgets.inactive.weak_bg_fill = theme.card;
    visuals.widgets.inactive.bg_fill = theme.card;
    visuals.widgets.inactive.bg_stroke = card_stroke;
    visuals.widgets.inactive.fg_stroke.color = theme.text;
    visuals.widgets.inactive.corner_radius = theme.corner(8);
    visuals.widgets.hovered.weak_bg_fill = theme.accent_dim;
    visuals.widgets.hovered.fg_stroke.color = theme.text;
    visuals.widgets.hovered.corner_radius = theme.corner(8);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgba_unmultiplied(64, 140, 255, 70);
    visuals.widgets.active.fg_stroke.color = theme.text;
    visuals.widgets.active.corner_radius = theme.corner(8);
    visuals.widgets.open.weak_bg_fill = theme.card;
    visuals.widgets.open.corner_radius = theme.corner(8);

    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size.y = 21.0;
    ctx.set_style(style);
}

impl Palette {
    /// Corner radius helper (`u8`-based).
    fn corner(&self, radius: u8) -> egui::CornerRadius {
        egui::CornerRadius::same(radius)
    }
}

/// Scales the built-in fonts by [`FONT_SCALE`] (visual only).
fn scale_fonts(ctx: &Context) {
    let mut fonts = egui::FontDefinitions::default();
    let original = fonts.font_data.clone();
    for (name, data) in original {
        let mut data = (*data).clone();
        data.tweak.scale = FONT_SCALE;
        fonts.font_data.insert(name, std::sync::Arc::new(data));
    }
    ctx.set_fonts(fonts);
}

/// A centered column of at most [`CONTENT_MAX_WIDTH`]: the tabs and the
/// wizard render inside it, so cards keep a readable width on wide
/// windows. Narrower windows get the full available width.
pub fn content_column<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let available = ui.available_width();
    let width = available.min(CONTENT_MAX_WIDTH);
    let margin = ((available - width) / 2.0).max(0.0);
    ui.horizontal_top(|ui| {
        ui.add_space(margin);
        ui.vertical(|ui| {
            ui.set_width(width);
            add_contents(ui)
        })
        .inner
    })
    .inner
}

/// A rounded card (job rows, accounts, conflicts, wizard steps).
pub fn card() -> Frame {
    let theme = palette();
    Frame::NONE
        .fill(theme.card)
        .stroke(Stroke::new(1.0_f32, theme.card_stroke))
        .corner_radius(theme.corner(10))
        .inner_margin(Margin::same(14))
}

/// A subtle chip (badges, step numbers) with the given fill.
pub fn chip(fill: Color32) -> Frame {
    Frame::NONE
        .fill(fill)
        .corner_radius(palette().corner(8))
        .inner_margin(Margin::symmetric(8, 3))
}

/// A primary (accent-filled) button; returns the clicked response.
pub fn primary(ui: &mut Ui, label: impl Into<egui::WidgetText>) -> Response {
    let theme = palette();
    ui.add(
        Button::new(label)
            .fill(theme.accent)
            .corner_radius(theme.corner(8))
            .stroke(Stroke::NONE),
    )
}

/// A quiet button (less visual weight than a normal one).
pub fn quiet(ui: &mut Ui, label: impl Into<egui::WidgetText>) -> Response {
    let theme = palette();
    ui.add(
        Button::new(label)
            .fill(Color32::TRANSPARENT)
            .corner_radius(theme.corner(8))
            .stroke(Stroke::new(1.0_f32, theme.card_stroke)),
    )
}

/// A destructive button (remove job, logout).
pub fn danger(ui: &mut Ui, label: impl Into<egui::WidgetText>) -> Response {
    let theme = palette();
    ui.add(
        Button::new(label)
            .fill(Color32::TRANSPARENT)
            .corner_radius(theme.corner(8))
            .stroke(Stroke::new(1.0_f32, tint(theme.error, 110))),
    )
}

/// The centered placeholder of an empty tab: a title, a hint, and an
/// optional primary action; returns whether the action was clicked.
pub fn empty_state(
    ui: &mut Ui,
    title: &str,
    title_color: Color32,
    hint: &str,
    action: Option<&str>,
) -> bool {
    let theme = palette();
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space(70.0);
        ui.label(
            egui::RichText::new(title)
                .size(20.0)
                .strong()
                .color(title_color),
        );
        ui.add_space(6.0);
        ui.label(egui::RichText::new(hint).color(theme.text_dim));
        if let Some(action) = action {
            ui.add_space(14.0);
            clicked = primary(ui, action).clicked();
        }
    });
    clicked
}

/// A square quiet button with a single glyph (refresh, pause, menu) and
/// a tooltip that names the action.
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> Response {
    let theme = palette();
    ui.add(
        Button::new(RichText::new(glyph).size(14.0).color(theme.text))
            .fill(Color32::TRANSPARENT)
            .corner_radius(theme.corner(8))
            .stroke(Stroke::new(1.0_f32, theme.card_stroke))
            .min_size(Vec2::new(30.0, 30.0)),
    )
    .on_hover_text(tooltip)
}

/// A single-line label that ellipsizes when it does not fit the row
/// (paths, URLs), so right-aligned buttons never overlap the text.
pub fn ellipsis(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    ui.add(egui::Label::new(text).truncate())
}

/// A colored status dot followed by a label (status line, empty states).
pub fn status(ui: &mut Ui, color: Color32, label: impl Into<RichText>) {
    ui.horizontal(|ui| {
        dot(ui, color, 4.0);
        ui.label(label.into().color(palette().text));
    });
}

/// A status pill: tinted rounded box with a colored dot and a short
/// label (login state, job state) — compact enough for card headers.
/// Painted directly, so it looks the same in left- and right-aligned rows.
pub fn pill(ui: &mut Ui, color: Color32, label: impl Into<String>) {
    let galley = ui
        .painter()
        .layout_no_wrap(label.into(), egui::FontId::proportional(12.5), color);
    let (pad_x, pad_y, dot_radius, gap) = (9.0, 4.0, 3.5, 6.0);
    let size = Vec2::new(
        pad_x * 2.0 + dot_radius * 2.0 + gap + galley.size().x,
        (pad_y * 2.0 + galley.size().y).max(24.0),
    );
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, palette().corner(8), tint(color, 40));
    let dot_center = egui::pos2(rect.left() + pad_x + dot_radius, rect.center().y);
    painter.circle_filled(dot_center, dot_radius, color);
    let text_pos = egui::pos2(
        dot_center.x + dot_radius + gap,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.galley(text_pos, galley, color);
}

/// A count badge (conflicts, errors) drawn next to labels.
pub fn badge(ui: &mut Ui, count: u64, color: Color32) {
    chip(tint(color, 48)).show(ui, |ui| {
        ui.label(
            RichText::new(count.to_string())
                .color(color)
                .size(12.0)
                .strong(),
        );
    });
}

/// A thin quota bar with the used/total text next to it. Without a
/// known total (`None` or `0`, i.e. unlimited) only the used amount is
/// shown.
pub fn quota_bar(ui: &mut Ui, used: u64, total: Option<u64>, format: impl Fn(u64) -> String) {
    let theme = palette();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        match total {
            Some(total) if total > 0 => {
                let fraction = (used as f64 / total as f64).clamp(0.0, 1.0) as f32;
                let color = if fraction > 0.9 {
                    theme.error
                } else if fraction > 0.75 {
                    theme.warning
                } else {
                    theme.accent
                };
                mini_bar(ui, fraction, color);
                ui.label(
                    RichText::new(format!("{} / {}", format(used), format(total)))
                        .small()
                        .color(theme.text_dim),
                );
            }
            _ => {
                ui.label(
                    RichText::new(format!("{} belegt", format(used)))
                        .small()
                        .color(theme.text_dim),
                );
            }
        }
    });
}

/// A thin painted progress bar (120×6 px) filled to `fraction`
/// (`0.0`–`1.0`); a started but tiny fraction still shows a sliver.
pub fn mini_bar(ui: &mut Ui, fraction: f32, color: Color32) {
    let theme = palette();
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(120.0, 6.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, theme.corner(3), theme.card_stroke);
    let mut filled = rect;
    filled.set_width((rect.width() * fraction).max(if fraction > 0.0 { 3.0 } else { 0.0 }));
    painter.rect_filled(filled, theme.corner(3), color);
}

/// A small painted triangle pointing up (`up`) or down — the direction
/// of a transfer. Painted, like [`dot`]: the bundled fonts have no
/// reliable arrow glyphs.
pub fn arrow(ui: &mut Ui, up: bool, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
    let rect = rect.shrink(2.0);
    let points = if up {
        vec![rect.left_bottom(), rect.center_top(), rect.right_bottom()]
    } else {
        vec![rect.left_top(), rect.center_bottom(), rect.right_top()]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
}

/// A small rounded square with the initial of a name (space/account).
pub fn avatar(ui: &mut Ui, initial: &str, color: Color32, size: f32) {
    let size = Vec2::splat(size);
    ui.allocate_ui(size, |ui| {
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, palette().corner(9), Color32::from_rgb(43, 52, 64));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial,
            egui::FontId::proportional(size.y / 2.0),
            color,
        );
    });
}

/// A section heading (slightly larger than body text).
pub fn heading(ui: &mut Ui, label: impl Into<RichText>) {
    ui.label(label.into().size(16.0).strong().color(palette().text));
}

/// A small, dimmed, upper-case label that groups rows inside a card
/// ("SPACES", "LETZTER LAUF").
pub fn section_label(ui: &mut Ui, label: &str) {
    ui.label(
        RichText::new(label.to_uppercase())
            .size(11.0)
            .strong()
            .color(palette().text_dim),
    );
}

/// A hairline separator inside a card, with vertical breathing room.
pub fn divider(ui: &mut Ui) {
    ui.add_space(6.0);
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, palette().card_stroke);
    ui.add_space(6.0);
}

/// A step number in a filled circle (wizard).
pub fn step(ui: &mut Ui, number: u32, label: &str) {
    let theme = palette();
    ui.horizontal(|ui| {
        chip(theme.accent_dim).show(ui, |ui| {
            ui.label(
                RichText::new(format!("{number}"))
                    .strong()
                    .color(theme.accent)
                    .size(13.0),
            );
        });
        ui.label(RichText::new(label).strong().size(15.0).color(theme.text));
    });
    ui.add_space(2.0);
}

/// A filled circle of the given radius, vertically centered in the row
/// (painted: the bundled fonts have no reliable bullet glyph).
pub fn dot(ui: &mut Ui, color: Color32, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 2.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}

/// The color with the given alpha (tinted fills behind colored text).
fn tint(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}
