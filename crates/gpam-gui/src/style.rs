//! Fonts, spacing and colours, with one palette for light mode and one for
//! dark mode.
//!
//! Proportional text uses the platform UI font and monospace text the
//! platform monospace font, both looked up with fontique (the library egui's
//! system font provider is built on). egui's bundled fonts stay in each
//! family as fallbacks, and eframe's system font provider fills in glyphs
//! that none of them have.

use std::sync::Arc;

use eframe::egui::epaint::text::VariationCoords;
use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, FontTweak, RichText, Stroke,
    Style, TextStyle, Theme, Visuals,
};
use fontique::{
    Attributes, Collection, CollectionOptions, FontWeight, GenericFamily, QueryFamily, QueryFont,
    QueryStatus, SourceCache, SourceCacheOptions,
};
use gpam_core::gcp::Scope;

/// A system font picked at startup, ready to hand to egui.
pub struct SystemFont {
    pub family: String,
    pub data: FontData,
}

/// The fonts found on this system. `None` means the bundled egui font is
/// used for that family.
#[derive(Default)]
pub struct SystemFonts {
    pub ui: Option<SystemFont>,
    /// Bold face of the UI font, for headers and section names.
    pub ui_bold: Option<SystemFont>,
    pub mono: Option<SystemFont>,
}

/// Name of the font family that holds the bold UI face.
const BOLD_FAMILY: &str = "gpam-bold";

/// The bold UI font family. It always exists. Without a bold system face it
/// falls back to the regular fonts.
pub fn bold_family() -> FontFamily {
    FontFamily::Name(BOLD_FAMILY.into())
}

/// Bold text in the UI font, for headers and section names.
pub fn bold(text: impl Into<String>) -> RichText {
    RichText::new(text).family(bold_family())
}

/// Named monospace families tried before the generic `monospace` family.
/// The generic family maps to Courier on macOS, so Menlo is asked for by
/// name there. Elsewhere fontconfig's `monospace` is the user's choice.
#[cfg(target_os = "macos")]
const MONO_NAMES: &[&str] = &["Menlo"];
#[cfg(not(target_os = "macos"))]
const MONO_NAMES: &[&str] = &[];

/// Look up the system UI and monospace fonts. Enumerating system fonts
/// takes around a hundred milliseconds, so call this off the main thread.
pub fn load_system_fonts() -> SystemFonts {
    let mut collection = Collection::new(CollectionOptions {
        shared: false,
        system_fonts: true,
    });
    let mut cache = SourceCache::new(SourceCacheOptions::default());

    let ui_families = [
        QueryFamily::Generic(GenericFamily::SystemUi),
        QueryFamily::Generic(GenericFamily::SansSerif),
    ];
    let mono_families: Vec<QueryFamily<'_>> = MONO_NAMES
        .iter()
        .map(|name| QueryFamily::Named(name))
        .chain([
            QueryFamily::Generic(GenericFamily::UiMonospace),
            QueryFamily::Generic(GenericFamily::Monospace),
        ])
        .collect();

    let regular = FontWeight::NORMAL;
    SystemFonts {
        ui: first_font(&mut collection, &mut cache, &ui_families, regular),
        ui_bold: first_font(&mut collection, &mut cache, &ui_families, FontWeight::BOLD),
        mono: first_font(&mut collection, &mut cache, &mono_families, regular),
    }
}

