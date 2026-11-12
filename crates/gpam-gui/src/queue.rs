//! Approval queue modal: grants forwarded over the local socket and waiting
//! for review.

use std::time::SystemTime;

use eframe::egui::{self, Align, Layout, Modal, RichText, Sense};
use gpam_core::app::approval_queue::{QueueAction, QueueView};
use gpam_core::gcp::grants::parse_grant_name;

use crate::app::Intent;
use crate::style::{
    ButtonKind, DIALOG_GAP, Palette, bold, dialog_frame, dialog_title, key_button, scope_icon,
};

const WIDTH: f32 = 520.0;

pub fn show(ctx: &egui::Context, view: &QueueView, scroll_to_cursor: bool, out: &mut Vec<Intent>) {
    let max_height = (ctx.content_rect().height() * 0.6).max(160.0);
    Modal::new(egui::Id::unique("gpam-queue"))
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            let palette = Palette::current(ui);
            let rows = view.rows();
            dialog_title(ui, "Pending approvals", |ui| {
                let count = match rows.len() {
                    1 => "1 request".to_owned(),
                    n => format!("{n} requests"),
                };
                ui.label(RichText::new(count).color(palette.dim));
            });

            let now = SystemTime::now();
            let mut intent = None;
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| {
                    for (pos, q) in rows.iter().enumerate() {
                        let selected = pos == view.cursor();
                        let parts = parse_grant_name(&q.event.name);
                        // The whole row senses clicks. Its labels are not
                        // selectable, so they do not take the click.
                        let builder = egui::UiBuilder::new()
                            .id_salt(("queue-row", pos))
                            .sense(Sense::click());
                        let response = ui
                            .scope_builder(builder, |ui| {
                                let fill = if selected {
                                    ui.visuals().selection.bg_fill
                                } else {
                                    egui::Color32::TRANSPARENT
                                };
                                egui::Frame::new()
                                    .fill(fill)
                                    .corner_radius(6.0)
                                    .inner_margin(egui::Margin::symmetric(10, 7))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            // Source and age at the right,
                                            // the grant at the left.
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    plain(
                                                        ui,
                                                        RichText::new(format!(
                                                            "{} ago",
                                                            q.age_label(now)
                                                        ))
                                                        .color(palette.dim),
                                                    );
                                                    plain(
                                                        ui,
                                                        RichText::new(q.source_label())
                                                            .color(palette.dim),
                                                    );
                                                    ui.with_layout(
                                                        Layout::left_to_right(Align::Center),
                                                        |ui| match &parts {
                                                            Some(p) => {
                                                                plain(ui, bold(p.entitlement));
                                                                ui.add_space(6.0);
                                                                scope_icon(
                                                                    ui,
                                                                    p.scope,
                                                                    palette.dim,
                                                                );
                                                                plain(
                                                                    ui,
                                                                    RichText::new(p.scope_id)
                                                                        .color(palette.dim),
                                                                );
                                                            }
                                                            None => plain(
                                                                ui,
                                                                RichText::new(&q.event.name),
                                                            ),
                                                        },
                                                    );
                                                },
                                            );
                                        });
                                    });
                            })
                            .response;
                        if selected && scroll_to_cursor {
                            response.scroll_to_me(None);
                        }
                        if response.double_clicked() {
                            intent = Some(Intent::QueueOpenRow(pos));
                        } else if response.clicked() {
                            intent = Some(Intent::QueueCursor(pos));
                        }
                    }
                });
            out.extend(intent);

            ui.add_space(DIALOG_GAP + 4.0);
            // Buttons at the right with the main one last. Each shows its
            // key.
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if key_button(ui, "Review", "enter", ButtonKind::Primary, true).clicked() {
                        out.push(Intent::Queue(QueueAction::Open));
                    }
                    if key_button(ui, "Drop", "d", ButtonKind::Secondary, true).clicked() {
                        out.push(Intent::Queue(QueueAction::Drop));
                    }
                    if key_button(ui, "Back", "esc", ButtonKind::Ghost, true).clicked() {
                        out.push(Intent::Queue(QueueAction::Back));
                    }
                });
            });
        });
}

/// A label that neither wraps nor takes clicks or text selection.
fn plain(ui: &mut egui::Ui, text: RichText) {
    ui.add(
        egui::Label::new(text)
            .selectable(false)
            .wrap_mode(egui::TextWrapMode::Truncate),
    );
}
