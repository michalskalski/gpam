//! Request flow modal: duration and justification for each selected
//! entitlement, the results so far, and the closing summary.

use eframe::egui::{self, Align, Layout, Modal, RichText, Sense, TextEdit, Ui};
use gpam_core::app::format::format_age;
use gpam_core::app::request::{
    Field, RequestAction, RequestFlow, RequestResult, ResultState, validate_duration,
};
use gpam_core::cache::EntitlementRow;
use gpam_core::gcp::grants::MIN_DURATION_SECS;

use crate::app::{Intent, RequestScreen, ids};
use crate::style::{
    ButtonKind, DIALOG_GAP, Palette, Tone, badge, banner, bold, bold_family, card, dialog_frame,
    dialog_title, key_button, scope_icon, segmented, text_input,
};

const WIDTH: f32 = 440.0;

/// Durations offered as one-click choices. Those outside an entitlement's
/// limits are left out.
const QUICK_DURATIONS: &[&str] = &["30m", "1h", "2h", "4h", "8h"];

/// More steps than this are shown as a count only.
const MAX_STEP_DOTS: usize = 24;

pub fn show(ctx: &egui::Context, screen: &mut RequestScreen, out: &mut Vec<Intent>) {
    Modal::new(egui::Id::unique("gpam-request"))
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            if screen.summary {
                summary(ui, &screen.flow, out);
            } else {
                prompt(ui, &mut screen.flow, screen.busy, out);
            }
        });
}

fn prompt(ui: &mut Ui, flow: &mut RequestFlow, busy: bool, out: &mut Vec<Intent>) {
    let palette = Palette::current(ui);
    dialog_title(ui, "Request access", |ui| {
        if flow.total() > 1 {
            ui.label(
                RichText::new(format!("{} of {}", flow.position() + 1, flow.total()))
                    .color(palette.dim),
            );
            step_dots(ui, flow, &palette);
        }
    });
    if !flow.results().is_empty() {
        card(ui, |ui| results(ui, flow.results(), &palette));
        ui.add_space(DIALOG_GAP);
    }

    let Some(modal) = flow.current() else {
        return;
    };
    let field = modal.field();
    let ent = modal.entitlement().clone();
    let mut duration = modal.duration().as_str().to_owned();
    let mut justification = modal.justification().as_str().to_owned();
    let error = modal.error().map(str::to_owned);

    card(ui, |ui| entitlement_summary(ui, &ent, &palette));
    ui.add_space(DIALOG_GAP);

    ui.add_enabled_ui(!busy, |ui| {
        // Duration: a field for any value, joined by the usual values as
        // one-click choices, with the allowed range underneath.
        field_label(ui, "Duration", field == Field::Duration, &palette);
        let max = ent.max_request_duration_secs;
        let choices: Vec<&str> = QUICK_DURATIONS
            .iter()
            .copied()
            .filter(|choice| validate_duration(choice, max).is_ok())
            .collect();
        ui.horizontal(|ui| {
            let edit = TextEdit::singleline(&mut duration)
                .id(ids::duration())
                .desired_width(72.0);
            if text_input(ui, edit, field == Field::Duration).changed() {
                flow.set_duration(&duration);
            }
            let chosen = choices.iter().position(|c| *c == duration.trim());
            if let Some(i) = segmented(ui, &choices, chosen) {
                flow.set_duration(choices[i]);
            }
        });
        let limits = match max.filter(|s| *s > 0) {
            Some(max) => format!(
                "Between {} and {}",
                format_age(MIN_DURATION_SECS),
                format_age(max)
            ),
            None => format!("At least {}", format_age(MIN_DURATION_SECS)),
        };
        ui.add_space(2.0);
        ui.label(RichText::new(limits).small().color(palette.dim));
        ui.add_space(DIALOG_GAP);

        if ent.justification_required {
            field_label(ui, "Justification", field == Field::Justification, &palette);
            // The text is one line that wraps, so a long justification
            // stays visible. Enter confirms the dialog and never reaches
            // the field, and pasted line breaks become spaces.
            let edit = TextEdit::multiline(&mut justification)
                .id(ids::justification())
                .hint_text("Why you need this access")
                .desired_rows(1)
                .desired_width(f32::INFINITY);
            if text_input(ui, edit, field == Field::Justification).changed() {
                flow.set_justification(&justification.replace(['\n', '\r'], " "));
            }
        } else {
            ui.label(
                RichText::new("No justification is needed for this entitlement.")
                    .small()
                    .color(palette.dim),
            );
        }
    });

    if let Some(err) = error {
        ui.add_space(DIALOG_GAP);
        banner(ui, &err, Tone::Error);
    }
    ui.add_space(DIALOG_GAP + 4.0);

    if busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(format!("Requesting {}", ent.short_name)).color(palette.dim));
        });
        return;
    }
    // Buttons at the right with the main one last. Each shows its key.
    let several = flow.total() > 1;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if key_button(ui, "Request", "enter", ButtonKind::Primary, true).clicked() {
                out.push(Intent::Request(RequestAction::Confirm));
            }
            let skip = if several { "Skip" } else { "Cancel" };
            if key_button(ui, skip, "esc", ButtonKind::Secondary, true).clicked() {
                out.push(Intent::Request(RequestAction::Skip));
            }
            if several
                && key_button(ui, "Cancel all", "shift-esc", ButtonKind::Ghost, true).clicked()
            {
                out.push(Intent::Request(RequestAction::Abort));
            }
        });
    });
}