/// The face closest to `weight` in the first family in `families` that is
/// installed and covers basic Latin.
fn first_font(
    collection: &mut Collection,
    cache: &mut SourceCache,
    families: &[QueryFamily<'_>],
    weight: FontWeight,
) -> Option<SystemFont> {
    for family in families {
        let mut found: Option<QueryFont> = None;
        let mut query = collection.query(cache);
        query.set_families([*family]);
        query.set_attributes(Attributes {
            weight,
            ..Default::default()
        });
        query.matches_with(|font| {
            if covers_latin(font) {
                found = Some(font.clone());
                QueryStatus::Stop
            } else {
                QueryStatus::Continue
            }
        });
        drop(query);
        if let Some(font) = found {
            let name = collection
                .family_name(font.family.0)
                .unwrap_or("unknown")
                .to_string();
            let (blob, _) = font.blob.into_raw_parts();
            return Some(SystemFont {
                family: name,
                data: FontData::from_blob(blob, font.index),
            });
        }
    }
    None
}

fn covers_latin(font: &QueryFont) -> bool {
    font.charmap()
        .is_some_and(|map| "Aa0".chars().all(|c| map.map(c).is_some_and(|g| g != 0)))
}

/// Install fonts and the shared style for both light and dark mode. The
/// theme itself follows the system setting, which is egui's default.
pub fn install(ctx: &egui::Context, fonts: SystemFonts) {
    let mut defs = FontDefinitions::default();
    for (font, family) in [
        (fonts.ui, FontFamily::Proportional),
        (fonts.mono, FontFamily::Monospace),
    ] {
        let Some(font) = font else {
            tracing::info!("no system font for {family:?}, using the bundled font");
            continue;
        };
        tracing::info!("using system font '{}' for {family:?}", font.family);
        let key = format!("system-{}", font.family);
        defs.font_data.insert(key.clone(), Arc::new(font.data));
        defs.families.entry(family).or_default().insert(0, key);
    }

    // The bold family is the bold system face followed by the regular
    // proportional fonts, so it always has glyphs. A variable font ships
    // every weight in one file, so the weight axis is set as well.
    let mut bold_fonts = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    match fonts.ui_bold {
        Some(font) => {
            tracing::info!("using system font '{}' for bold text", font.family);
            let key = format!("system-bold-{}", font.family);
            let data = font.data.tweak(FontTweak {
                coords: VariationCoords::new([(b"wght", 700.0)]),
                ..Default::default()
            });
            defs.font_data.insert(key.clone(), Arc::new(data));
            bold_fonts.insert(0, key);
        }
        None => tracing::info!("no bold system font, bold text uses the regular font"),
    }
    defs.families.insert(bold_family(), bold_fonts);

    ctx.set_fonts(defs);
    ctx.set_visuals_of(Theme::Light, visuals(false));
    ctx.set_visuals_of(Theme::Dark, visuals(true));
    ctx.all_styles_mut(apply_style);
}

fn apply_style(style: &mut Style) {
    use FontFamily::{Monospace, Proportional};
    style.text_styles = [
        (TextStyle::Small, FontId::new(11.0, Proportional)),
        (TextStyle::Body, FontId::new(13.5, Proportional)),
        (TextStyle::Button, FontId::new(13.5, Proportional)),
        (TextStyle::Heading, FontId::new(16.0, bold_family())),
        (TextStyle::Monospace, FontId::new(12.5, Monospace)),
    ]
    .into();
    let spacing = &mut style.spacing;
    spacing.item_spacing = egui::vec2(8.0, 6.0);
    spacing.button_padding = egui::vec2(10.0, 4.0);
    spacing.interact_size.y = 22.0;
    spacing.window_margin = egui::Margin::same(14);
}

/// The colours of one theme. Every colour the app draws with comes from
/// here, so a theme is changed in this one place.
///
/// The colours are inspired by the excellent Modus themes:
/// <https://protesilaos.com/emacs/modus-themes>
/// Their author placed the colour palette in the public domain:
/// <https://protesilaos.com/codelog/2022-05-10-modus-themes-palette-cc0/>
struct Colors {
    bg_main: Color32,
    bg_dim: Color32,
    fg_main: Color32,
    fg_dim: Color32,
    bg_active: Color32,
    bg_inactive: Color32,
    border: Color32,
    border_strong: Color32,
    bg_completion: Color32,
    bg_hover: Color32,
    red: Color32,
    green: Color32,
    yellow_warmer: Color32,
    blue: Color32,
    blue_warmer: Color32,
    magenta_cooler: Color32,
    cyan: Color32,
    bg_red_nuanced: Color32,
    bg_green_nuanced: Color32,
    bg_yellow_nuanced: Color32,
    bg_blue_nuanced: Color32,
    bg_status_bar: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The light theme.
const LIGHT: Colors = Colors {
    bg_main: rgb(0xffffff),
    bg_dim: rgb(0xf2f2f2),
    fg_main: rgb(0x000000),
    fg_dim: rgb(0x595959),
    bg_active: rgb(0xc4c4c4),
    bg_inactive: rgb(0xe0e0e0),
    border: rgb(0x9f9f9f),
    border_strong: rgb(0x5a5a5a),
    bg_completion: rgb(0xc0deff),
    bg_hover: rgb(0xb2e4dc),
    red: rgb(0xa60000),
    green: rgb(0x006800),
    yellow_warmer: rgb(0x884900),
    blue: rgb(0x0031a9),
    blue_warmer: rgb(0x3548cf),
    magenta_cooler: rgb(0x531ab6),
    cyan: rgb(0x005e8b),
    bg_red_nuanced: rgb(0xffe8e8),
    bg_green_nuanced: rgb(0xe0f6e0),
    bg_yellow_nuanced: rgb(0xf8f0d0),
    bg_blue_nuanced: rgb(0xecedff),
    bg_status_bar: rgb(0xe6e6e6),
};

/// The dark theme.
const DARK: Colors = Colors {
    bg_main: rgb(0x000000),
    bg_dim: rgb(0x1e1e1e),
    fg_main: rgb(0xffffff),
    fg_dim: rgb(0x989898),
    bg_active: rgb(0x535353),
    bg_inactive: rgb(0x303030),
    border: rgb(0x646464),
    border_strong: rgb(0x959595),
    bg_completion: rgb(0x2f447f),
    bg_hover: rgb(0x45605e),
    red: rgb(0xff5f59),
    green: rgb(0x44bc44),
    yellow_warmer: rgb(0xfec43f),
    blue: rgb(0x2fafff),
    blue_warmer: rgb(0x79a8ff),
    magenta_cooler: rgb(0xb6a0ff),
    cyan: rgb(0x00d3d0),
    bg_red_nuanced: rgb(0x3a0c14),
    bg_green_nuanced: rgb(0x092f1f),
    bg_yellow_nuanced: rgb(0x381d0f),
    bg_blue_nuanced: rgb(0x12154a),
    bg_status_bar: rgb(0x2d2d2d),
};

fn colors(dark: bool) -> &'static Colors {
    if dark { &DARK } else { &LIGHT }
}

/// egui visuals for one theme, built from its colours.
fn visuals(dark: bool) -> Visuals {
    let c = colors(dark);
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = c.bg_main;
    v.window_fill = c.bg_main;
    v.window_stroke = Stroke::new(1.0, c.border_strong);
    v.extreme_bg_color = c.bg_main;
    v.text_edit_bg_color = Some(c.bg_dim);
    v.faint_bg_color = c.bg_dim;
    v.code_bg_color = c.bg_dim;
    v.weak_text_color = Some(c.fg_dim);
    v.hyperlink_color = c.blue;
    v.warn_fg_color = c.yellow_warmer;
    v.error_fg_color = c.red;
    v.selection.bg_fill = c.bg_completion;
    v.selection.stroke = Stroke::new(1.0, c.fg_main);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = c.bg_main;
    w.noninteractive.weak_bg_fill = c.bg_dim;
    w.noninteractive.bg_stroke = Stroke::new(1.0, c.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, c.fg_main);
    for (state, fill, edge) in [
        (&mut w.inactive, c.bg_inactive, c.border),
        (&mut w.hovered, c.bg_hover, c.border_strong),
        (&mut w.active, c.bg_active, c.border_strong),
        (&mut w.open, c.bg_active, c.border_strong),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, edge);
        state.fg_stroke = Stroke::new(1.0, c.fg_main);
    }
    v
}

