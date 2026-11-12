//! Approval queue modal: grants forwarded over the local socket and waiting
//! for review.

use std::time::SystemTime;

use eframe::egui::{self, Align, Layout, Modal, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use gpam_core::app::approval_queue::{QueueAction, QueueView};

use crate::app::Intent;
use crate::style::{Palette, bold, bold_family, row_height};

pub fn show(ctx: &egui::Context, view: &QueueView, scroll_to_cursor: bool, out: &mut Vec<Intent>) {
    let max_height = (ctx.content_rect().height() * 0.6).max(160.0);
    Modal::new(egui::Id::unique("gpam-queue")).show(ctx, |ui| {
        ui.set_width(720.0);
        let palette = Palette::current(ui);
        let rows = view.rows();
        ui.label(
            RichText::new(format!("pending approvals ({})", rows.len()))
                .color(palette.accent)
                .family(bold_family()),
        );
        ui.separator();

        let now = SystemTime::now();
        let row_h = row_height(ui);
        let mut intent = None;
        let mut table = TableBuilder::new(ui)
            .id_salt("queue")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(240.0).at_least(80.0).clip(true))
            .column(Column::initial(220.0).at_least(80.0).clip(true))
            .column(Column::initial(110.0).at_least(50.0).clip(true))
            .column(Column::remainder().at_least(50.0))
            .min_scrolled_height(0.0)
            .max_scroll_height(max_height);
        if scroll_to_cursor {
            table = table.scroll_to_row(view.cursor(), None);
        }
        table
            .header(row_h, |mut header| {
                for title in ["entitlement", "scope", "source", "age"] {
                    header.col(|ui| {
                        ui.label(bold(title));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let pos = row.index();
                    let Some(q) = rows.get(pos) else {
                        return;
                    };
                    row.set_selected(pos == view.cursor());
                    let (ent, scope_short, scope_id) = q.display_parts();
                    row.col(|ui| {
                        cell(ui, RichText::new(ent));
                    });
                    row.col(|ui| {
                        cell(
                            ui,
                            RichText::new(format!("{scope_short}:{scope_id}")).color(palette.dim),
                        );
                    });
                    row.col(|ui| {
                        cell(
                            ui,
                            RichText::new(format!("({})", q.source_label())).color(palette.dim),
                        );
                    });
                    row.col(|ui| {
                        cell(
                            ui,
                            RichText::new(format!("{} ago", q.age_label(now))).color(palette.dim),
                        );
                    });
                    let response = row.response();
                    if response.double_clicked() {
                        intent = Some(Intent::QueueOpenRow(pos));
                    } else if response.clicked() {
                        intent = Some(Intent::QueueCursor(pos));
                    }
                });
            });
        out.extend(intent);

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("review").clicked() {
                out.push(Intent::Queue(QueueAction::Open));
            }
            if ui.button("drop").clicked() {
                out.push(Intent::Queue(QueueAction::Drop));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("back").clicked() {
                    out.push(Intent::Queue(QueueAction::Back));
                }
            });
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("j/k navigate | enter review | d/x drop | esc/q back")
                .small()
                .color(palette.dim),
        );
    });
}

fn cell(ui: &mut egui::Ui, text: RichText) {
    ui.add(
        egui::Label::new(text)
            .selectable(false)
            .wrap_mode(egui::TextWrapMode::Truncate),
    );
}
