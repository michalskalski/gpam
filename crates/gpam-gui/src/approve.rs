//! Approve modal for one forwarded grant: details, approve or deny with an
//! optional reason, and the result.

use eframe::egui::{self, Modal, RichText, TextEdit, Ui};
use gpam_core::app::approve::{
    ApproveAction, ApproveModal, Decision, Step, StepResult, detail_fields, role_binding_label,
};
use gpam_core::gcp::grants::GrantDetails;

use crate::app::{ApproveScreen, Intent, ids};
use crate::style::{Palette, Tone, badge, bold_family};

const WIDTH: f32 = 600.0;

pub fn show(ctx: &egui::Context, screen: &mut ApproveScreen, out: &mut Vec<Intent>) {
    Modal::new(egui::Id::unique("gpam-approve")).show(ctx, |ui| {
        ui.set_width(WIDTH);
        let palette = Palette::current(ui);
        ui.label(
            RichText::new("approve grant")
                .color(palette.accent)
                .family(bold_family()),
        );
        ui.separator();
        match screen {
            ApproveScreen::Loading(name) => {
                ui.add(egui::Label::new(RichText::new(name.as_str()).monospace()).wrap());
                ui.add_space(6.0);
                ui.label(RichText::new("working: loading grant...").color(palette.dim));
            }
            ApproveScreen::Submitting { details, step } => {
                if let Some(d) = details {
                    grant_details(ui, d, &palette);
                }
                ui.add_space(6.0);
                let label = match step {
                    Step::Reason(decision) => decision.label(),
                    _ => "decision",
                };
                ui.label(RichText::new(format!("working: sending {label}...")).color(palette.dim));
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
        Step::Confirm | Step::Reason(_) => {
            if let Some(d) = modal.details() {
                ui.heading(format!("grant: {}", d.entitlement_short_name));
                grant_details(ui, d, palette);
            }
            ui.add_space(8.0);
        }
        Step::Result(_) => {
            let title = if entitlement.is_empty() {
                "result".to_string()
            } else {
                format!("grant: {entitlement}")
            };
            ui.heading(title);
        }
    }

    match &step {
        Step::Confirm => {
            ui.horizontal(|ui| {
                if ui.button("approve").clicked() {
                    out.push(Intent::Approve(ApproveAction::Choose(Decision::Approve)));
                }
                if ui.button("deny").clicked() {
                    out.push(Intent::Approve(ApproveAction::Choose(Decision::Deny)));
                }
                if ui.button("cancel").clicked() {
                    out.push(Intent::Approve(ApproveAction::Cancel));
                }
            });
        }
        Step::Reason(decision) => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("reason ({}):", decision.label()))
                        .color(palette.accent)
                        .family(bold_family()),
                );
                let mut reason = modal.reason().as_str().to_owned();
                let response = TextEdit::singleline(&mut reason)
                    .id(ids::reason())
                    .desired_width(f32::INFINITY)
                    .show(ui)
                    .response;
                if response.changed() {
                    modal.set_reason(&reason);
                }
            });
            ui.label(
                RichText::new("enter to confirm  |  esc to go back")
                    .small()
                    .color(palette.dim),
            );
            ui.horizontal(|ui| {
                if ui.button(decision.label()).clicked() {
                    out.push(Intent::Approve(ApproveAction::Submit));
                }
                if ui.button("back").clicked() {
                    out.push(Intent::Approve(ApproveAction::Back));
                }
            });
        }
        Step::Result(result) => {
            let tone = match result {
                StepResult::Ok(_) => Tone::Ok,
                StepResult::AlreadyHandled => Tone::Info,
                StepResult::Err(_) => Tone::Error,
            };
            let text = match result {
                StepResult::Ok(decision) => decision.past_tense(),
                StepResult::AlreadyHandled => "already handled",
                StepResult::Err(_) => "error",
            };
            badge(ui, text, tone);
            let message = RichText::new(result.message(&entitlement));
            let message = if matches!(result, StepResult::Err(_)) {
                message.color(palette.error)
            } else {
                message
            };
            ui.add(egui::Label::new(message).wrap());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("dismiss").clicked() {
                    out.push(Intent::Approve(ApproveAction::Dismiss));
                }
                ui.label(
                    RichText::new("press enter / esc / q to dismiss")
                        .small()
                        .color(palette.dim),
                );
            });
        }
    }

    ui.add_space(4.0);
    let keys = match step {
        Step::Confirm => "a approve | d deny | esc cancel",
        Step::Reason(_) => "enter confirm | esc back",
        Step::Result(_) => "enter / esc / q dismiss",
    };
    ui.label(RichText::new(keys).small().color(palette.dim));
}

fn grant_details(ui: &mut Ui, d: &GrantDetails, palette: &Palette) {
    egui::Grid::new("approve-details")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (label, value) in detail_fields(d) {
                ui.label(RichText::new(label).color(palette.dim));
                ui.add(egui::Label::new(value).wrap());
                ui.end_row();
            }
            ui.label(RichText::new("roles").color(palette.dim));
            ui.vertical(|ui| {
                if d.role_bindings.is_empty() {
                    ui.label("(none listed)");
                }
                for rb in &d.role_bindings {
                    ui.add(
                        egui::Label::new(RichText::new(role_binding_label(rb)).monospace()).wrap(),
                    );
                }
            });
            ui.end_row();
            ui.label(RichText::new("justify").color(palette.dim));
            ui.add(egui::Label::new(d.justification.as_deref().unwrap_or("(none)")).wrap());
            ui.end_row();
        });
}