/// Semantic colours for the current theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub pending: Color32,
    pub error: Color32,
    pub info: Color32,
    pub dim: Color32,
    /// Headings, the focused field label and the primary button.
    pub accent: Color32,
    /// Text drawn on top of `accent`.
    pub on_accent: Color32,
    /// Names of marked rows.
    pub marked: Color32,
    /// Fill of the status bar.
    pub status_bg: Color32,
    /// Text and fill of each badge tone: an accent colour and its tinted
    /// background.
    tones: [(Color32, Color32); 5],
}

/// What a badge stands for, which picks its colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Error,
    Info,
    Neutral,
}

impl Palette {
    pub fn of(visuals: &Visuals) -> Self {
        let c = colors(visuals.dark_mode);
        Self {
            pending: c.yellow_warmer,
            error: c.red,
            info: c.cyan,
            dim: c.fg_dim,
            accent: c.blue_warmer,
            on_accent: c.bg_main,
            marked: c.magenta_cooler,
            status_bg: c.bg_status_bar,
            tones: [
                (c.green, c.bg_green_nuanced),
                (c.yellow_warmer, c.bg_yellow_nuanced),
                (c.red, c.bg_red_nuanced),
                (c.blue, c.bg_blue_nuanced),
                (c.fg_main, c.bg_inactive),
            ],
        }
    }

