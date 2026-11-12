//! The main window: search field, entitlement table, detail panel, live
//! grants and status bar, plus the help and logs overlays.

use std::collections::BTreeSet;

use eframe::egui::{
    self, Align, Layout, Modal, RichText, ScrollArea, Sense, TextEdit, TextWrapMode, Ui,
};
use egui_extras::{Column, TableBuilder};
use gpam_core::app::action::{Action, Focus};
use gpam_core::app::browse::{
    ApprovalMode, BrowseState, SortColumn, approval_badge, grant_expired, role_label,
};
use gpam_core::app::format::format_age;
use gpam_core::app::help::{LIST_KEYS, SEARCH_KEYS};
use gpam_core::app::scope_tree::{ScopeKey, TreeRow, split_role_token, split_scope_token};
use gpam_core::cache::{EntitlementRow, GrantRow, now_unix};
use gpam_core::gcp::Scope;
use gpam_core::gcp::grants::{GrantState, approval_console_url};

use crate::app::{Intent, ids};
use crate::style::{
    Palette, SCOPE_ICON_SIZE, Tone, badge, badge_button, bold, bold_family, chip_button,
    paint_count, paint_scope_icon, primary_button, row_height, scope_icon, scope_kind, section,
    status_dot, text_row, value_row,
};

/// Below this width the detail panel moves under the table.
pub const NARROW_WIDTH: f32 = 760.0;

/// Scope navigator keys shown in the help overlay.
const NAVIGATOR_HINTS: &[(&str, &str)] = &[
    ("tab / shift-tab", "next / previous open panel"),
    ("[", "show or hide scopes"),
    ("j / k", "move, list shows the scope"),
    ("h / l", "close / open branch"),
    ("enter", "to the list with this scope"),
    ("space", "keep this scope in the list, or let it go"),
    ("esc", "back to list"),
    ("/", "to the search field"),
];

/// Search field extras of this frontend, shown after the shared search
/// keys.
const SEARCH_HINTS: &[(&str, &str)] = &[
    ("ctrl-u / ctrl-l", "clear typed text"),
    (
        "tab / shift-tab",
        "while typing in: or role: next / previous match",
    ),
    (
        "space / enter",
        "while typing in: or role: take the match shown",
    ),
    ("backspace", "in an empty field: drop the last label"),
];

/// Grants panel keys shown in the help overlay.
const GRANT_HINTS: &[(&str, &str)] = &[
    ("h / l", "previous / next grant"),
    ("g / G", "first / last grant"),
    ("y", "copy the approval link of a pending grant"),
    ("tab / shift-tab", "next / previous open panel"),
    ("esc", "back to list"),
    ("b", "close the panel"),
];

/// Keys that close the window, as the help overlay names them. On macOS
/// the system's application menu provides Cmd-Q.
#[cfg(target_os = "macos")]
const QUIT_KEYS: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const QUIT_KEYS: &str = "ctrl-q";

/// Window-level shortcuts shown in the help overlay.
const WINDOW_HINTS: &[(&str, &str)] = &[
    ("]", "show or hide details"),
    ("b", "open or close grants"),
    ("s / S", "sort by next column / reverse"),
    ("ctrl-l", "clear typed search text"),
    ("T", "theme: auto, light, dark"),
    ("ctrl/cmd +", "larger text"),
    ("ctrl/cmd -", "smaller text"),
    ("ctrl/cmd 0", "reset text size"),
];

/// A press-and-drag over the entitlement rows that highlights them. It
/// lives in egui's temporary memory for the duration of one press.
#[derive(Clone, Copy)]
struct DragSelect {
    /// Row the press started on.
    anchor: usize,
    /// Row the drag last reached.
    last: usize,
    /// The drag reached a second row, so the press is not a click.
    dragged: bool,
    /// Input time of the last step taken past a table edge.
    stepped_at: f64,
}

/// Seconds between row steps while a drag is held past the top or bottom
/// edge of the table.
const EDGE_SCROLL_STEP: f64 = 0.04;

/// Text size of the search field, larger than body text because the search
/// is the main way into the list.
const SEARCH_TEXT_SIZE: f32 = 17.0;

/// The search field's own state.
#[derive(Default)]
pub struct SearchInput {
    pub text: String,
    /// Which matching scope the `in:name` suggestion shows. Tab steps it
    /// and any edit resets it.
    pub scope_choice: usize,
}

/// How the scope navigator is shown this frame.
#[derive(Clone, Copy)]
pub struct NavView<'a> {
    pub open: bool,
    /// Scopes kept with Space. They stay in the list while the cursor
    /// moves on.
    pub kept: &'a [ScopeKey],
    /// List keys currently go to the navigator.
    pub focused: bool,
    pub scroll_to_cursor: bool,
}

/// Which of the collapsible panels are open this frame.
#[derive(Clone, Copy)]
pub struct Panels<'a> {
    pub details_open: bool,
    pub grants_open: bool,
    /// Scroll the entitlement table to its cursor.
    pub scroll_to_cursor: bool,
    pub grants: GrantsView<'a>,
}

/// How the grants panel is shown this frame.
#[derive(Clone, Copy)]
pub struct GrantsView<'a> {
    /// List keys currently go to the grants panel.
    pub focused: bool,
    /// Name of the selected grant, which the details panel shows.
    pub selected: Option<&'a str>,
    pub scroll_to_selected: bool,
}

/// Live grants in the order their cards are shown.
pub fn ordered_grants(state: &BrowseState) -> Vec<&GrantRow> {
    let mut rows: Vec<_> = state.live_grants().collect();
    rows.sort_by_key(|g| grant_order(g));
    rows
}

/// Draw the browse view. Pointer gestures are pushed to `intents`. Text
/// typed into the search field goes straight to core.
pub fn show(
    ui: &mut Ui,
    state: &mut BrowseState,
    search: &mut SearchInput,
    selection: &BTreeSet<usize>,
    nav: NavView<'_>,
    panels: Panels<'_>,
    out: &mut Vec<Intent>,
) {
    let palette = Palette::current(ui);
    let narrow = ui.available_width() < NARROW_WIDTH;

    egui::Panel::bottom("status")
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(palette.status_bg)
                .inner_margin(egui::Margin::symmetric(10, 4)),
        )
        .show(ui, |ui| status_bar(ui, state, &palette, out));

    // The navigator runs the full height on the left and gives way on a
    // narrow window.
    if nav.open && !narrow {
        egui::Panel::left("navigator")
            .resizable(true)
            .default_size(220.0)
            .size_range(150.0..=520.0)
            .show(ui, |ui| navigator(ui, state, nav, &palette, out));
    } else if !narrow {
        // Hidden, the navigator leaves a thin strip with the control that
        // brings it back.
        egui::Panel::left("navigator-collapsed")
            .resizable(false)
            .exact_size(32.0)
            .show(ui, |ui| {
                if collapsed_strip(ui, Side::Left, "Scopes", "show scopes ([)") {
                    out.push(Intent::ToggleNavigator);
                }
            });
    }

    // The grants header is always there. The cards under it show while the
    // panel is open and there are grants to list.
    if panels.grants_open && state.live_grants().next().is_some() {
        egui::Panel::bottom("grants")
            .resizable(true)
            .default_size(152.0)
            .size_range(120.0..=600.0)
            .show(ui, |ui| {
                grants(ui, state, true, panels.grants, &palette, out)
            });
    } else {
        egui::Panel::bottom("grants-closed")
            .resizable(false)
            .show(ui, |ui| {
                grants(ui, state, false, panels.grants, &palette, out)
            });
    }

    match (panels.details_open, narrow) {
        (true, true) => {
            egui::Panel::bottom("detail-narrow")
                .resizable(true)
                .default_size(200.0)
                .size_range(80.0..=600.0)
                .show(ui, |ui| detail(ui, state, panels.grants.selected, out));
        }
        (true, false) => {
            egui::Panel::right("detail")
                .resizable(true)
                .default_size(340.0)
                .size_range(260.0..=900.0)
                .show(ui, |ui| detail(ui, state, panels.grants.selected, out));
        }
        // Hidden, the details panel leaves a thin strip on the right with its
        // name and the control that brings it back.
        (false, false) => {
            egui::Panel::right("detail-collapsed")
                .resizable(false)
                .exact_size(32.0)
                .show(ui, |ui| {
                    if collapsed_strip(ui, Side::Right, "Details", "show details (])") {
                        out.push(Intent::ToggleDetails);
                    }
                });
        }
        // A narrow window has no room for a side strip, so the hidden
        // details panel is a one-line header above the grants.
        (false, true) => {
            egui::Panel::bottom("detail-narrow-closed")
                .resizable(false)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    let header = ui
                        .horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(egui::Vec2::splat(14.0), Sense::hover());
                            let shape =
                                egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(8.0));
                            ui.painter().add(egui::Shape::rotated_triangle(
                                shape,
                                -std::f32::consts::FRAC_PI_2,
                                palette.dim,
                            ));
                            ui.label(bold("Details"));
                        })
                        .response;
                    let header = ui.interact(
                        header.rect,
                        ui.scope_id().with("details-header"),
                        Sense::click(),
                    );
                    if header.on_hover_text("show details (])").clicked() {
                        out.push(Intent::ToggleDetails);
                    }
                    ui.add_space(2.0);
                });
        }
    }

    egui::Panel::top("search").resizable(false).show(ui, |ui| {
        search_bar(ui, state, search, &palette, out);
        toolbar(ui, state, &palette, out);
    });

    egui::CentralPanel::default().show(ui, |ui| {
        entitlement_table(ui, state, selection, panels.scroll_to_cursor, &palette, out);
    });
}

