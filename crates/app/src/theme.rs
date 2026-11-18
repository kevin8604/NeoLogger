use eframe::egui::{
    self, pos2, vec2, Color32, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText,
    Rounding, Sense, Stroke, TextStyle, Ui, Vec2,
};
use neolog_core::Rgb;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Runtime palette. Every color the app paints by hand comes from here, so a
/// theme switch only has to swap one table and rebuild the egui style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub dark: bool,
    pub text: Color32,
    pub dim: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent_strong: Color32,
    pub accent_soft: Color32,
    pub bookmark: Color32,
    pub ok: Color32,
    pub bg: Color32,
    pub chrome: Color32,
    pub surface: Color32,
    pub surface_high: Color32,
    pub border: Color32,
    pub border_soft: Color32,
    pub extreme_bg: Color32,
    pub hovered_bg: Color32,
    pub row_even: Color32,
    pub row_odd: Color32,
    pub hover: Color32,
    pub selected_row: Color32,
    pub selection: Color32,
    pub find_bg: Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    Contrast,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::Dark, Theme::Light, Theme::Contrast];

    pub fn id(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::Contrast => "contrast",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme::Dark => "深色",
            Theme::Light => "浅色",
            Theme::Contrast => "高对比",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.id() == id)
    }
}

const DARK: Palette = Palette {
    dark: true,
    text: Color32::from_rgb(230, 233, 240),
    dim: Color32::from_rgb(150, 158, 173),
    faint: Color32::from_rgb(96, 104, 119),
    accent: Color32::from_rgb(125, 140, 255),
    accent_strong: Color32::from_rgb(166, 177, 255),
    accent_soft: Color32::from_rgb(48, 56, 100),
    bookmark: Color32::from_rgb(255, 199, 95),
    ok: Color32::from_rgb(74, 205, 150),
    bg: Color32::from_rgb(11, 13, 17),
    chrome: Color32::from_rgb(15, 18, 24),
    surface: Color32::from_rgb(20, 24, 31),
    surface_high: Color32::from_rgb(27, 32, 42),
    border: Color32::from_rgb(40, 46, 58),
    border_soft: Color32::from_rgb(29, 34, 44),
    extreme_bg: Color32::from_rgb(9, 11, 15),
    hovered_bg: Color32::from_rgb(34, 40, 54),
    row_even: Color32::from_rgb(14, 17, 22),
    row_odd: Color32::from_rgb(17, 21, 28),
    hover: Color32::from_rgba_premultiplied(6, 6, 6, 6),
    selected_row: Color32::from_rgba_premultiplied(15, 16, 30, 30),
    selection: Color32::from_rgba_premultiplied(42, 47, 85, 85),
    find_bg: Rgb(104, 82, 22),
};

const LIGHT: Palette = Palette {
    dark: false,
    text: Color32::from_rgb(32, 37, 48),
    dim: Color32::from_rgb(92, 101, 117),
    faint: Color32::from_rgb(138, 146, 160),
    accent: Color32::from_rgb(79, 99, 224),
    accent_strong: Color32::from_rgb(54, 71, 187),
    accent_soft: Color32::from_rgb(223, 228, 255),
    bookmark: Color32::from_rgb(166, 108, 0),
    ok: Color32::from_rgb(22, 130, 93),
    bg: Color32::from_rgb(243, 245, 249),
    chrome: Color32::from_rgb(236, 239, 245),
    surface: Color32::from_rgb(255, 255, 255),
    surface_high: Color32::from_rgb(244, 246, 250),
    border: Color32::from_rgb(205, 212, 224),
    border_soft: Color32::from_rgb(228, 232, 240),
    extreme_bg: Color32::from_rgb(255, 255, 255),
    hovered_bg: Color32::from_rgb(233, 237, 246),
    row_even: Color32::from_rgb(255, 255, 255),
    row_odd: Color32::from_rgb(248, 250, 253),
    hover: Color32::from_rgba_premultiplied(0, 0, 0, 8),
    selected_row: Color32::from_rgba_premultiplied(22, 30, 90, 16),
    selection: Color32::from_rgba_premultiplied(40, 60, 180, 60),
    find_bg: Rgb(255, 226, 138),
};

