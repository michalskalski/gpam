//! Approve modal for one forwarded grant: details, approve or deny with an
//! optional reason, and the result.

use eframe::egui::{self, Align, Layout, Modal, RichText, TextEdit, Ui};
use gpam_core::app::approve::{ApproveAction, ApproveModal, Decision, Step, StepResult};
use gpam_core::app::format::format_age;
use gpam_core::gcp::grants::{GrantDetails, GrantState};

use crate::app::{ApproveScreen, Intent, ids};
use crate::browse::grant_state_label;
use crate::style::{
    ButtonKind, DIALOG_GAP, Palette, Tone, badge, banner, bold, bold_family, card, dialog_frame,
    dialog_title, key_button, scope_icon, text_input, text_row, value_row,
};

const WIDTH: f32 = 480.0;

pub fn show(ctx: &egui::Context, screen: &mut ApproveScreen, out: &mut Vec<Intent>) {
    Modal::new(egui::Id::unique("gpam-approve"))
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            let palette = Palette::current(ui);
            match screen {
                ApproveScreen::Loading(name) => {
                    dialog_title(ui, "Review grant", |_| {});
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Loading the grant").color(palette.dim));
                    });
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(RichText::new(name.as_str()).small().color(palette.dim))
                            .wrap(),
                    );
                }
                ApproveScreen::Submitting { details, step } => {
                    dialog_title(ui, "Review grant", |_| {});
                    if let Some(d) = details {
                        grant_details(ui, d, &palette);
                    }
                    let sending = match step {
                        Step::Reason(Decision::Deny) => "Sending the denial",
                        _ => "Sending the approval",
                    };
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(sending).color(palette.dim));
                    });
                }
                ApproveScreen::Ready(modal) => ready(ui, modal, &palette, out),
            }
        });
}

fn ready(ui: &mut Ui, modal: &mut ApproveModal, palette: &Palette, out: &mut Vec<Intent>) {
    let step = modal.step().clone();
    let entitlement = modal
        .details()
        .map(|d| d.entitlement_short_name.clone())
        .unwrap_or_default();
    match &step {
        Step::Confirm => {
            let state = modal.details().map(|d| d.state.clone());
            dialog_title(ui, "Review grant", |ui| {
                if let Some(state) = state {
                    let tone = if state == GrantState::ApprovalAwaited {
                        Tone::Warn
                    } else {
                        Tone::Neutral
                    };
                    badge(ui, &grant_state_label(&state), tone);
                }
            });
            if let Some(d) = modal.details() {
                grant_details(ui, d, palette);
            }
            ui.add_space(4.0);
            buttons(ui, |ui| {
                if key_button(ui, "Approve", "a", ButtonKind::Primary, true).clicked() {
                    out.push(Intent::Approve(ApproveAction::Choose(Decision::Approve)));
                }
                if key_button(ui, "Deny", "d", ButtonKind::Secondary, true).clicked() {
                    out.push(Intent::Approve(ApproveAction::Choose(Decision::Deny)));
                }
                if key_button(ui, "Cancel", "esc", ButtonKind::Ghost, true).clicked() {
                    out.push(Intent::Approve(ApproveAction::Cancel));
                }
            });
        }
        Step::Reason(decision) => {
            let (title, label, action) = match decision {
                Decision::Approve => ("Approve grant", "Reason for approving", "Approve"),
                Decision::Deny => ("Deny grant", "Reason for denying", "Deny"),
            };
            dialog_title(ui, title, |_| {});
            if let Some(d) = modal.details() {
                grant_details(ui, d, palette);
            }
            ui.label(bold(label).small().color(palette.accent));
            ui.add_space(4.0);
            let mut reason = modal.reason().as_str().to_owned();
            let edit = TextEdit::singleline(&mut reason)
                .id(ids::reason())
                .hint_text("Optional")
                .desired_width(f32::INFINITY);
            if text_input(ui, edit, true).changed() {
                modal.set_reason(&reason);
            }
            ui.add_space(DIALOG_GAP + 4.0);
            buttons(ui, |ui| {
                if key_button(ui, action, "enter", ButtonKind::Primary, true).clicked() {
                    out.push(Intent::Approve(ApproveAction::Submit));
                }
                if key_button(ui, "Back", "esc", ButtonKind::Secondary, true).clicked() {
                    out.push(Intent::Approve(ApproveAction::Back));
                }
            });
        }
        Step::Result(result) => {
            let (title, tone) = match result {
                StepResult::Ok(Decision::Approve) => ("Grant approved", Tone::Ok),
                StepResult::Ok(Decision::Deny) => ("Grant denied", Tone::Ok),
                StepResult::AlreadyHandled => ("Already handled", Tone::Info),
                StepResult::Err(_) => ("Something went wrong", Tone::Error),
            };
            dialog_title(ui, title, |_| {});
            let message = match result {
                StepResult::Ok(decision) => {
                    format!("{entitlement} was {}.", decision.past_tense())
                }
                StepResult::AlreadyHandled => {
                    format!("{entitlement} was already approved or denied by someone else.")
                }
                StepResult::Err(message) => message.clone(),
            };
            banner(ui, &message, tone);
            ui.add_space(DIALOG_GAP + 4.0);
            buttons(ui, |ui| {
                if key_button(ui, "Done", "enter", ButtonKind::Primary, true).clicked() {
                    out.push(Intent::Approve(ApproveAction::Dismiss));
                }
            });
        }
    }
}

/// The dialog's buttons, at the right with the first one added last in
/// reading order.
fn buttons(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), add);
    });
}

/// What is being asked for: the entitlement and its scope in a soft card,
/// then who asks, for how long, which roles and why.
fn grant_details(ui: &mut Ui, d: &GrantDetails, palette: &Palette) {
    card(ui, |ui| {
        ui.add(
            egui::Label::new(
                RichText::new(&d.entitlement_short_name)
                    .family(bold_family())
                    .size(15.0),
            )
            .truncate(),
        );
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            scope_icon(ui, d.scope_type, palette.dim);
            let scope = format!("{}/{}", d.scope_type.plural(), d.scope_id);
            ui.add(egui::Label::new(RichText::new(scope).color(palette.dim)).truncate());
        });
    });
    ui.add_space(DIALOG_GAP);
    value_row(ui, "Requester", palette, |ui| {
        ui.add(egui::Label::new(&d.requester).truncate());
    });
    ui.separator();
    value_row(ui, "Duration", palette, |ui| {
        ui.label(format_age(d.requested_duration_secs));
    });
    ui.separator();
    text_row(ui, "Roles", palette, |ui| {
        if d.role_bindings.is_empty() {
            ui.label(RichText::new("none listed").color(palette.dim));
        }
        for rb in &d.role_bindings {
            // Full role names. This is where precision matters.
            ui.add(egui::Label::new(&rb.role).wrap());
            if let Some(condition) = &rb.condition {
                ui.add(
                    egui::Label::new(RichText::new(condition).small().color(palette.dim)).wrap(),
                );
            }
        }
    });
    ui.separator();
    text_row(ui, "Justification", palette, |ui| {
        match d.justification.as_deref().filter(|s| !s.is_empty()) {
            Some(why) => ui.add(egui::Label::new(why).wrap()),
            None => ui.label(RichText::new("none").color(palette.dim)),
        };
    });
    ui.separator();
    // The full resource name, to match against the request you were told
    // about.
    text_row(ui, "Grant", palette, |ui| {
        ui.add(egui::Label::new(RichText::new(&d.name).small()).wrap());
    });
    ui.add_space(DIALOG_GAP);
}