fn search_bar(
    ui: &mut Ui,
    state: &mut BrowseState,
    search: &mut SearchInput,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    // The field spans the panel, so its edges line up with the toolbar row
    // under it.
    ui.add_space(4.0);
    let focused = state.focus() == Focus::Search;
    search_field(ui, state, search, focused, palette, out);
    ui.add_space(2.0);
}

/// A drawn magnifying glass that marks the search field.
fn search_icon(ui: &mut Ui, color: egui::Color32) {
    let side = SEARCH_TEXT_SIZE;
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(side), Sense::hover());
    let stroke = egui::Stroke::new(1.6, color);
    let radius = side * 0.30;
    let center = rect.left_top() + egui::Vec2::splat(side * 0.42);
    ui.painter().circle_stroke(center, radius, stroke);
    // The handle runs from the rim towards the bottom right corner.
    let along = std::f32::consts::FRAC_1_SQRT_2;
    ui.painter().line_segment(
        [
            center + egui::Vec2::splat(radius * along),
            center + egui::Vec2::splat(radius * along + side * 0.27),
        ],
        stroke,
    );
}

/// The search field: a magnifying glass, active scopes as removable labels,
/// then the text. An
/// `in:name` word turns into a label once a space follows it.
fn search_field(
    ui: &mut Ui,
    state: &mut BrowseState,
    search: &mut SearchInput,
    focused: bool,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    let visuals = ui.visuals();
    let fill = visuals
        .text_edit_bg_color
        .unwrap_or(visuals.extreme_bg_color);
    let edge = if focused {
        egui::Stroke::new(1.0, palette.accent)
    } else {
        visuals.widgets.inactive.bg_stroke
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(edge)
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            let font = egui::FontId::new(SEARCH_TEXT_SIZE, egui::FontFamily::Proportional);
            // The line is as tall as its tallest item, a scope label, so
            // the icon, the labels and the text share one centre line.
            let text_height = ui
                .painter()
                .layout_no_wrap("Ag".to_owned(), font.clone(), palette.dim)
                .size()
                .y;
            // The field keeps its full width whatever is in it, so its right
            // edge stays in line with the toolbar under it.
            ui.set_min_width(ui.available_width());
            let line = egui::vec2(ui.available_width(), text_height + 4.0);
            ui.allocate_ui_with_layout(line, Layout::left_to_right(Align::Center), |ui| {
                let tint = if focused { palette.accent } else { palette.dim };
                search_icon(ui, tint);
                for key in state.scope_nodes().to_vec() {
                    let label = state.scope_label(&key).unwrap_or(&key.id);
                    if chip_button(ui, &format!("in: {label}"), Tone::Info, SEARCH_TEXT_SIZE)
                        .on_hover_text("click to remove")
                        .clicked()
                    {
                        out.push(Intent::RemoveScope(key));
                    }
                }
                for role in state.role_filters().to_vec() {
                    let label = format!("role: {}", role_label(&role));
                    if chip_button(ui, &label, Tone::Info, SEARCH_TEXT_SIZE)
                        .on_hover_text("click to remove")
                        .clicked()
                    {
                        out.push(Intent::RemoveRole(role));
                    }
                }
                // The long hint explains the field while it is empty. Next to
                // labels a short one is enough.
                let labelled = !state.scope_nodes().is_empty() || !state.role_filters().is_empty();
                let hint = if labelled {
                    "search"
                } else {
                    "press / to search, in:scope or role:name to narrow"
                };
                let SearchInput { text, scope_choice } = search;
                // Room is left at the right for the clear button.
                let output = TextEdit::singleline(text)
                    .id(ids::search())
                    .frame(egui::Frame::new())
                    .font(font.clone())
                    .hint_text(RichText::new(hint).font(font.clone()))
                    .desired_width((ui.available_width() - CLEAR_BUTTON_SIZE - 6.0).max(40.0))
                    .show(ui);
                if !text.is_empty() && clear_button(ui, palette) {
                    out.push(Intent::ClearQuery);
                }
                let response = output.response;
                // What the `in:name` being typed would turn into, in grey
                // right after the typed text. Tab steps through the other
                // matching scopes.
                if let Some(token) = split_scope_token(text).token {
                    let candidates = state.scope_candidates(&token);
                    let hint = if let Some(key) =
                        candidates.get(*scope_choice % candidates.len().max(1))
                    {
                        let label = state.scope_label(key).unwrap_or(&key.id);
                        if candidates.len() > 1 {
                            format!(
                                "\u{2192} {label}   {} of {}   tab: next   enter: take",
                                *scope_choice % candidates.len() + 1,
                                candidates.len()
                            )
                        } else {
                            format!("\u{2192} {label}   enter: take")
                        }
                    } else if token.is_empty() {
                        "type a scope name".to_owned()
                    } else {
                        "no scope matches".to_owned()
                    };
                    let at = egui::pos2(
                        output.galley_pos.x + output.galley.size().x + 10.0,
                        response.rect.center().y,
                    );
                    ui.painter().with_clip_rect(response.rect).text(
                        at,
                        egui::Align2::LEFT_CENTER,
                        hint,
                        font,
                        palette.dim,
                    );
                } else if let Some(token) = split_role_token(text).token {
                    // The same for a `role:name` being typed.
                    let candidates = state.role_candidates(&token);
                    let hint = if let Some(role) =
                        candidates.get(*scope_choice % candidates.len().max(1))
                    {
                        // The typed text itself stands for every match.
                        let label = if candidates.len() > 1 && *role == token.trim() {
                            format!("any role with \"{role}\"")
                        } else {
                            role_label(role).to_owned()
                        };
                        if candidates.len() > 1 {
                            format!(
                                "\u{2192} {label}   {} of {}   tab: next   enter: take",
                                *scope_choice % candidates.len() + 1,
                                candidates.len()
                            )
                        } else {
                            format!("\u{2192} {label}   enter: take")
                        }
                    } else {
                        "no role matches".to_owned()
                    };
                    let at = egui::pos2(
                        output.galley_pos.x + output.galley.size().x + 10.0,
                        response.rect.center().y,
                    );
                    ui.painter().with_clip_rect(response.rect).text(
                        at,
                        egui::Align2::LEFT_CENTER,
                        hint,
                        font,
                        palette.dim,
                    );
                }
                if !response.changed() {
                    return;
                }
                // A space after a `role:name` takes the role currently
                // shown.
                let query = split_role_token(text);
                let candidates = query
                    .token
                    .as_deref()
                    .filter(|_| query.complete)
                    .map(|token| state.role_candidates(token))
                    .unwrap_or_default();
                if !candidates.is_empty() {
                    state.add_role_filter(&candidates[*scope_choice % candidates.len()]);
                    *text = query.rest;
                    if let Some(mut edit) = TextEdit::load_state(ui.ctx(), ids::search()) {
                        let end = egui::text::CCursor::new(text.chars().count());
                        edit.cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(end)));
                        edit.store(ui.ctx(), ids::search());
                    }
                }
                // A space after the token takes the scope currently shown.
                let query = split_scope_token(text);
                let candidates = query
                    .token
                    .as_deref()
                    .filter(|_| query.complete)
                    .map(|token| state.scope_candidates(token))
                    .unwrap_or_default();
                if !candidates.is_empty() {
                    let key = candidates[*scope_choice % candidates.len()].clone();
                    state.add_scope(key);
                    *text = query.rest;
                    // The text got shorter, so put the cursor at its end.
                    if let Some(mut edit) = TextEdit::load_state(ui.ctx(), ids::search()) {
                        let end = egui::text::CCursor::new(text.chars().count());
                        edit.cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(end)));
                        edit.store(ui.ctx(), ids::search());
                    }
                }
                *scope_choice = 0;
                state.set_query(&split_scope_token(text).rest);
            });
        });
}

const CLEAR_BUTTON_SIZE: f32 = 18.0;

/// A small cross at the end of the search field that empties the typed
/// text.
fn clear_button(ui: &mut Ui, palette: &Palette) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::Vec2::splat(CLEAR_BUTTON_SIZE), Sense::click());
    let color = if response.hovered() {
        ui.painter().circle_filled(
            rect.center(),
            CLEAR_BUTTON_SIZE / 2.0,
            ui.visuals().faint_bg_color,
        );
        ui.visuals().text_color()
    } else {
        palette.dim
    };
    let arm = CLEAR_BUTTON_SIZE * 0.22;
    let c = rect.center();
    let stroke = egui::Stroke::new(1.5, color);
    for dir in [1.0, -1.0] {
        ui.painter().line_segment(
            [
                egui::pos2(c.x - arm, c.y - arm * dir),
                egui::pos2(c.x + arm, c.y + arm * dir),
            ],
            stroke,
        );
    }
    response
        .on_hover_text("clear typed text (ctrl-l)")
        .clicked()
}

