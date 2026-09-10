//! Look and feel.
//!
//! One palette, a handful of building blocks, and a few themes to choose
//! between - a bright room and a dark stage want different things, and the
//! person reading this at front of house at 1am is not the same person who set
//! it up at lunchtime.
//!
//! The palette is read at draw time rather than compiled in, so switching a
//! theme takes effect on the next frame.

use egui::{Color32, CornerRadius, FontFamily, FontId, Frame, Margin, Stroke, TextStyle, Ui, Vec2};
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// Every colour the interface draws with.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// True when the theme is dark, which decides egui's own base visuals.
    pub dark: bool,
    /// Behind everything.
    pub bg: Color32,
    /// Header, tab bar and status bar.
    pub panel: Color32,
    /// Cards and chips.
    pub surface: Color32,
    /// Borders and rules.
    pub line: Color32,
    pub text: Color32,
    /// Labels, hints, anything secondary.
    pub dim: Color32,
    pub accent: Color32,
    /// Rolling, connected, matching.
    pub good: Color32,
    /// Worth a look.
    pub warn: Color32,
    /// Wrong.
    pub bad: Color32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    /// The original: near-black with a teal accent.
    #[default]
    Midnight,
    /// Softer and bluer, for a lit room.
    Slate,
    /// Light, for daylight and for projectors.
    Daylight,
    /// Very dark, warm accent, nothing bright: a stage at night.
    Amber,
    /// Black and white with a hard accent, for tired eyes and bad screens.
    Contrast,
}

impl Theme {
    pub const ALL: [Theme; 5] = [
        Theme::Midnight,
        Theme::Slate,
        Theme::Daylight,
        Theme::Amber,
        Theme::Contrast,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Midnight => "Midnight",
            Theme::Slate => "Slate",
            Theme::Daylight => "Daylight",
            Theme::Amber => "Amber",
            Theme::Contrast => "High contrast",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Theme::Midnight => "near-black, teal accent",
            Theme::Slate => "softer and bluer, for a lit room",
            Theme::Daylight => "light, for daylight and projectors",
            Theme::Amber => "nothing bright: a stage at night",
            Theme::Contrast => "hard edges for tired eyes",
        }
    }

    pub fn palette(self) -> Palette {
        match self {
            Theme::Midnight => Palette {
                dark: true,
                bg: Color32::from_rgb(22, 24, 28),
                panel: Color32::from_rgb(28, 31, 36),
                surface: Color32::from_rgb(34, 38, 44),
                line: Color32::from_rgb(52, 57, 65),
                text: Color32::from_rgb(226, 230, 236),
                dim: Color32::from_rgb(138, 146, 158),
                accent: Color32::from_rgb(72, 168, 190),
                good: Color32::from_rgb(96, 190, 120),
                warn: Color32::from_rgb(224, 176, 74),
                bad: Color32::from_rgb(224, 100, 88),
            },
            Theme::Slate => Palette {
                dark: true,
                bg: Color32::from_rgb(38, 44, 54),
                panel: Color32::from_rgb(45, 52, 63),
                surface: Color32::from_rgb(54, 62, 75),
                line: Color32::from_rgb(74, 85, 101),
                text: Color32::from_rgb(232, 237, 243),
                dim: Color32::from_rgb(155, 167, 183),
                accent: Color32::from_rgb(122, 162, 247),
                good: Color32::from_rgb(126, 200, 145),
                warn: Color32::from_rgb(230, 186, 100),
                bad: Color32::from_rgb(232, 122, 112),
            },
            Theme::Daylight => Palette {
                dark: false,
                bg: Color32::from_rgb(242, 244, 246),
                panel: Color32::from_rgb(252, 253, 254),
                surface: Color32::from_rgb(255, 255, 255),
                line: Color32::from_rgb(206, 213, 219),
                text: Color32::from_rgb(24, 31, 37),
                dim: Color32::from_rgb(94, 107, 118),
                accent: Color32::from_rgb(14, 108, 133),
                good: Color32::from_rgb(31, 122, 68),
                warn: Color32::from_rgb(150, 100, 12),
                bad: Color32::from_rgb(176, 51, 40),
            },
            Theme::Amber => Palette {
                dark: true,
                bg: Color32::from_rgb(16, 14, 12),
                panel: Color32::from_rgb(24, 20, 16),
                surface: Color32::from_rgb(32, 27, 21),
                line: Color32::from_rgb(58, 48, 36),
                text: Color32::from_rgb(232, 210, 176),
                dim: Color32::from_rgb(150, 128, 100),
                accent: Color32::from_rgb(224, 150, 60),
                good: Color32::from_rgb(178, 168, 86),
                warn: Color32::from_rgb(226, 168, 72),
                bad: Color32::from_rgb(206, 96, 64),
            },
            Theme::Contrast => Palette {
                dark: true,
                bg: Color32::from_rgb(0, 0, 0),
                panel: Color32::from_rgb(12, 12, 12),
                surface: Color32::from_rgb(22, 22, 22),
                line: Color32::from_rgb(120, 120, 120),
                text: Color32::from_rgb(255, 255, 255),
                dim: Color32::from_rgb(190, 190, 190),
                accent: Color32::from_rgb(120, 210, 255),
                good: Color32::from_rgb(110, 240, 140),
                warn: Color32::from_rgb(255, 208, 80),
                bad: Color32::from_rgb(255, 120, 100),
            },
        }
    }
}