/// Name, scope and approval mode of the entitlement being requested.
fn entitlement_summary(ui: &mut Ui, ent: &EntitlementRow, palette: &Palette) {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ent.approvers.is_empty() {
                badge(ui, "auto-approved", Tone::Ok);
            } else {
                badge(ui, "needs approval", Tone::Warn).on_hover_text(ent.approvers.join("\n"));
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(&ent.short_name)
                            .family(bold_family())
                            .size(15.0),
                    )
                    .truncate(),
                );
            });
        });
    });
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        scope_icon(ui, ent.scope_type, palette.dim);
        let id = format!("{}/{}", ent.scope_type.plural(), ent.scope_id);
        let name = ent
            .scope_display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map_or(id, str::to_owned);
        ui.add(egui::Label::new(RichText::new(name).color(palette.dim)).truncate());
    });
}

/// One dot per entitlement in the flow: coloured by outcome when handled,
/// filled for the current one and hollow for those still to come.
fn step_dots(ui: &mut Ui, flow: &RequestFlow, palette: &Palette) {
    let total = flow.total();
    if total > MAX_STEP_DOTS {
        return;
    }
    const STEP: f32 = 12.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(STEP * total as f32, 12.0), Sense::hover());
    for i in 0..total {
        let center = egui::pos2(rect.left() + STEP * (i as f32 + 0.5), rect.center().y);
        let outcome = flow.results().get(i).map(|r| match r.state {
            ResultState::Ok(_) => palette.tone(Tone::Ok).0,
            ResultState::Err(_) => palette.tone(Tone::Error).0,
            ResultState::Skipped => palette.dim,
        });
        match outcome {
            Some(color) => {
                ui.painter().circle_filled(center, 3.5, color);
            }
            None if i == flow.position() => {
                ui.painter().circle_filled(center, 4.5, palette.accent);
            }
            None => {
                ui.painter()
                    .circle_stroke(center, 3.0, egui::Stroke::new(1.0, palette.dim));
            }
        }
    }
}

/// The name of a field, above it. It takes the accent colour while its
/// field is the one being edited.
fn field_label(ui: &mut Ui, label: &str, focused: bool, palette: &Palette) {
    let text = bold(label).small();
    ui.label(if focused {
        text.color(palette.accent)
    } else {
        text
    });
    ui.add_space(4.0);
}

/// One line per handled entitlement: outcome badge, name, then the grant
/// ID or the error underneath.
fn results(ui: &mut Ui, results: &[RequestResult], palette: &Palette) {
    for (i, r) in results.iter().enumerate() {
        if i > 0 {
            ui.add_space(6.0);
        }
        let (text, tone) = match r.state {
            ResultState::Ok(_) => ("requested", Tone::Ok),
            ResultState::Err(_) => ("failed", Tone::Error),
            ResultState::Skipped => ("skipped", Tone::Neutral),
        };
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                badge(ui, text, tone);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(&r.short_name).truncate());
                });
            });
        });
        match &r.state {
            ResultState::Ok(name) => {
                ui.add(egui::Label::new(RichText::new(name).small().color(palette.dim)).truncate());
            }
            ResultState::Err(msg) => {
                ui.add(egui::Label::new(RichText::new(msg).small().color(palette.error)).wrap());
            }
            ResultState::Skipped => {}
        }
    }
}

fn summary(ui: &mut Ui, flow: &RequestFlow, out: &mut Vec<Intent>) {
    let palette = Palette::current(ui);
    let (ok, failed, skipped) = flow.counts();
    dialog_title(ui, "Request summary", |ui| {
        if skipped > 0 {
            badge(ui, &format!("{skipped} skipped"), Tone::Neutral);
        }
        if failed > 0 {
            badge(ui, &format!("{failed} failed"), Tone::Error);
        }
        if ok > 0 {
            badge(ui, &format!("{ok} requested"), Tone::Ok);
        }
    });
    egui::ScrollArea::vertical()
        .max_height(360.0)
        .show(ui, |ui| {
            card(ui, |ui| results(ui, flow.results(), &palette));
        });
    ui.add_space(DIALOG_GAP + 4.0);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if key_button(ui, "Continue", "enter", ButtonKind::Primary, true).clicked() {
                out.push(Intent::DismissSummary);
            }
        });
    });
}