/// Scope navigator: organizations, folders and projects that hold
/// entitlements. Choosing a line narrows the table to the entitlements
/// defined on that scope.
fn navigator(
    ui: &mut Ui,
    state: &BrowseState,
    nav: NavView<'_>,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let title = bold("Scopes");
        ui.label(if nav.focused {
            title.color(palette.accent)
        } else {
            title
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if sidebar_toggle(ui, Side::Left, "hide scopes ([)") {
                out.push(Intent::ToggleNavigator);
            }
        });
    });
    ui.separator();

    let rows = state.scope_rows();
    let active = state.scope_nodes();
    let cursor = state.nav_cursor();
    ScrollArea::vertical()
        .id_salt("navigator-scroll")
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let line = NavLine {
                pos: 0,
                depth: 0,
                branch: None,
                kind: None,
                label: "all scopes",
                count: state.entitlements().len(),
                more: 0,
                hint: "show entitlements from every scope".to_owned(),
                active: active.is_empty(),
                kept: false,
                at_cursor: nav.focused && cursor == 0,
            };
            nav_line(ui, &line, nav.scroll_to_cursor, palette, out);
            for (i, row) in rows.iter().enumerate() {
                let line = NavLine {
                    pos: i + 1,
                    depth: row.depth,
                    branch: row.has_children.then_some(row.expanded),
                    kind: Some(row.key.scope),
                    label: &row.label,
                    count: row.own,
                    // A closed branch stands for everything inside it.
                    more: if row.has_children && !row.expanded {
                        row.below
                    } else {
                        0
                    },
                    hint: scope_hint(row),
                    active: active.contains(&row.key),
                    kept: nav.kept.contains(&row.key),
                    at_cursor: nav.focused && cursor == i + 1,
                };
                nav_line(ui, &line, nav.scroll_to_cursor, palette, out);
            }
        });
}

/// Tooltip for a scope: what it is and how many entitlements it holds.
fn scope_hint(row: &TreeRow) -> String {
    let mut hint = format!("{} {}\n{} defined here", row.key.scope, row.key.id, row.own);
    if row.below > 0 {
        hint.push_str(&format!(", {} further down", row.below));
    }
    hint
}

/// Which edge of the window a side panel sits on.
#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

/// The button that hides or shows a side panel: a drawn window outline with
/// a pane divided off on that panel's side, the usual sign for a side
/// panel. Returns true on click.
fn sidebar_toggle(ui: &mut Ui, side: Side, hint: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(SIDEBAR_TOGGLE_SIZE, Sense::click());
    paint_sidebar_toggle(ui, rect, &response, side);
    response.on_hover_text(hint).clicked()
}

const SIDEBAR_TOGGLE_SIZE: egui::Vec2 = egui::vec2(24.0, 20.0);

/// Paint the side-panel icon in `rect`: a window outline with a divider
/// near the side the panel is on.
fn paint_sidebar_toggle(ui: &Ui, rect: egui::Rect, response: &egui::Response, side: Side) {
    let visuals = ui.style().interact(response);
    if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, visuals.weak_bg_fill);
    }
    let stroke = egui::Stroke::new(1.5, visuals.fg_stroke.color);
    let frame = rect.shrink2(egui::vec2(4.0, 3.5));
    ui.painter()
        .rect_stroke(frame, 2.5, stroke, egui::StrokeKind::Inside);
    let divider = match side {
        Side::Left => frame.left() + frame.width() * 0.36,
        Side::Right => frame.right() - frame.width() * 0.36,
    };
    ui.painter().line_segment(
        [
            egui::pos2(divider, frame.top()),
            egui::pos2(divider, frame.bottom()),
        ],
        stroke,
    );
}

/// The strip a hidden side panel leaves behind: the panel icon and the
/// panel name written down the strip. A click anywhere on it brings the
/// panel back.
fn collapsed_strip(ui: &mut Ui, side: Side, title: &str, hint: &str) -> bool {
    // The icon and the name share the centre line of the strip, whatever
    // margin the panel adds around its content.
    let strip = ui.max_rect();
    let whole = ui.interact(strip, ui.scope_id().with("strip"), Sense::click());
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(
            strip.center().x,
            strip.top() + 4.0 + SIDEBAR_TOGGLE_SIZE.y / 2.0,
        ),
        SIDEBAR_TOGGLE_SIZE,
    );
    let icon = ui.interact(icon_rect, ui.scope_id().with("strip-icon"), Sense::click());
    paint_sidebar_toggle(ui, icon_rect, &icon, side);
    let color = if whole.hovered() || icon.hovered() {
        ui.visuals().text_color()
    } else {
        Palette::current(ui).dim
    };
    let size = egui::TextStyle::Body.resolve(ui.style()).size;
    let galley = ui.painter().layout_no_wrap(
        title.to_owned(),
        egui::FontId::new(size, bold_family()),
        color,
    );
    // Turned a quarter clockwise the text runs downwards, and its line
    // height extends to the left of the anchor.
    let anchor = egui::pos2(
        strip.center().x + galley.size().y / 2.0,
        icon_rect.bottom() + 10.0,
    );
    ui.painter().add(
        egui::epaint::TextShape::new(anchor, galley, color).with_angle(std::f32::consts::FRAC_PI_2),
    );
    let icon = icon.on_hover_text(hint).clicked();
    icon || whole.on_hover_text(hint).clicked()
}

/// One navigator line.
struct NavLine<'a> {
    /// Navigator position. Zero is the "all scopes" line.
    pos: usize,
    depth: usize,
    /// `Some(expanded)` for a scope with children.
    branch: Option<bool>,
    /// Kind of the scope on this line. The "all scopes" line has none.
    kind: Option<Scope>,
    label: &'a str,
    /// Entitlements defined on this line's scope, shown as a count badge.
    /// Zero shows no badge and dims the label.
    count: usize,
    /// Entitlements hidden inside a closed branch, shown as `+N`.
    more: usize,
    hint: String,
    /// The table is narrowed to this line.
    active: bool,
    /// Kept with Space, shown with a dot and in bold.
    kept: bool,
    at_cursor: bool,
}

fn nav_line(
    ui: &mut Ui,
    line: &NavLine<'_>,
    scroll: bool,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    const INDENT: f32 = 14.0;
    const ARROW: f32 = 16.0;

    let height = row_height(ui);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
    if line.at_cursor {
        ui.painter()
            .rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
        if scroll {
            response.scroll_to_me(None);
        }
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 2.0, ui.visuals().faint_bg_color);
    }

    let body = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::TextStyle::Small.resolve(ui.style());
    let text_color = ui.visuals().text_color();
    let arrow_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 4.0 + line.depth as f32 * INDENT, rect.top()),
        egui::vec2(ARROW, height),
    );
    let mut toggled = false;
    if let Some(expanded) = line.branch {
        let arrow = ui.interact(arrow_rect, response.id.with("arrow"), Sense::click());
        toggled = arrow.clicked();
        // A triangle pointing right when closed and down when open.
        let shape = egui::Rect::from_center_size(arrow_rect.center(), egui::Vec2::splat(8.0));
        let rotation = if expanded {
            0.0
        } else {
            -std::f32::consts::FRAC_PI_2
        };
        let color = if arrow.hovered() {
            text_color
        } else {
            palette.dim
        };
        ui.painter()
            .add(egui::Shape::rotated_triangle(shape, rotation, color));
    }

    // Right edge: the count badge, with `+N` to its left for a closed
    // branch. The label is clipped where they start.
    let mut right = rect.right() - 6.0;
    if line.count > 0 {
        let tone = if line.active {
            Tone::Info
        } else {
            Tone::Neutral
        };
        right = paint_count(ui, egui::pos2(right, rect.center().y), line.count, tone).left() - 6.0;
    }
    if line.kept {
        let dot = egui::pos2(right - 3.0, rect.center().y);
        ui.painter().circle_filled(dot, 3.0, palette.accent);
        right = dot.x - 9.0;
    }
    if line.more > 0 {
        let more = ui.painter().text(
            egui::pos2(right, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            format!("+{}", line.more),
            small,
            palette.dim,
        );
        right = more.left() - 6.0;
    }
    let (font, color) = if line.kept {
        (egui::FontId::new(body.size, bold_family()), palette.accent)
    } else if line.active {
        (body, palette.accent)
    } else if line.count == 0 {
        (body, palette.dim)
    } else {
        (body, text_color)
    };
    // An icon sits between the arrow and the label: the kind of the scope,
    // or a small grid on the "all scopes" line. Every line has one, so all
    // labels of one depth start at the same place.
    let icon = egui::Rect::from_min_size(
        egui::pos2(
            arrow_rect.right() + 2.0,
            rect.center().y - SCOPE_ICON_SIZE / 2.0,
        ),
        egui::Vec2::splat(SCOPE_ICON_SIZE),
    );
    let icon_color = if line.active {
        palette.accent
    } else {
        palette.dim
    };
    match line.kind {
        Some(kind) => paint_scope_icon(ui.painter(), icon, kind, icon_color),
        None => {
            for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let cell = egui::Rect::from_center_size(
                    icon.center() + egui::vec2(dx, dy) * 3.0,
                    egui::Vec2::splat(4.0),
                );
                ui.painter().rect_filled(cell, 1.0, icon_color);
            }
        }
    }
    let label_left = icon.right() + 5.0;
    let label_clip = egui::Rect::from_min_max(
        egui::pos2(label_left, rect.top()),
        egui::pos2(right, rect.bottom()),
    );
    // A label too long for the line ends in an ellipsis.
    let mut job = egui::text::LayoutJob::simple_singleline(line.label.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(label_clip.width());
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(label_clip.left(), rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );

    if toggled {
        out.push(Intent::NavToggleExpand(line.pos));
    } else if response.clicked() {
        // A plain click narrows to this scope alone. With Ctrl or Cmd held
        // it is added to, or taken out of, the active scopes.
        if line.pos > 0 && ui.input(|i| i.modifiers.command) {
            out.push(Intent::NavToggle(line.pos));
        } else {
            out.push(Intent::NavSelect(line.pos));
        }
    }
    response.on_hover_text(&line.hint);
}