    /// `(text, fill)` colours of a badge tone.
    pub fn tone(&self, tone: Tone) -> (Color32, Color32) {
        self.tones[match tone {
            Tone::Ok => 0,
            Tone::Warn => 1,
            Tone::Error => 2,
            Tone::Info => 3,
            Tone::Neutral => 4,
        }]
    }

    pub fn current(ui: &egui::Ui) -> Self {
        Self::of(ui.visuals())
    }
}

/// A state as a small coloured dot followed by plain text. Quieter than a
/// badge, for states repeated down a long list.
pub fn status_dot(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let (color, _) = Palette::current(ui).tone(tone);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
    ui.add(
        egui::Label::new(text)
            .selectable(false)
            .wrap_mode(egui::TextWrapMode::Truncate),
    );
}

/// A small rounded badge with a tinted fill, for states and counts.
pub fn badge(ui: &mut egui::Ui, text: &str, tone: Tone) -> egui::Response {
    pill(ui, text, tone, egui::Sense::hover())
}

/// A badge that can be clicked. It gains an outline while hovered.
pub fn badge_button(ui: &mut egui::Ui, text: &str, tone: Tone) -> egui::Response {
    pill(ui, text, tone, egui::Sense::click())
}

/// A clickable badge with text of the given size, for labels that sit in
/// a line of larger text.
pub fn chip_button(ui: &mut egui::Ui, text: &str, tone: Tone, text_size: f32) -> egui::Response {
    let font = FontId::new(text_size, FontFamily::Proportional);
    pill_with(ui, text, tone, egui::Sense::click(), font)
}

fn pill(ui: &mut egui::Ui, text: &str, tone: Tone, sense: egui::Sense) -> egui::Response {
    let font = TextStyle::Small.resolve(ui.style());
    pill_with(ui, text, tone, sense, font)
}

fn pill_with(
    ui: &mut egui::Ui,
    text: &str,
    tone: Tone,
    sense: egui::Sense,
    font: FontId,
) -> egui::Response {
    let (fg, bg) = Palette::current(ui).tone(tone);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let size = galley.size() + egui::vec2(14.0, 4.0);
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let radius = size.y / 2.0;
    ui.painter().rect_filled(rect, radius, bg);
    if sense.senses_click() && response.hovered() {
        ui.painter()
            .rect_stroke(rect, radius, Stroke::new(1.0, fg), egui::StrokeKind::Inside);
    }
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
    response
}

/// A titled group of rows in a rounded box, the way settings windows group
/// related values.
pub fn section(
    ui: &mut egui::Ui,
    title: &str,
    palette: &Palette,
    rows: impl FnOnce(&mut egui::Ui),
) {
    ui.label(bold(title).small().color(palette.dim));
    ui.add_space(2.0);
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            rows(ui);
        });
    ui.add_space(12.0);
}

/// One row of a section: the label at the left and a short value at the
/// right.
pub fn value_row(
    ui: &mut egui::Ui,
    label: &str,
    palette: &Palette,
    value: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(palette.dim));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), value);
    });
}

/// One row of a section whose value is long: the label on its own line and
/// the value wrapped underneath.
pub fn text_row(
    ui: &mut egui::Ui,
    label: &str,
    palette: &Palette,
    value: impl FnOnce(&mut egui::Ui),
) {
    ui.label(RichText::new(label).small().color(palette.dim));
    value(ui);
}

/// How much a dialog button stands out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// The main action: filled with the accent colour.
    Primary,
    /// Another action: outlined.
    Secondary,
    /// A rare action: text only until hovered.
    Ghost,
}

