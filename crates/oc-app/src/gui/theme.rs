//! The visual theme of the GUI: a coherent color palette (dark base,
//! blue accent, semantic status colors) plus reusable building blocks —
//! cards, status dots, count badges, primary buttons, and avatars.
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

    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(14.0, 7.0);
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
            .stroke(Stroke::new(
                1.0_f32,
                Color32::from_rgba_unmultiplied(235, 100, 105, 110),
            )),
    )
}

/// A colored status dot followed by a label.
pub fn status(ui: &mut Ui, color: Color32, label: impl Into<RichText>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("\u{25cf}").color(color).size(11.0));
        ui.label(label.into().color(palette().text));
    });
}

/// A count badge (conflicts, errors) drawn next to labels.
pub fn badge(ui: &mut Ui, count: u64, color: Color32) {
    chip(Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        48,
    ))
    .show(ui, |ui| {
        ui.label(
            RichText::new(count.to_string())
                .color(color)
                .size(12.0)
                .strong(),
        );
    });
}

/// A small rounded square with the initial of a name (space/account).
pub fn avatar(ui: &mut Ui, initial: &str, color: Color32) {
    let size = Vec2::new(34.0, 34.0);
    ui.allocate_ui(size, |ui| {
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, palette().corner(9), Color32::from_rgb(43, 52, 64));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial,
            egui::FontId::proportional(17.0),
            color,
        );
    });
}

/// A section heading (slightly larger than body text).
pub fn heading(ui: &mut Ui, label: impl Into<RichText>) {
    ui.label(label.into().size(16.0).strong().color(palette().text));
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