/// Pointer equivalents of the list key bindings. Each button shows its key
/// in the tooltip.
fn toolbar(ui: &mut Ui, state: &BrowseState, palette: &Palette, out: &mut Vec<Intent>) {
    ui.horizontal(|ui| {
        // Left: what the table shows.
        // A toggle that looks like the button next to it and stays pressed
        // in while the marked view is on.
        if ui
            .add(egui::Button::new("marked only").selected(state.only_marked()))
            .on_hover_text("m")
            .clicked()
        {
            out.push(Intent::Browse(Action::ToggleMarkedView));
        }
        if ui
            .button(format!("type: {}", state.scope_filter_label()))
            .on_hover_text("show only organizations, folders or projects (f)")
            .clicked()
        {
            out.push(Intent::Browse(Action::CycleScopeFilter));
        }
        // How many entitlements the search and the filters leave.
        let (shown, all) = (state.filtered().len(), state.entitlements().len());
        let count = if shown == all {
            format!("{all} entitlements")
        } else {
            format!("{shown} of {all} entitlements")
        };
        ui.label(RichText::new(count).color(palette.dim));

        // Right: what happens to the chosen rows. The request button is
        // last, so it sits at the far right.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let marked = state.marked_count();
            // With nothing marked the request is for the row under the
            // cursor, which PAM refuses while that row has a live grant.
            let blocked = (marked == 0)
                .then(|| state.current())
                .flatten()
                .and_then(|ent| {
                    let grants = state.grants_by_entitlement();
                    let g = grants.get(ent.name.as_str())?;
                    (!grant_expired(g, now_unix())).then(|| {
                        if g.state.is_active() {
                            "this entitlement already has an active grant"
                        } else {
                            "this entitlement is already requested"
                        }
                    })
                });
            let label = if marked > 0 {
                format!("Request {marked}")
            } else {
                "Request".to_owned()
            };
            let enabled = blocked.is_none() && (marked > 0 || state.current().is_some());
            let hint = match (enabled, marked > 0) {
                (false, _) => blocked.unwrap_or("nothing to request"),
                (true, true) => "enter: request the marked rows, not the row under the cursor",
                (true, false) => "enter: request the row under the cursor",
            };
            if primary_button(ui, &label, enabled)
                .on_hover_text(hint)
                .clicked()
            {
                out.push(Intent::Browse(Action::Submit));
            }
            if marked > 0
                && ui
                    .button("Clear")
                    .on_hover_text("unmark all (esc)")
                    .clicked()
            {
                out.push(Intent::ClearMarks);
            }
        });
    });
    ui.add_space(4.0);
}