static PALETTE: RwLock<Palette> = RwLock::new(MIDNIGHT);
const MIDNIGHT: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(22, 24, 28),
    panel: Color32::from_rgb(28, 31, 36),
    surface: Color32::from_rgb(34, 38, 44),
    line: Color32::from_rgb(52, 57, 65),
    text: Color32::from_rgb(226, 230, 236),
    dim: Color32::from_rgb(138, 146, 158),
    accent: Color32::from_rgb(72, 168, 190),
    good: Color32::from_rgb(96, 190, 120),
    warn: Color32::from_rgb(224, 176, 74),
    bad: Color32::from_rgb(224, 100, 88),
};

pub fn palette() -> Palette {
    PALETTE.read().map(|p| *p).unwrap_or(MIDNIGHT)
}

pub fn bg() -> Color32 { palette().bg }
pub fn panel() -> Color32 { palette().panel }
pub fn surface() -> Color32 { palette().surface }
pub fn line() -> Color32 { palette().line }
pub fn text() -> Color32 { palette().text }
pub fn dim() -> Color32 { palette().dim }
pub fn accent() -> Color32 { palette().accent }
pub fn good() -> Color32 { palette().good }
pub fn warn() -> Color32 { palette().warn }
pub fn bad() -> Color32 { palette().bad }

/// Put a theme in force. Takes effect on the next frame.
pub fn apply(ctx: &egui::Context, theme: Theme) {
    if let Ok(mut current) = PALETTE.write() {
        *current = theme.palette();
    }
    install(ctx);
}

/// Push the palette into egui's own visuals, and set the spacing and type that
/// do not change between themes.
pub fn install(ctx: &egui::Context) {
    let p = palette();
    let mut visuals = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.panel;
    visuals.faint_bg_color = if p.dark {
        p.bg.gamma_multiply(1.25)
    } else {
        p.bg.gamma_multiply(0.97)
    };
    visuals.extreme_bg_color = if p.dark {
        p.bg.gamma_multiply(0.7)
    } else {
        Color32::WHITE
    };
    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.selection.bg_fill = p.accent.linear_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, p.text);
    visuals.hyperlink_color = p.accent;

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
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    let button = if p.dark { p.surface.gamma_multiply(1.3) } else { p.bg.gamma_multiply(0.94) };
    visuals.widgets.inactive.bg_fill = button;
    visuals.widgets.inactive.weak_bg_fill = button;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    let hovered = if p.dark { button.gamma_multiply(1.25) } else { button.gamma_multiply(0.94) };
    visuals.widgets.hovered.bg_fill = hovered;
    visuals.widgets.hovered.weak_bg_fill = hovered;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent.linear_multiply(0.6));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    visuals.widgets.active.bg_fill = p.accent.linear_multiply(0.55);
    visuals.widgets.active.weak_bg_fill = p.accent.linear_multiply(0.45);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
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
        .fill(surface())
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(12, 10))
        .stroke(Stroke::new(1.0, line()))
        .show(ui, add)
        .inner
}

/// A card with a title above it.
pub fn titled_card<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(title).color(dim()).small().strong());
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
        .fill(panel())
        .corner_radius(CornerRadius::same(11))
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                dot(ui, color);
                ui.label(egui::RichText::new(label).color(text()));
                if !detail.is_empty() {
                    ui.label(egui::RichText::new(detail).color(dim()).small());
                }
            });
        });
}

/// The one button on a screen that does the thing.
pub fn primary(label: &str) -> egui::Button<'static> {
    let p = palette();
    // Dark text on the accent when the accent is bright, light when it is not.
    let ink = if p.accent.r() as u32 + p.accent.g() as u32 + p.accent.b() as u32 > 420 {
        Color32::from_rgb(12, 22, 26)
    } else {
        Color32::WHITE
    };
    egui::Button::new(egui::RichText::new(label.to_owned()).color(ink).strong()).fill(p.accent)
}

/// A form label of consistent width, set against its field the way a
/// preferences pane does: right-aligned, so the controls line up.
pub fn field(ui: &mut Ui, label: &str) {
    ui.allocate_ui_with_layout(
        Vec2::new(150.0, 20.0),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.add(egui::Label::new(egui::RichText::new(label).color(dim())).truncate());
        },
    );
}

/// Column heading inside a table.
pub fn column(ui: &mut Ui, label: &str) {
    ui.label(egui::RichText::new(label).color(dim()).small().strong());
}

/// A number, monospaced so columns stop dancing.
pub fn num(ui: &mut Ui, label: impl Into<String>) {
    ui.label(egui::RichText::new(label.into()).monospace().color(text()));
}

/// Shown where a list would otherwise just be blank.
pub fn empty(ui: &mut Ui, message: &str) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(message).color(dim()));
    });
}