/// A rounded button drawn without the look of a stock button. `key` is the
/// key that triggers it, shown small after the label, or empty. When not
/// `enabled` the button is greyed out and does not sense clicks.
pub fn key_button(
    ui: &mut egui::Ui,
    label: &str,
    key: &str,
    kind: ButtonKind,
    enabled: bool,
) -> egui::Response {
    let palette = Palette::current(ui);
    let size = TextStyle::Button.resolve(ui.style()).size;
    let family = if kind == ButtonKind::Primary {
        bold_family()
    } else {
        FontFamily::Proportional
    };
    let text_color = match (enabled, kind) {
        (false, _) => palette.dim,
        (true, ButtonKind::Primary) => palette.on_accent,
        (true, ButtonKind::Secondary) => ui.visuals().text_color(),
        (true, ButtonKind::Ghost) => palette.dim,
    };
    let label_galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), FontId::new(size, family), text_color);
    let key_color = text_color.gamma_multiply(0.7);
    let key_galley = (!key.is_empty()).then(|| {
        ui.painter().layout_no_wrap(
            key.to_owned(),
            TextStyle::Small.resolve(ui.style()),
            key_color,
        )
    });
    const GAP: f32 = 8.0;
    let key_width = key_galley.as_ref().map_or(0.0, |g| g.size().x + GAP);
    let content = egui::vec2(label_galley.size().x + key_width, label_galley.size().y);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(content + egui::vec2(28.0, 12.0), sense);
    let down = response.is_pointer_button_down_on();
    let hovered = response.hovered();
    let quiet_fill = if down {
        ui.visuals().widgets.inactive.bg_fill
    } else if hovered {
        ui.visuals().faint_bg_color
    } else {
        Color32::TRANSPARENT
    };
    let fill = match kind {
        _ if !enabled => {
            if kind == ButtonKind::Primary {
                ui.visuals().widgets.inactive.bg_fill
            } else {
                Color32::TRANSPARENT
            }
        }
        ButtonKind::Primary if down => palette.accent.lerp_to_gamma(Color32::BLACK, 0.18),
        ButtonKind::Primary if hovered => palette.accent.lerp_to_gamma(palette.on_accent, 0.14),
        ButtonKind::Primary => palette.accent,
        ButtonKind::Secondary | ButtonKind::Ghost => quiet_fill,
    };
    ui.painter().rect_filled(rect, 7.0, fill);
    if kind == ButtonKind::Secondary {
        ui.painter().rect_stroke(
            rect,
            7.0,
            ui.visuals().widgets.noninteractive.bg_stroke,
            egui::StrokeKind::Inside,
        );
    }
    let left = rect.center().x - content.x / 2.0;
    let label_pos = egui::pos2(left, rect.center().y - label_galley.size().y / 2.0);
    let label_width = label_galley.size().x;
    ui.painter().galley(label_pos, label_galley, text_color);
    if let Some(key_galley) = key_galley {
        let key_pos = egui::pos2(
            left + label_width + GAP,
            rect.center().y - key_galley.size().y / 2.0,
        );
        ui.painter().galley(key_pos, key_galley, key_color);
    }
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// The button for the main action of a view.
pub fn primary_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    key_button(ui, label, "", ButtonKind::Primary, enabled)
}

/// Text size inside form inputs.
pub const INPUT_TEXT_SIZE: f32 = 14.0;

/// A text field for forms: a rounded box with a border that turns to the
/// accent colour while the field is the one being edited. The caller sets
/// the id, hint and width on `edit`.
pub fn text_input(ui: &mut egui::Ui, edit: egui::TextEdit<'_>, focused: bool) -> egui::Response {
    let palette = Palette::current(ui);
    let visuals = ui.visuals();
    let stroke = if focused {
        Stroke::new(1.5, palette.accent)
    } else {
        visuals.widgets.noninteractive.bg_stroke
    };
    egui::Frame::new()
        .fill(
            visuals
                .text_edit_bg_color
                .unwrap_or(visuals.extreme_bg_color),
        )
        .stroke(stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            // The field is at least as tall as other controls, which is
            // taller than one line of text, so the text is centred in it.
            // The box around it supplies the padding.
            ui.add(
                edit.frame(egui::Frame::new())
                    .margin(egui::Margin::ZERO)
                    .align(egui::Align2::LEFT_CENTER)
                    .font(FontId::new(INPUT_TEXT_SIZE, FontFamily::Proportional)),
            )
        })
        .inner
}