fn entitlement_table(
    ui: &mut Ui,
    state: &BrowseState,
    selection: &BTreeSet<usize>,
    scroll_to_cursor: bool,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    let row_h = row_height(ui) + 4.0;
    let area = ui.available_rect_before_wrap();
    let (modifiers, pressed, down, released, pointer_y, now) = ui.input(|i| {
        (
            i.modifiers,
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
            i.pointer.latest_pos().map(|p| p.y),
            i.time,
        )
    });
    let drag_id = egui::Id::unique("gpam-drag-select");
    let mut drag: Option<DragSelect> = ui.data(|d| d.get_temp(drag_id)).flatten();
    let ctx = ui.ctx().clone();
    // A divider, column edge or scrollbar being dragged owns the pointer, so
    // it never turns into a row selection.
    let other_drag = ctx.dragged_id().is_some();
    if (!down && !released) || other_drag {
        drag = None;
    }
    let cursor = state.cursor();
    let filtered = state.filtered();
    let entitlements = state.entitlements();

    // With rows marked, Enter requests those and not the row under the
    // cursor, so the cursor highlight steps back.
    if state.marked_count() > 0 {
        let visuals = ui.visuals_mut();
        visuals.selection.bg_fill = visuals
            .selection
            .bg_fill
            .lerp_to_gamma(visuals.panel_fill, 0.55);
    }
    // Rows are separated by hairlines and nothing else: no stripes and no
    // column borders at rest. A border still shows while it is hovered or
    // dragged to resize a column.
    let rule = ui.visuals().widgets.noninteractive.bg_stroke;
    let hairline = egui::Stroke::new(1.0, rule.color.lerp_to_gamma(ui.visuals().panel_fill, 0.6));
    ui.visuals_mut().widgets.noninteractive.bg_stroke.color = egui::Color32::TRANSPARENT;
    // The row under the pointer gets a faint neutral tint, weaker than the
    // selection and without a hue of its own.
    ui.visuals_mut().widgets.hovered.bg_fill = ui.visuals().faint_bg_color;
    // Bottom edge of every row drawn this frame.
    let mut row_edges: Vec<f32> = Vec::new();
    // Name and scope start out sharing what the fixed columns leave, with
    // room kept for the grant column so its state and time are not cut off.
    const CHECK_WIDTH: f32 = 28.0;
    const APPROVAL_WIDTH: f32 = 76.0;
    const MAX_WIDTH: f32 = 48.0;
    const GRANT_WIDTH: f32 = 124.0;
    let spacing = ui.spacing().item_spacing;
    let gaps = 5.0 * spacing.x;
    let flexible =
        (ui.available_width() - CHECK_WIDTH - APPROVAL_WIDTH - MAX_WIDTH - GRANT_WIDTH - gaps)
            .max(180.0);
    let name_width = flexible * 0.52;
    let scope_width = flexible * 0.48;
    let width_id = ui.scope_id().with("entitlements-width");
    let room = ui.available_width();
    let refit = ui
        .data(|d| d.get_temp::<f32>(width_id))
        .is_some_and(|last| (last - room).abs() > 0.5);
    ui.data_mut(|d| d.insert_temp(width_id, room));
    let mut table = TableBuilder::new(ui)
        .id_salt("entitlements")
        .striped(false)
        .resizable(true)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(CHECK_WIDTH))
        .column(Column::initial(name_width).at_least(100.0).clip(true))
        .column(Column::initial(scope_width).at_least(80.0).clip(true))
        .column(Column::initial(APPROVAL_WIDTH).at_least(40.0).clip(true))
        .column(Column::initial(MAX_WIDTH).at_least(40.0).clip(true))
        // Only a last column fills the leftover width, which is what makes
        // rows and their highlight reach the right edge.
        .column(Column::remainder().at_least(GRANT_WIDTH).clip(true))
        .min_scrolled_height(0.0)
        .auto_shrink(false);
    // Live grants by entitlement, so a row shows that it is already held or
    // requested.
    let now_secs = now_unix();
    let granted = state.grants_by_entitlement();
    if scroll_to_cursor && !filtered.is_empty() {
        table = table.scroll_to_row(cursor, None);
    }
    // When the room for the table changes, for example because a side panel
    // opened, the columns start over from widths that fit the new room.
    if refit {
        table.reset();
    }

    let sort = state.sort();
    let mut clicks = Vec::new();
    // Row under the pointer this frame.
    let mut under_pointer = None;
    // Row the pointer button went down on this frame, if the press landed on
    // a row and not on another widget.
    let mut pressed_row = None;
    table
        .header(row_h, |mut header| {
            header.col(|_| {});
            for (title, column) in [
                ("Name", SortColumn::Name),
                ("Scope", SortColumn::Scope),
                ("Approval", SortColumn::Approval),
                ("Max", SortColumn::MaxDuration),
                ("Grant", SortColumn::Grant),
            ] {
                header.col(|ui| {
                    let mut title_and_arrow = |ui: &mut Ui| {
                        let label = ui.add(
                            egui::Label::new(bold(title))
                                .selectable(false)
                                .sense(Sense::click()),
                        );
                        // A small triangle marks the ordered column. It
                        // points up for ascending and down for descending.
                        if let Some(order) = sort.filter(|o| o.column == column) {
                            let (rect, _) =
                                ui.allocate_exact_size(egui::Vec2::splat(10.0), Sense::hover());
                            let shape =
                                egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(7.0));
                            let rotation = if order.descending {
                                0.0
                            } else {
                                std::f32::consts::PI
                            };
                            ui.painter().add(egui::Shape::rotated_triangle(
                                shape,
                                rotation,
                                palette.accent,
                            ));
                        }
                        if label
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("sort by this column (s: next column, S: reverse)")
                            .clicked()
                        {
                            out.push(Intent::Sort(column));
                        }
                    };
                    // Durations are numbers, so that column is set to the
                    // right, header included.
                    if column == SortColumn::MaxDuration {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_space(NUMBER_COLUMN_PAD);
                            title_and_arrow(ui);
                        });
                    } else {
                        title_and_arrow(ui);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(row_h, filtered.len(), |mut row| {
                let pos = row.index();
                let Some(ent) = filtered.get(pos).and_then(|&i| entitlements.get(i)) else {
                    return;
                };
                let idx = filtered.get(pos).copied();
                let marked = idx.is_some_and(|i| state.is_marked(i));
                row.set_selected(pos == cursor || idx.is_some_and(|i| selection.contains(&i)));

                let mut checked = marked;
                let mut toggled = false;
                row.col(|ui| {
                    toggled = ui.add(egui::Checkbox::without_text(&mut checked)).changed();
                });
                row.col(|ui| {
                    if marked {
                        plain(
                            ui,
                            RichText::new(&ent.short_name)
                                .color(palette.marked)
                                .family(bold_family()),
                        );
                    } else {
                        plain(ui, &ent.short_name);
                    }
                });
                row.col(|ui| {
                    scope_icon(ui, ent.scope_type, palette.dim);
                    let name = ent
                        .scope_display_name
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&ent.scope_id);
                    plain(ui, RichText::new(name).color(palette.dim));
                });
                row.col(|ui| {
                    // The column answers one yes-or-no question, so a row
                    // that needs an approver gets a dot and the others stay
                    // empty. Hovering the dot names the approvers.
                    if approval_badge(ent).1 == ApprovalMode::Manual {
                        let (rect, dot) =
                            ui.allocate_exact_size(egui::vec2(14.0, 14.0), Sense::hover());
                        ui.painter()
                            .circle_filled(rect.center(), 3.5, palette.tone(Tone::Warn).0);
                        dot.on_hover_text(format!(
                            "needs approval from:\n{}",
                            ent.approvers.join("\n")
                        ));
                    }
                });
                row.col(|ui| {
                    let max = ent
                        .max_request_duration_secs
                        .filter(|s| *s > 0)
                        .map(format_age)
                        .unwrap_or_else(|| "-".into());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(NUMBER_COLUMN_PAD);
                        plain(ui, RichText::new(max).color(palette.dim));
                    });
                });
                row.col(|ui| {
                    let Some(g) = granted.get(ent.name.as_str()) else {
                        return;
                    };
                    match grant_remaining(g, now_secs) {
                        Some(0) => plain(ui, RichText::new("expired").color(palette.dim)),
                        Some(left) if left < EXPIRING_SECS => {
                            let text = format!("expiring {}", format_age(left));
                            status_dot(ui, &text, Tone::Error)
                        }
                        Some(left) => {
                            status_dot(ui, &format!("active {}", format_age(left)), Tone::Ok)
                        }
                        None => status_dot(ui, "pending", Tone::Warn),
                    }
                });

                let response = row.response();
                row_edges.push(response.rect.bottom());
                if response
                    .ctx
                    .rect_contains_pointer(response.layer_id, response.rect)
                {
                    under_pointer = Some(pos);
                }
                if pressed && response.is_pointer_button_down_on() {
                    pressed_row = Some(pos);
                }
                if toggled {
                    clicks.push(Intent::ToggleMark(pos));
                } else if response.double_clicked() {
                    clicks.push(Intent::OpenRow(pos));
                } else if response.clicked() {
                    if modifiers.shift {
                        clicks.push(Intent::ExtendSelection(pos));
                    } else if modifiers.command {
                        clicks.push(Intent::ToggleSelected(pos));
                    } else {
                        clicks.push(Intent::SetCursor(pos));
                    }
                }
            });
        });

    // A rule under the header and a hairline under each row. They are drawn
    // across the columns, clipped to the table.
    // The table paints a row's highlight half the item spacing beyond the
    // row on every side, so the lines sit on those same edges.
    let across = (area.left() - spacing.x / 2.0)..=(area.right() + spacing.x / 2.0);
    let header_edge = area.top() + row_h + spacing.y / 2.0;
    ui.painter().hline(across.clone(), header_edge, rule);
    let body = egui::Rect::from_min_max(
        egui::pos2(*across.start(), header_edge + 1.0),
        egui::pos2(*across.end(), area.bottom()),
    );
    let lines = ui.painter().with_clip_rect(body);
    for y in row_edges {
        lines.hline(across.clone(), y + spacing.y / 2.0, hairline);
    }

    // Holding the button and moving onto another row highlights every row
    // between the press and the pointer.
    if pressed {
        drag = pressed_row.filter(|_| !other_drag).map(|pos| DragSelect {
            anchor: pos,
            last: pos,
            dragged: false,
            stepped_at: now,
        });
    } else if down && let Some(d) = &mut drag {
        // Held above the first row or below the table, the drag keeps
        // stepping one row further and the table scrolls after it.
        let past_edge = pointer_y.and_then(|y| {
            if y < area.top() + row_h {
                d.last.checked_sub(1)
            } else if y > area.bottom() {
                Some(d.last + 1).filter(|next| *next < filtered.len())
            } else {
                None
            }
        });
        let target = match past_edge {
            Some(next) => {
                // Steps are timed, so keep frames coming while the pointer
                // rests outside the table.
                ctx.request_repaint();
                (now - d.stepped_at >= EDGE_SCROLL_STEP).then(|| {
                    d.stepped_at = now;
                    out.push(Intent::ScrollToCursor);
                    next
                })
            }
            None => under_pointer.filter(|pos| *pos != d.last),
        };
        if let Some(pos) = target {
            out.push(Intent::SelectRange {
                from: d.anchor,
                to: pos,
            });
            d.last = pos;
            d.dragged = true;
        }
    }
    // A press that dragged is not also a click on the row it ends on.
    if !drag.is_some_and(|d| d.dragged) {
        out.append(&mut clicks);
    }
    if released {
        drag = None;
    }
    ctx.data_mut(|d| d.insert_temp(drag_id, drag));
}

/// Space kept at the right of a right-set column, so its text does not
/// touch the next column.
const NUMBER_COLUMN_PAD: f32 = 10.0;

/// A non-selectable, non-wrapping label, so table rows react to clicks
/// instead of starting a text selection.
fn plain(ui: &mut Ui, text: impl Into<egui::WidgetText>) {
    ui.add(
        egui::Label::new(text)
            .selectable(false)
            .wrap_mode(TextWrapMode::Truncate),
    );
}

/// The details panel. It shows the entitlement under the table cursor, or,
/// while a grant card is selected, that grant and its entitlement.
fn detail(ui: &mut Ui, state: &BrowseState, grant: Option<&str>, out: &mut Vec<Intent>) {
    let palette = Palette::current(ui);
    let grant = grant.and_then(|name| state.live_grants().find(|g| g.name == name));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(bold("Details"));
        if grant.is_some() {
            badge(ui, "grant", Tone::Info).on_hover_text("esc: back to the table row");
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if sidebar_toggle(ui, Side::Right, "hide details (])") {
                out.push(Intent::ToggleDetails);
            }
            let label = if state.show_raw() { "summary" } else { "raw" };
            if ui.small_button(label).on_hover_text("J").clicked() {
                out.push(Intent::Browse(Action::ToggleRaw));
            }
        });
    });
    ui.separator();

    if state.show_raw() {
        ScrollArea::both()
            .id_salt("detail-raw")
            .auto_shrink(false)
            .show(ui, |ui| {
                let raw = match grant {
                    Some(g) => g
                        .raw_json
                        .clone()
                        .unwrap_or_else(|| "no raw data stored for this grant".to_owned()),
                    None => state.preview(),
                };
                ui.add(
                    egui::Label::new(RichText::new(raw).monospace())
                        .wrap_mode(TextWrapMode::Extend),
                );
            });
        return;
    }
    ScrollArea::vertical()
        .id_salt("detail-summary")
        .auto_shrink(false)
        .show(ui, |ui| {
            let ent = match grant {
                Some(g) => {
                    grant_fields(ui, g, &palette);
                    state
                        .entitlements()
                        .iter()
                        .find(|e| e.name == g.entitlement_name)
                }
                None => state.current(),
            };
            let Some(ent) = ent else {
                let missing = if grant.is_some() {
                    "the entitlement of this grant is not in the list"
                } else {
                    "no entitlement selected"
                };
                ui.label(RichText::new(missing).color(palette.dim));
                return;
            };
            entitlement_sections(ui, state, ent, &palette, out);
        });
}