const CONTRAST: Palette = Palette {
    dark: true,
    text: Color32::from_rgb(255, 255, 255),
    dim: Color32::from_rgb(205, 212, 224),
    faint: Color32::from_rgb(160, 170, 186),
    accent: Color32::from_rgb(90, 220, 255),
    accent_strong: Color32::from_rgb(190, 242, 255),
    accent_soft: Color32::from_rgb(0, 70, 95),
    bookmark: Color32::from_rgb(255, 214, 0),
    ok: Color32::from_rgb(0, 255, 160),
    bg: Color32::from_rgb(0, 0, 0),
    chrome: Color32::from_rgb(8, 8, 10),
    surface: Color32::from_rgb(16, 16, 20),
    surface_high: Color32::from_rgb(28, 28, 34),
    border: Color32::from_rgb(120, 130, 150),
    border_soft: Color32::from_rgb(58, 64, 76),
    extreme_bg: Color32::from_rgb(0, 0, 0),
    hovered_bg: Color32::from_rgb(40, 44, 54),
    row_even: Color32::from_rgb(0, 0, 0),
    row_odd: Color32::from_rgb(14, 14, 18),
    hover: Color32::from_rgba_premultiplied(60, 60, 60, 40),
    selected_row: Color32::from_rgba_premultiplied(20, 60, 90, 90),
    selection: Color32::from_rgba_premultiplied(40, 120, 160, 120),
    find_bg: Rgb(150, 100, 0),
};

const PALETTES: [Palette; 3] = [DARK, LIGHT, CONTRAST];
static CURRENT: AtomicUsize = AtomicUsize::new(0);

pub fn current() -> Theme {
    Theme::ALL[CURRENT.load(Ordering::Relaxed).min(PALETTES.len() - 1)]
}

pub fn set_theme(theme: Theme) {
    let index = Theme::ALL
        .iter()
        .position(|candidate| *candidate == theme)
        .unwrap_or(0);
    CURRENT.store(index, Ordering::Relaxed);
}

/// Active palette, read lock-free on every paint.
pub fn p() -> &'static Palette {
    &PALETTES[CURRENT.load(Ordering::Relaxed).min(PALETTES.len() - 1)]
}

const DOC_PALETTE: [Color32; 8] = [
    Color32::from_rgb(91, 141, 239),
    Color32::from_rgb(186, 133, 240),
    Color32::from_rgb(63, 191, 159),
    Color32::from_rgb(224, 163, 92),
    Color32::from_rgb(110, 139, 255),
    Color32::from_rgb(224, 108, 117),
    Color32::from_rgb(86, 182, 194),
    Color32::from_rgb(201, 180, 88),
];

/// Rounded card used for sidebar sections and the filter strip.
pub fn card() -> egui::Frame {
    let p = p();
    egui::Frame::none()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border_soft))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(10.0, 8.0))
        .outer_margin(Margin::symmetric(0.0, 4.0))
}

/// Small rounded label for counts and statuses.
pub fn chip(ui: &mut Ui, text: impl Into<String>, fg: Color32, bg: Color32) -> egui::Response {
    egui::Frame::none()
        .fill(bg)
        .rounding(Rounding::same(5.0))
        .inner_margin(Margin::symmetric(6.0, 1.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).size(10.5).color(fg));
        })
        .response
}

/// Primary action: accent fill with contrast text.
pub fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
    let p = p();
    ui.add(
        egui::Button::new(RichText::new(text).size(12.5).color(p.bg))
            .fill(p.accent)
            .stroke(Stroke::NONE)
            .rounding(Rounding::same(7.0))
            .min_size(Vec2::new(0.0, 26.0)),
    )
}

/// Log-mark logo: accent tile with three dark lines.
pub fn brand(ui: &mut Ui) {
    let p = p();
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, Rounding::same(7.0), p.accent);
    for idx in 0..3 {
        let inset = if idx == 1 { 6.0 } else { 7.0 };
        let y = rect.top() + 8.0 + idx as f32 * 4.0;
        painter.line_segment(
            [pos2(rect.left() + inset, y), pos2(rect.right() - inset, y)],
            Stroke::new(1.6_f32, p.bg),
        );
    }
}

/// Hairline used between the chrome and content.
pub fn hairline(ui: &mut Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p().border_soft);
}

pub fn color(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.0, rgb.1, rgb.2)
}

/// Text color for plugin-provided accents (severity, decorate spans) and
/// keyword highlights. The log palette is tuned for dark canvases; on the light
/// theme the color is blended toward the text color so it stays readable.
pub fn ink(rgb: Rgb) -> Color32 {
    if p().dark {
        return color(rgb);
    }
    const MIX: f32 = 0.62;
    let text = p().text;
    let blend = |channel: u8, target: u8| {
        (channel as f32 * MIX + target as f32 * (1.0 - MIX)).round() as u8
    };
    Color32::from_rgb(
        blend(rgb.0, text.r()),
        blend(rgb.1, text.g()),
        blend(rgb.2, text.b()),
    )
}

