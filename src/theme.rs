//! Look and feel: one palette, one set of building blocks.
//!
//! The bridge is read at a glance in a dark room while something else has your
//! attention, so the rules are: state is colour-coded and always in the same
//! place, numbers are monospaced so they stop jittering, and everything else
//! stays quiet.

use egui::{Color32, CornerRadius, FontFamily, FontId, Frame, Margin, Stroke, TextStyle, Ui, Vec2};

pub const BG: Color32 = Color32::from_rgb(22, 24, 28);
pub const PANEL: Color32 = Color32::from_rgb(28, 31, 36);
pub const CARD: Color32 = Color32::from_rgb(34, 38, 44);
pub const LINE: Color32 = Color32::from_rgb(52, 57, 65);
pub const TEXT: Color32 = Color32::from_rgb(226, 230, 236);
pub const DIM: Color32 = Color32::from_rgb(138, 146, 158);
pub const ACCENT: Color32 = Color32::from_rgb(72, 168, 190);
pub const GREEN: Color32 = Color32::from_rgb(96, 190, 120);
pub const AMBER: Color32 = Color32::from_rgb(224, 176, 74);
pub const RED: Color32 = Color32::from_rgb(224, 100, 88);

pub fn install(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.faint_bg_color = Color32::from_rgb(30, 33, 38);
    visuals.extreme_bg_color = Color32::from_rgb(18, 20, 23);
    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.selection.bg_fill = ACCENT.linear_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, TEXT);
    visuals.hyperlink_color = ACCENT;

    let radius = CornerRadius::same(5);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = radius;
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(44, 49, 56);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(40, 44, 51);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(56, 62, 71);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(52, 58, 66);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.linear_multiply(0.6));
    visuals.widgets.active.bg_fill = ACCENT.linear_multiply(0.55);
    visuals.widgets.active.weak_bg_fill = ACCENT.linear_multiply(0.45);
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 7.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.interact_size.y = 24.0;
        style.spacing.indent = 16.0;
        style.text_styles = [
            (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();
    });
}

/// A panel with a little breathing room, for grouping one idea.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(12, 10))
        .stroke(Stroke::new(1.0, LINE))
        .show(ui, add)
        .inner
}

/// A card with a title above it.
pub fn titled_card<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(title).color(DIM).small().strong());
    ui.add_space(3.0);
    card(ui, add)
}

/// A status dot, painted rather than drawn from a font: the bundled fonts have
/// no glyph for it.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// Dot, label and detail in one quiet pill - used for link state.
pub fn pill(ui: &mut Ui, color: Color32, label: &str, detail: &str) {
    Frame::new()
        .fill(PANEL)
        .corner_radius(CornerRadius::same(11))
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                dot(ui, color);
                ui.label(egui::RichText::new(label).color(TEXT));
                if !detail.is_empty() {
                    ui.label(egui::RichText::new(detail).color(DIM).small());
                }
            });
        });
}

/// The one button on a screen that does the thing.
pub fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_owned()).color(Color32::from_rgb(12, 22, 26)).strong())
        .fill(ACCENT)
}

/// A left-hand form label of consistent width.
pub fn field(ui: &mut Ui, label: &str) {
    ui.add_sized(
        [138.0, 20.0],
        egui::Label::new(egui::RichText::new(label).color(DIM)).halign(egui::Align::LEFT),
    );
}

/// Column heading inside a table.
pub fn column(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(DIM).small().strong());
}

/// A number, monospaced so columns stop dancing.
pub fn num(ui: &mut Ui, text: impl Into<String>) {
    ui.label(egui::RichText::new(text.into()).monospace().color(TEXT));
}

/// Shown where a list would otherwise just be blank.
pub fn empty(ui: &mut Ui, message: &str) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(message).color(DIM));
    });
}