/// The entitlement as sections: what it is, how it is requested, who
/// approves it and which roles it grants.
fn entitlement_sections(
    ui: &mut Ui,
    state: &BrowseState,
    ent: &EntitlementRow,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    section(ui, "Entitlement", palette, |ui| {
        ui.add(egui::Label::new(bold(&ent.short_name)).wrap());
        ui.separator();
        let resource = format!("{}/{}", ent.scope_type.plural(), ent.scope_id);
        match ent.scope_display_name.as_deref().filter(|s| !s.is_empty()) {
            Some(name) => {
                value_row(ui, "Scope", palette, |ui| {
                    ui.add(egui::Label::new(name).truncate());
                    scope_icon(ui, ent.scope_type, palette.dim);
                });
                ui.separator();
                text_row(ui, "Resource", palette, |ui| {
                    ui.add(egui::Label::new(resource).wrap());
                });
            }
            None => {
                value_row(ui, "Scope", palette, |ui| {
                    ui.add(egui::Label::new(resource).truncate());
                    scope_icon(ui, ent.scope_type, palette.dim);
                });
            }
        }
    });

    section(ui, "Request", palette, |ui| {
        value_row(ui, "Max duration", palette, |ui| {
            let max = ent
                .max_request_duration_secs
                .filter(|s| *s > 0)
                .map(format_age)
                .unwrap_or_else(|| "no limit".into());
            ui.label(max);
        });
        ui.separator();
        value_row(ui, "Justification", palette, |ui| {
            if ent.justification_required {
                badge(ui, "required", Tone::Warn);
            } else {
                ui.label("optional");
            }
        });
        ui.separator();
        value_row(ui, "Approval", palette, |ui| {
            if ent.approvers.is_empty() {
                badge(ui, "auto", Tone::Info);
            } else {
                badge(ui, "needs approval", Tone::Warn);
            }
        });
    });

    if !ent.approvers.is_empty() {
        section(ui, "Approvers", palette, |ui| {
            for (i, approver) in ent.approvers.iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                ui.add(egui::Label::new(approver).wrap());
            }
        });
    }

    section(ui, "Roles", palette, |ui| {
        if ent.roles.is_empty() {
            ui.label(RichText::new("none listed").color(palette.dim));
        }
        for (i, role) in ent.roles.iter().enumerate() {
            if i > 0 {
                ui.separator();
            }
            // A role is a link that adds it to the search as a `role:`
            // label. One already there is shown in the accent colour. The
            // section title says these are roles, so the `roles/` prefix is
            // left out.
            let active = state
                .role_filters()
                .iter()
                .any(|r| r.eq_ignore_ascii_case(role));
            let text = if active {
                RichText::new(role_label(role)).color(palette.accent)
            } else {
                RichText::new(role_label(role))
            };
            let hint = if active {
                format!("{role}\nalready in the search")
            } else {
                format!("{role}\nclick to show only entitlements granting this role")
            };
            let link = ui
                .add(egui::Label::new(text).wrap().sense(Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(hint);
            if link.clicked() && !active {
                out.push(Intent::AddRole(role.clone()));
            }
        }
    });
}

/// The grants panel. Its header line is always shown: a triangle to open
/// or close the panel, the title and a count. `open` adds one card per
/// grant.
fn grants(
    ui: &mut Ui,
    state: &BrowseState,
    open: bool,
    view: GrantsView<'_>,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    let now = now_unix();
    let rows = ordered_grants(state);
    let pending = rows.iter().any(|g| !g.state.is_active());
    let expiring = rows
        .iter()
        .any(|g| grant_remaining(g, now).is_some_and(|left| left > 0 && left < EXPIRING_SECS));
    // The count is coloured while something expires or waits, so a closed
    // panel still says when to look.
    let tone = match (rows.is_empty(), expiring, pending) {
        (true, ..) => Tone::Neutral,
        (false, true, _) => Tone::Error,
        (false, false, true) => Tone::Warn,
        (false, false, false) => Tone::Ok,
    };
    ui.add_space(4.0);
    let header = ui
        .horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(14.0), Sense::hover());
            let shape = egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(8.0));
            let rotation = if open {
                0.0
            } else {
                -std::f32::consts::FRAC_PI_2
            };
            ui.painter()
                .add(egui::Shape::rotated_triangle(shape, rotation, palette.dim));
            ui.label(if view.focused {
                bold("Grants").color(palette.accent)
            } else {
                bold("Grants")
            });
            badge(ui, &rows.len().to_string(), tone);
            if rows.is_empty() {
                ui.label(RichText::new("no active or pending grants").color(palette.dim));
            }
        })
        .response;
    let header = ui.interact(
        header.rect,
        ui.scope_id().with("grants-header"),
        Sense::click(),
    );
    if header.on_hover_text("open or close (b)").clicked() {
        out.push(Intent::ToggleGrants);
    }
    if !open {
        ui.add_space(2.0);
        return;
    }
    ui.add_space(4.0);
    ScrollArea::vertical()
        .id_salt("grants")
        .auto_shrink(false)
        .show(ui, |ui| {
            let gap = ui.spacing().item_spacing.x;
            let avail = ui.available_width();
            let cols = (((avail + gap) / (GRANT_CARD_MIN_WIDTH + gap)).floor() as usize).max(1);
            let width = (avail - gap * (cols - 1) as f32) / cols as f32;
            for line in rows.chunks(cols) {
                ui.horizontal_top(|ui| {
                    for g in line {
                        let selected = view.selected == Some(g.name.as_str());
                        let card = GrantCard {
                            now,
                            width,
                            selected,
                            focused: selected && view.focused,
                            scroll: selected && view.scroll_to_selected,
                        };
                        grant_card(ui, g, card, palette, out);
                    }
                });
            }
            // Room under the last line of cards, like the room above the
            // first.
            ui.add_space(8.0);
        });
}

/// Cards never get narrower than this. A wider panel fits more per line.
const GRANT_CARD_MIN_WIDTH: f32 = 260.0;

/// An active grant with less than this many seconds left counts as
/// expiring.
const EXPIRING_SECS: i64 = 600;

/// Seconds an active grant has left, or `None` while it is still pending.
fn grant_remaining(g: &GrantRow, now: i64) -> Option<i64> {
    g.state
        .is_active()
        .then(|| g.expires_at.map_or(0, |exp| (exp - now).max(0)))
}

/// Sort key that puts pending grants first, then active ones with the
/// soonest expiry first, then those that ran out.
fn grant_order(g: &GrantRow) -> (u8, i64) {
    let rank = if grant_expired(g, now_unix()) {
        2
    } else {
        u8::from(g.state.is_active())
    };
    (rank, g.expires_at.unwrap_or(i64::MAX))
}

/// Words for the state badge of a grant.
fn grant_state_label(state: &GrantState) -> String {
    match state {
        GrantState::Active => "active".into(),
        GrantState::Requested => "requested".into(),
        GrantState::ApprovalAwaited => "awaiting approval".into(),
        GrantState::Scheduled => "scheduled".into(),
        GrantState::Activating => "activating".into(),
        GrantState::Revoking => "revoking".into(),
        GrantState::Withdrawing => "withdrawing".into(),
        other => other.to_string().to_lowercase(),
    }
}

/// Seconds the copy button shows a tick after a click.
const COPIED_SECS: f64 = 1.5;

/// A small button that copies `url` to the clipboard. It draws two
/// overlapping sheets, and a tick for a moment after a click.
fn copy_link_button(ui: &mut Ui, grant: &str, url: String, palette: &Palette) {
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(18.0), Sense::click());
    let id = egui::Id::unique(("copied-link", grant));
    let now = ui.input(|i| i.time);
    if response.clicked() {
        ui.ctx().copy_text(url);
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    let copied = ui
        .data(|d| d.get_temp::<f64>(id))
        .is_some_and(|at| now - at < COPIED_SECS);
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let c = rect.center();
    if copied {
        // Keep drawing until the tick is due to go away.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
        let stroke = egui::Stroke::new(1.6, palette.tone(Tone::Ok).0);
        let points = [
            egui::pos2(c.x - 4.5, c.y),
            egui::pos2(c.x - 1.5, c.y + 3.0),
            egui::pos2(c.x + 4.5, c.y - 3.5),
        ];
        ui.painter().line_segment([points[0], points[1]], stroke);
        ui.painter().line_segment([points[1], points[2]], stroke);
    } else {
        let color = if response.hovered() {
            ui.visuals().text_color()
        } else {
            palette.dim
        };
        let stroke = egui::Stroke::new(1.2, color);
        let sheet = egui::vec2(7.0, 8.5);
        let back = egui::Rect::from_center_size(c + egui::vec2(-1.5, -1.5), sheet);
        let front = egui::Rect::from_center_size(c + egui::vec2(1.5, 1.5), sheet);
        ui.painter()
            .rect_stroke(back, 1.5, stroke, egui::StrokeKind::Middle);
        ui.painter()
            .rect_filled(front, 1.5, ui.visuals().faint_bg_color);
        ui.painter()
            .rect_stroke(front, 1.5, stroke, egui::StrokeKind::Middle);
    }
    response.on_hover_text(if copied {
        "copied"
    } else {
        "copy the console approval link"
    });
}