/// A row of choices joined into one control, with the chosen one raised.
/// Returns the choice that was clicked.
pub fn segmented(ui: &mut egui::Ui, options: &[&str], selected: Option<usize>) -> Option<usize> {
    let palette = Palette::current(ui);
    let font = FontId::new(INPUT_TEXT_SIZE - 1.0, FontFamily::Proportional);
    let galleys: Vec<_> = options
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let color = if selected == Some(i) {
                ui.visuals().text_color()
            } else {
                palette.dim
            };
            let font = if selected == Some(i) {
                FontId::new(font.size, bold_family())
            } else {
                font.clone()
            };
            (
                ui.painter().layout_no_wrap((*text).to_owned(), font, color),
                color,
            )
        })
        .collect();
    let height = galleys.iter().map(|(g, _)| g.size().y).fold(0.0, f32::max) + 14.0;
    let widths: Vec<f32> = galleys.iter().map(|(g, _)| g.size().x + 22.0).collect();
    let total: f32 = widths.iter().sum();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, height), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 6.0, ui.visuals().widgets.inactive.bg_fill);
    let mut clicked = None;
    let mut left = rect.left();
    for (i, ((galley, color), width)) in galleys.into_iter().zip(widths).enumerate() {
        let cell =
            egui::Rect::from_min_size(egui::pos2(left, rect.top()), egui::vec2(width, height));
        left += width;
        let response = ui
            .interact(
                cell,
                ui.scope_id().with(("segment", i)),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if selected == Some(i) {
            ui.painter()
                .rect_filled(cell.shrink(2.0), 4.5, ui.visuals().panel_fill);
        } else if response.hovered() {
            ui.painter()
                .rect_filled(cell.shrink(2.0), 4.5, ui.visuals().faint_bg_color);
        }
        ui.painter()
            .galley(cell.center() - galley.size() / 2.0, galley, color);
        if response.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// A soft filled box without an outline, for a block of related content
/// inside a dialog.
pub fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            content(ui);
        });
}

/// Side of the square a scope icon is drawn in.
pub const SCOPE_ICON_SIZE: f32 = 14.0;

/// Word for a scope kind, used in tooltips and the help legend.
pub fn scope_kind(scope: Scope) -> &'static str {
    match scope {
        Scope::Project => "project",
        Scope::Folder => "folder",
        Scope::Organization => "organization",
    }
}

/// Paint the icon of a scope kind centred in `rect`: an office building for
/// an organization, a folder, and a hexagon for a project.
pub fn paint_scope_icon(painter: &egui::Painter, rect: egui::Rect, scope: Scope, color: Color32) {
    let s = SCOPE_ICON_SIZE;
    let c = rect.center();
    let at = |x: f32, y: f32| egui::pos2(c.x + x * s, c.y + y * s);
    match scope {
        Scope::Organization => {
            let walls = egui::Rect::from_min_max(at(-0.32, -0.44), at(0.32, 0.44));
            painter.rect_stroke(
                walls,
                1.0,
                Stroke::new(1.2, color),
                egui::StrokeKind::Inside,
            );
            for y in [-0.24, 0.0, 0.24] {
                for x in [-0.12, 0.12] {
                    let window =
                        egui::Rect::from_center_size(at(x, y), egui::Vec2::splat(0.13 * s));
                    painter.rect_filled(window, 0.0, color);
                }
            }
        }
        Scope::Folder => {
            let tab = egui::Rect::from_min_max(at(-0.46, -0.36), at(-0.04, -0.1));
            let body = egui::Rect::from_min_max(at(-0.46, -0.2), at(0.46, 0.36));
            painter.rect_filled(tab, 1.5, color);
            painter.rect_filled(body, 1.5, color);
        }
        Scope::Project => {
            let points = (0..6)
                .map(|i| {
                    let angle =
                        std::f32::consts::FRAC_PI_3 * i as f32 + std::f32::consts::FRAC_PI_6;
                    at(0.44 * angle.cos(), 0.44 * angle.sin())
                })
                .collect();
            painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
        }
    }
}

/// A scope kind icon as a widget. Hovering it names the kind.
pub fn scope_icon(ui: &mut egui::Ui, scope: Scope, color: Color32) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::Vec2::splat(SCOPE_ICON_SIZE), egui::Sense::hover());
    paint_scope_icon(ui.painter(), rect, scope, color);
    response.on_hover_text(scope_kind(scope))
}

/// Paint a count badge whose right edge sits at `right_center`. Returns the
/// area it covers.
pub fn paint_count(
    ui: &egui::Ui,
    right_center: egui::Pos2,
    count: usize,
    tone: Tone,
) -> egui::Rect {
    let (fg, bg) = Palette::current(ui).tone(tone);
    let font = TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(count.to_string(), font, fg);
    let size = egui::vec2(
        (galley.size().x + 12.0).max(galley.size().y + 4.0),
        galley.size().y + 4.0,
    );
    let rect = egui::Rect::from_min_size(
        egui::pos2(right_center.x - size.x, right_center.y - size.y / 2.0),
        size,
    );
    ui.painter().rect_filled(rect, size.y / 2.0, bg);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
    rect
}

/// Body text height plus padding, used for table rows.
pub fn row_height(ui: &egui::Ui) -> f32 {
    ui.text_style_height(&TextStyle::Body) + 8.0
}