pub fn doc_color(doc: u32) -> Color32 {
    DOC_PALETTE[doc as usize % DOC_PALETTE.len()]
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    if let Ok(bytes) = std::fs::read("C:/Windows/Fonts/segoeui.ttf") {
        fonts
            .font_data
            .insert("ui".to_owned(), FontData::from_owned(bytes));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "ui".to_owned());
    }

    let mut cjk = None;
    for path in [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), FontData::from_owned(bytes));
            cjk = Some("cjk".to_owned());
            break;
        }
    }

    let mut bold_font = "Hack".to_owned();
    for path in [
        "C:/Windows/Fonts/consolab.ttf",
        "C:/Windows/Fonts/msyhbd.ttc",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("mono-bold".to_owned(), FontData::from_owned(bytes));
            bold_font = "mono-bold".to_owned();
            break;
        }
    }
    let mut bold_family = vec![bold_font];
    if let Some(cjk) = &cjk {
        bold_family.push(cjk.clone());
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(cjk.clone());
        }
    }
    fonts
        .families
        .insert(FontFamily::Name("mono-bold".into()), bold_family);

    ctx.set_fonts(fonts);
}

pub fn apply_style(ctx: &egui::Context) {
    let p = p();
    if p.dark {
        ctx.set_theme(egui::ThemePreference::Dark);
        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
    } else {
        ctx.set_theme(egui::ThemePreference::Light);
        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Light));
    }
    let mut style = (*ctx.style()).clone();
    let mut visuals = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.panel_fill = p.bg;
    visuals.window_fill = p.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_rounding = Rounding::same(11.0);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: vec2(0.0, 12.0),
        blur: 32.0,
        spread: 0.0,
        color: if p.dark {
            Color32::from_black_alpha(140)
        } else {
            Color32::from_black_alpha(40)
        },
    };
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: vec2(0.0, 6.0),
        blur: 18.0,
        spread: 0.0,
        color: if p.dark {
            Color32::from_black_alpha(120)
        } else {
            Color32::from_black_alpha(32)
        },
    };
    visuals.menu_rounding = Rounding::same(8.0);
    visuals.extreme_bg_color = p.extreme_bg;
    visuals.faint_bg_color = p.surface;
    visuals.code_bg_color = p.surface_high;
    visuals.override_text_color = Some(p.text);
    visuals.hyperlink_color = p.accent_strong;
    visuals.text_cursor.stroke = Stroke::new(2.0_f32, p.accent);
    visuals.selection.bg_fill = p.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);
    visuals.window_highlight_topmost = true;
    visuals.striped = false;
    visuals.collapsing_header_frame = false;
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let rounding = Rounding::same(7.0);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = p.surface;
    widgets.noninteractive.weak_bg_fill = p.surface;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border_soft);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.dim);
    widgets.noninteractive.rounding = rounding;

    widgets.inactive.bg_fill = p.surface_high;
    widgets.inactive.weak_bg_fill = p.surface_high;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border_soft);
    widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.inactive.rounding = rounding;

    widgets.hovered.bg_fill = p.hovered_bg;
    widgets.hovered.weak_bg_fill = p.hovered_bg;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent.gamma_multiply(0.55));
    widgets.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.hovered.rounding = rounding;

    widgets.active.bg_fill = p.accent_soft;
    widgets.active.weak_bg_fill = p.accent_soft;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    widgets.active.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.active.rounding = rounding;

    widgets.open.bg_fill = p.surface_high;
    widgets.open.weak_bg_fill = p.surface_high;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, p.border);
    widgets.open.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.open.rounding = rounding;

    style.visuals = visuals;

    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 4.0);
    style.spacing.window_margin = Margin::symmetric(12.0, 10.0);
    style.spacing.menu_margin = Margin::symmetric(6.0, 5.0);
    style.spacing.indent = 14.0;
    style.spacing.interact_size = Vec2::new(36.0, 24.0);
    style.spacing.slider_width = 96.0;
    style.spacing.combo_width = 150.0;
    style.spacing.text_edit_width = 260.0;
    style.spacing.icon_width = 18.0;
    style.spacing.icon_width_inner = 10.0;
    style.spacing.scroll = egui::style::ScrollStyle::solid();
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.handle_min_length = 28.0;
    style.spacing.scroll.bar_inner_margin = 5.0;
    style.spacing.scroll.bar_outer_margin = 2.0;

    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(17.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
        (TextStyle::Button, FontId::proportional(12.5)),
        (TextStyle::Small, FontId::proportional(11.0)),
    ]
    .into();

    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_ids_round_trip() {
        for theme in Theme::ALL {
            assert_eq!(Theme::from_id(theme.id()), Some(theme));
        }
        assert_eq!(Theme::from_id("nope"), None);
        assert_eq!(Theme::default().id(), "dark");
    }

    #[test]
    fn palettes_differ_per_theme() {
        assert!(DARK.dark && CONTRAST.dark);
        assert!(!LIGHT.dark);
        assert_ne!(DARK.bg, LIGHT.bg);
        assert_ne!(DARK.text, CONTRAST.text);
        assert_ne!(DARK.find_bg, LIGHT.find_bg);
    }
}