/// Remember that the approval link of a grant was just copied, so its copy
/// button shows a tick for a moment.
pub fn mark_link_copied(ctx: &egui::Context, grant: &str) {
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique(("copied-link", grant)), now));
}

/// Badge text, badge tone and timing line of a grant.
fn grant_status(g: &GrantRow, now: i64) -> (String, Tone, String) {
    let remaining = grant_remaining(g, now);
    let tone = match remaining {
        None => Tone::Warn,
        Some(0) => Tone::Neutral,
        Some(left) if left < EXPIRING_SECS => Tone::Error,
        Some(_) => Tone::Ok,
    };
    let state = match remaining {
        Some(0) => "expired".to_string(),
        Some(left) if left < EXPIRING_SECS => "expiring".to_string(),
        _ => grant_state_label(&g.state),
    };
    let timing = match remaining {
        Some(0) => "expired, leaving the list shortly".to_string(),
        Some(left) => format!(
            "{} left of {}",
            format_age(left),
            format_age(g.requested_duration_secs)
        ),
        None => format!("waiting {}", format_age(now - g.created_at)),
    };
    (state, tone, timing)
}

/// The selected grant as labelled fields, shown above its entitlement in
/// the details panel.
fn grant_fields(ui: &mut Ui, g: &GrantRow, palette: &Palette) {
    let (state, tone, timing) = grant_status(g, now_unix());
    section(ui, "Grant", palette, |ui| {
        value_row(ui, "Status", palette, |ui| {
            badge(ui, &state, tone);
        });
        ui.separator();
        value_row(ui, "Time", palette, |ui| {
            ui.label(timing);
        });
        ui.separator();
        value_row(ui, "Requested for", palette, |ui| {
            ui.label(format_age(g.requested_duration_secs));
        });
        ui.separator();
        text_row(ui, "Justification", palette, |ui| {
            match g.justification.as_deref().filter(|s| !s.is_empty()) {
                Some(why) => ui.add(egui::Label::new(why).wrap()),
                None => ui.label(RichText::new("none").color(palette.dim)),
            };
        });
        ui.separator();
        text_row(ui, "Grant name", palette, |ui| {
            ui.add(egui::Label::new(RichText::new(&g.name).small()).wrap());
        });
        if g.state == GrantState::ApprovalAwaited
            && let Some(url) = approval_console_url(&g.name)
        {
            ui.separator();
            ui.horizontal(|ui| {
                copy_link_button(ui, &g.name, url, palette);
                ui.label(RichText::new("copy the approval link (y)").color(palette.dim));
            });
        }
    });
}

/// How one grant card is drawn.
#[derive(Clone, Copy)]
struct GrantCard {
    now: i64,
    width: f32,
    selected: bool,
    /// Selected while the grants panel has the keyboard.
    focused: bool,
    /// Scroll the panel so this card is in view.
    scroll: bool,
}

/// One grant as a card: name and state, scope, then the time left with a
/// bar that empties as the grant runs out. A click selects it, which shows
/// it in the details panel.
fn grant_card(
    ui: &mut Ui,
    g: &GrantRow,
    card: GrantCard,
    palette: &Palette,
    out: &mut Vec<Intent>,
) {
    let GrantCard { now, width, .. } = card;
    let remaining = grant_remaining(g, now);
    let (state, tone, timing) = grant_status(g, now);
    let stroke = if card.selected {
        egui::Stroke::new(1.5, palette.accent)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    let fill = if card.focused {
        ui.visuals()
            .selection
            .bg_fill
            .lerp_to_gamma(ui.visuals().faint_bg_color, 0.5)
    } else {
        ui.visuals().faint_bg_color
    };
    let frame = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 8));
    let inner_width = width - 20.0 - 2.0 * stroke.width;
    // The card senses clicks as a whole, and the copy button inside it
    // still gets its own.
    let builder = egui::UiBuilder::new()
        .id_salt(("grant-card", g.name.as_str()))
        .sense(Sense::click());
    let card_response = ui
        .scope_builder(builder, |ui| {
            frame.show(ui, |ui| {
                ui.set_width(inner_width);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            badge(ui, &state, tone);
                            // A grant waiting for approval offers its console
                            // approval page as a link to hand to approvers.
                            if g.state == GrantState::ApprovalAwaited
                                && let Some(url) = approval_console_url(&g.name)
                            {
                                copy_link_button(ui, &g.name, url, palette);
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.add(
                                    egui::Label::new(bold(&g.entitlement_short_name)).truncate(),
                                );
                            });
                        });
                    });
                    ui.horizontal(|ui| {
                        scope_icon(ui, g.scope_type, palette.dim);
                        let name = g
                            .scope_display_name
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&g.scope_id);
                        ui.add(egui::Label::new(name).truncate());
                    });
                    ui.add_space(2.0);
                    ui.label(RichText::new(timing).small().color(palette.dim));
                    // The bar shows the share of the requested time still left.
                    let (bar, _) =
                        ui.allocate_exact_size(egui::vec2(inner_width, 4.0), Sense::hover());
                    let (fg, _) = palette.tone(tone);
                    ui.painter().rect_filled(
                        bar,
                        2.0,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    );
                    if let Some(left) = remaining {
                        let share = left as f32 / g.requested_duration_secs.max(1) as f32;
                        let mut filled = bar;
                        filled.set_width(bar.width() * share.clamp(0.0, 1.0));
                        ui.painter().rect_filled(filled, 2.0, fg);
                    }
                });
            });
        })
        .response;
    if card.scroll {
        card_response.scroll_to_me(None);
    }
    if card_response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("click to show this grant in the details panel")
        .clicked()
    {
        out.push(Intent::SelectGrant(g.name.clone()));
    }
}

fn status_bar(ui: &mut Ui, state: &BrowseState, palette: &Palette, out: &mut Vec<Intent>) {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Icon buttons, added right to left: theme, help, logs.
            ui.spacing_mut().item_spacing.x = 2.0;
            let theme = ui.ctx().options(|o| o.theme_preference);
            let (name, icon) = match theme {
                egui::ThemePreference::System => ("auto", Icon::ThemeAuto),
                egui::ThemePreference::Light => ("light", Icon::ThemeLight),
                egui::ThemePreference::Dark => ("dark", Icon::ThemeDark),
            };
            let hint = format!("theme: {name}, click to change (T)");
            if icon_button(ui, icon, &hint, palette.status_bg) {
                out.push(Intent::CycleTheme);
            }
            if icon_button(ui, Icon::Help, "help (?)", palette.status_bg) {
                out.push(Intent::Browse(Action::Help));
            }
            if icon_button(ui, Icon::Logs, "logs (L)", palette.status_bg) {
                out.push(Intent::Browse(Action::ShowLogs));
            }
            ui.spacing_mut().item_spacing.x = 8.0;
            let unread = state.unread_log_count();
            if unread > 0
                && badge_button(
                    ui,
                    &format!("{unread} warning{}", if unread == 1 { "" } else { "s" }),
                    Tone::Error,
                )
                .on_hover_text("L")
                .clicked()
            {
                out.push(Intent::Browse(Action::ShowLogs));
            }
            let pending = state.pending_approvals();
            if pending > 0
                && badge_button(ui, &format!("{pending} to approve"), Tone::Warn)
                    .on_hover_text("A")
                    .clicked()
            {
                out.push(Intent::Browse(Action::OpenApprovalQueue));
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                plain(ui, bold(state.account()));
                if state.scope_filter().is_some() {
                    ui.separator();
                    badge(
                        ui,
                        &format!("type: {}", state.scope_filter_label()),
                        Tone::Info,
                    );
                }
                ui.separator();
                if state.refresh_in_flight() {
                    badge(ui, "refreshing", Tone::Info);
                } else {
                    let age = format!("cache {} old", state.cache_age_label(now_unix()));
                    if refresh_info(ui, &age, palette) {
                        out.push(Intent::Browse(Action::Refresh));
                    }
                }
                if !state.status().is_empty() {
                    ui.separator();
                    plain(ui, state.status());
                }
            });
        });
    });
}

/// The cache age in the status bar, with a circular arrow in front. A click
/// on it refreshes the entitlement list.
fn refresh_info(ui: &mut Ui, text: &str, palette: &Palette) -> bool {
    const ICON: f32 = 14.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, palette.dim);
    let size = egui::vec2(ICON + 5.0 + galley.size().x, galley.size().y.max(ICON));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let color = if response.hovered() {
        ui.visuals().text_color()
    } else {
        palette.dim
    };
    // Three quarters of a circle, ending in an arrow head.
    let center = egui::pos2(rect.left() + ICON / 2.0, rect.center().y);
    let radius = ICON * 0.36;
    let at = |angle: f32| center + radius * egui::vec2(angle.cos(), angle.sin());
    let start = -std::f32::consts::FRAC_PI_2;
    let sweep = 1.5 * std::f32::consts::PI;
    let points: Vec<egui::Pos2> = (0..=18)
        .map(|i| at(start + sweep * i as f32 / 18.0))
        .collect();
    ui.painter()
        .add(egui::Shape::line(points, egui::Stroke::new(1.4, color)));
    let tip = at(start + sweep);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            tip + egui::vec2(0.0, -3.5),
            tip + egui::vec2(0.0, 3.5),
            tip + egui::vec2(4.0, 0.0),
        ],
        color,
        egui::Stroke::NONE,
    ));
    ui.painter().galley_with_override_text_color(
        egui::pos2(
            rect.left() + ICON + 5.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("refresh now (R)")
        .clicked()
}

/// Drawn icons for the status bar buttons.
#[derive(Clone, Copy)]
enum Icon {
    /// Lines of text.
    Logs,
    /// A question mark in a circle.
    Help,
    /// A half-filled circle: the theme follows the system.
    ThemeAuto,
    /// A sun.
    ThemeLight,
    /// A crescent moon.
    ThemeDark,
}

/// A borderless button showing a drawn icon, with a tooltip. `backdrop` is
/// the colour behind the button, used to cut shapes out of an icon. Returns
/// true on click.
fn icon_button(ui: &mut Ui, icon: Icon, hint: &str, backdrop: egui::Color32) -> bool {
    use std::f32::consts::TAU;

    let (rect, response) = ui.allocate_exact_size(egui::vec2(24.0, 20.0), Sense::click());
    let visuals = ui.style().interact(&response);
    let color = visuals.fg_stroke.color;
    let behind = if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, visuals.weak_bg_fill);
        visuals.weak_bg_fill
    } else {
        backdrop
    };
    let stroke = egui::Stroke::new(1.5, color);
    let c = rect.center();
    let painter = ui.painter();
    match icon {
        Icon::Logs => {
            for (dy, width) in [(-4.5, 12.0), (0.0, 9.0), (4.5, 12.0)] {
                let left = egui::pos2(c.x - 6.0, c.y + dy);
                painter.line_segment([left, left + egui::vec2(width, 0.0)], stroke);
            }
        }
        Icon::Help => {
            painter.circle_stroke(c, 7.5, stroke);
            painter.text(
                c,
                egui::Align2::CENTER_CENTER,
                "?",
                egui::FontId::new(11.0, bold_family()),
                color,
            );
        }
        Icon::ThemeAuto => {
            // The left half of the disc is filled.
            let radius = 7.0;
            let half: Vec<egui::Pos2> = (0..=16)
                .map(|i| {
                    let angle = TAU * 0.25 + TAU * 0.5 * i as f32 / 16.0;
                    c + radius * egui::vec2(angle.cos(), angle.sin())
                })
                .collect();
            painter.add(egui::Shape::convex_polygon(half, color, egui::Stroke::NONE));
            painter.circle_stroke(c, radius, stroke);
        }
        Icon::ThemeLight => {
            painter.circle_filled(c, 3.4, color);
            for i in 0..8 {
                let angle = TAU * i as f32 / 8.0;
                let ray = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment([c + 5.6 * ray, c + 8.0 * ray], stroke);
            }
        }
        Icon::ThemeDark => {
            // A disc with a second disc cut out of its upper right.
            painter.circle_filled(c, 7.0, color);
            painter.circle_filled(c + egui::vec2(3.2, -2.6), 5.8, behind);
        }
    }
    response.on_hover_text(hint).clicked()
}

/// Help or logs overlay, when open. Any key closes them (handled by the
/// key map), as does a click outside or on the close button.
/// Draw the help or logs overlay when one is open. `scroll` is how far the
/// keyboard asked the overlay to scroll this frame.
pub fn show_overlays(ctx: &egui::Context, state: &BrowseState, scroll: f32, out: &mut Vec<Intent>) {
    // An overlay never outgrows the window. What does not fit scrolls.
    let window = ctx.content_rect();
    let max = egui::vec2(
        (window.width() - 64.0).max(240.0),
        (window.height() - 130.0).max(120.0),
    );
    if state.show_help() {
        let response = Modal::new(egui::Id::unique("gpam-help")).show(ctx, |ui| {
            ui.set_max_width(max.x);
            ui.heading("Help");
            ui.label(
                RichText::new("j / k scroll, any other key closes")
                    .small()
                    .color(Palette::current(ui).dim),
            );
            ui.add_space(6.0);
            ScrollArea::both()
                .id_salt("help-scroll")
                .max_width(max.x)
                .max_height(max.y)
                .show(ui, |ui| {
                    if scroll != 0.0 {
                        ui.scroll_with_delta(egui::vec2(0.0, -scroll));
                    }
                    help(ui, max.x);
                });
            close_button(ui, out);
        });
        if response.backdrop_response.clicked() {
            out.push(Intent::CloseOverlay);
        }
    }
    if state.show_logs() {
        let response = Modal::new(egui::Id::unique("gpam-logs")).show(ctx, |ui| {
            ui.set_max_width(max.x.min(640.0));
            ui.heading("Logs");
            ui.label(
                RichText::new("j / k scroll, any other key closes")
                    .small()
                    .color(Palette::current(ui).dim),
            );
            ui.add_space(6.0);
            ScrollArea::vertical()
                .id_salt("logs-scroll")
                .max_height(max.y)
                .show(ui, |ui| {
                    if scroll != 0.0 {
                        ui.scroll_with_delta(egui::vec2(0.0, -scroll));
                    }
                    logs(ui, state);
                });
            close_button(ui, out);
        });
        if response.backdrop_response.clicked() {
            out.push(Intent::CloseOverlay);
        }
    }
}

fn close_button(ui: &mut Ui, out: &mut Vec<Intent>) {
    ui.add_space(6.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.button("close").clicked() {
            out.push(Intent::CloseOverlay);
        }
    });
}

/// Key reference. The sections are spread over as many columns as fit in
/// `width`, so a small window gets a tall list to scroll and a large one
/// shows everything at once.
fn help(ui: &mut Ui, width: f32) {
    const COLUMN_WIDTH: f32 = 330.0;
    let palette = Palette::current(ui);
    // The shared table lists the terminal's quit keys, which differ here.
    let list: Vec<(&str, &str)> = LIST_KEYS
        .iter()
        .copied()
        .filter(|(_, what)| *what != "quit")
        .chain([(QUIT_KEYS, "quit")])
        .collect();
    let search: Vec<(&str, &str)> = SEARCH_KEYS.iter().chain(SEARCH_HINTS).copied().collect();
    let sections: [(&str, &[(&str, &str)]); 5] = [
        ("list", &list),
        ("search", &search),
        ("scopes", NAVIGATOR_HINTS),
        ("grants", GRANT_HINTS),
        ("window", WINDOW_HINTS),
    ];
    let columns = ((width / COLUMN_WIDTH) as usize).clamp(1, 3);
    // A section costs its rows plus a title and a gap.
    let total: usize = sections.iter().map(|(_, rows)| rows.len() + 2).sum();
    let per_column = total.div_ceil(columns);
    let mut next = 0;
    ui.horizontal_top(|ui| {
        for column in 0..columns {
            let last = column + 1 == columns;
            ui.vertical(|ui| {
                let mut used = 0;
                while let Some((title, rows)) = sections.get(next) {
                    let cost = rows.len() + 2;
                    if !last && used > 0 && used + cost > per_column {
                        break;
                    }
                    key_table(ui, title, rows, &palette);
                    ui.add_space(12.0);
                    used += cost;
                    next += 1;
                }
                if last {
                    ui.label(bold("scope icons"));
                    for scope in [Scope::Organization, Scope::Folder, Scope::Project] {
                        ui.horizontal(|ui| {
                            scope_icon(ui, scope, palette.dim);
                            ui.label(RichText::new(scope_kind(scope)).color(palette.dim));
                        });
                    }
                }
            });
            if !last {
                ui.add_space(24.0);
            }
        }
    });
}

fn key_table(ui: &mut Ui, title: &str, rows: &[(&str, &str)], palette: &Palette) {
    ui.vertical(|ui| {
        ui.label(bold(title));
        egui::Grid::new(title)
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for (keys, desc) in rows {
                    ui.label(RichText::new(*keys).monospace().color(palette.info));
                    ui.label(RichText::new(*desc).color(palette.dim));
                    ui.end_row();
                }
            });
    });
}

fn logs(ui: &mut Ui, state: &BrowseState) {
    let palette = Palette::current(ui);
    let (last, log_path) = state.log_snapshot();
    match last {
        None => {
            ui.label(RichText::new("no warnings or errors recorded yet").color(palette.dim));
        }
        Some(entry) => {
            let color = if entry.level == tracing::Level::ERROR {
                palette.error
            } else {
                palette.pending
            };
            let age = entry
                .at
                .elapsed()
                .map(|d| format_age(d.as_secs() as i64))
                .unwrap_or_else(|_| "0s".into());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(entry.level.to_string())
                        .color(color)
                        .family(bold_family()),
                );
                ui.label(RichText::new(format!("({age} ago)")).color(palette.dim));
            });
            ui.add(egui::Label::new(entry.message).wrap());
        }
    }
    ui.add_space(8.0);
    ui.label(RichText::new("full log:").color(palette.dim));
    ui.add(egui::Label::new(RichText::new(log_path).monospace()).wrap());
}
