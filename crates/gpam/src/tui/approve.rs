use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use gpam_core::app::approve::{
    ApproveAction, ApproveModal, ApproveOutcome, ApproveStep, Decision, Step, StepResult,
};
use gpam_core::app::format::format_age;
use gpam_core::backend::DynBackend;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::Term;
use super::keymap::edit_action;

/// Open the approve modal for a known grant name. Fetches details first. On
/// fetch error or terminal-state grant the modal opens in the result step.
pub async fn run(term: &mut Term, backend: DynBackend, name: String) -> Result<ApproveOutcome> {
    let mut modal = ApproveModal::load(&backend, &name).await;

    let mut events = EventStream::new();
    loop {
        term.draw(|f| render(f, &modal))?;

        let Some(Ok(event)) = events.next().await else {
            continue;
        };
        let Event::Key(key) = event else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let Some(action) = approve_action(modal.step(), key) else {
            continue;
        };
        match modal.apply(action) {
            ApproveStep::Continue => {}
            ApproveStep::Submit => modal.submit(&backend).await,
            ApproveStep::Close(outcome) => return Ok(outcome),
        }
    }
}

/// Map a key to an approve-modal action. Letters are commands in the confirm
/// and result steps but text while typing a reason, so the mapping depends
/// on the step.
fn approve_action(step: &Step, key: KeyEvent) -> Option<ApproveAction> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if matches!(key.code, KeyCode::Char('c')) && ctrl {
        return Some(ApproveAction::Quit);
    }
    match step {
        Step::Confirm => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(ApproveAction::Cancel),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                Some(ApproveAction::Choose(Decision::Approve))
            }
            KeyCode::Char('d') | KeyCode::Char('D') => Some(ApproveAction::Choose(Decision::Deny)),
            _ => None,
        },
        Step::Reason(_) => match key.code {
            KeyCode::Esc => Some(ApproveAction::Back),
            KeyCode::Enter => Some(ApproveAction::Submit),
            _ => edit_action(key).map(ApproveAction::Edit),
        },
        Step::Result(_) => match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(' ') => {
                Some(ApproveAction::Dismiss)
            }
            _ => None,
        },
    }
}

fn render(frame: &mut Frame, modal: &ApproveModal) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, outer[0]);
    render_body(frame, modal, outer[1]);
    render_footer(frame, modal, outer[2]);
}

fn render_header(frame: &mut Frame, area: Rect) {
    let p = Paragraph::new(Span::styled(
        " approve grant ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(p, area);
}

fn render_body(frame: &mut Frame, modal: &ApproveModal, parent: Rect) {
    let w = (parent.width as f32 * 0.8) as u16;
    let h = 24u16.min(parent.height.saturating_sub(2));
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let title = match modal.details() {
        Some(d) => format!(" grant: {} ", d.entitlement_short_name),
        None => " result ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(block.clone(), area);

    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    match modal.step() {
        Step::Confirm => render_confirm(frame, modal, inner, None),
        Step::Reason(decision) => render_confirm(frame, modal, inner, Some(*decision)),
        Step::Result(r) => render_result(frame, modal, inner, r),
    }
}

fn render_confirm(frame: &mut Frame, modal: &ApproveModal, area: Rect, decision: Option<Decision>) {
    let Some(d) = modal.details() else { return };
    let dim = Style::default().add_modifier(Modifier::DIM);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(field_line("requester", &d.requester, dim));
    lines.push(field_line(
        "scope",
        &format!("{}/{}", d.scope_type.plural(), d.scope_id),
        dim,
    ));
    lines.push(field_line("entitlement", &d.entitlement_short_name, dim));
    lines.push(field_line(
        "duration",
        &format_age(d.requested_duration_secs),
        dim,
    ));
    lines.push(field_line("state", &d.state.to_string(), dim));

    if d.role_bindings.is_empty() {
        lines.push(field_line("roles", "(none listed)", dim));
    } else {
        lines.push(Line::from(vec![Span::styled("  roles:      ", dim)]));
        for rb in &d.role_bindings {
            let suffix = match &rb.condition {
                Some(c) => format!("{}  ({})", rb.role, c),
                None => rb.role.clone(),
            };
            lines.push(Line::from(format!("    - {suffix}")));
        }
    }

    let just = d.justification.as_deref().unwrap_or("(none)");
    lines.push(Line::from(vec![Span::styled("  justify:    ", dim)]));
    lines.push(Line::from(format!("    {just}")));

    // The full resource name, to match against the request you were told
    // about.
    lines.push(Line::from(vec![Span::styled("  grant:      ", dim)]));
    lines.push(Line::from(format!("    {}", d.name)));

    lines.push(Line::from(""));
    match decision {
        None => {
            lines.push(Line::from(Span::styled(
                "[a] approve   [d] deny   [esc] cancel",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        Some(dec) => {
            let prefix = format!("> reason ({}): ", dec.label());
            let prefix_w = prefix.chars().count() as u16;
            lines.push(Line::from(vec![
                Span::styled(
                    prefix.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(modal.reason().as_str().to_string()),
            ]));
            lines.push(Line::from(Span::styled(
                "enter to confirm  |  esc to go back",
                dim,
            )));

            let cursor_x = area.x + prefix_w + modal.reason().display_cursor();
            let cursor_y = area.y + (lines.len() as u16).saturating_sub(2);
            if cursor_y < area.y + area.height {
                frame.set_cursor_position((cursor_x, cursor_y));
            }
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn field_line(label: &str, value: &str, dim: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<11}"), dim),
        Span::raw(value.to_string()),
    ])
}

fn render_result(frame: &mut Frame, modal: &ApproveModal, area: Rect, r: &StepResult) {
    let mut lines: Vec<Line> = Vec::new();
    let entitlement = modal
        .details()
        .map(|d| d.entitlement_short_name.clone())
        .unwrap_or_default();
    match r {
        StepResult::Ok(decision) => {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("[{}]  ", decision.label()),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("grant {entitlement} {}.", decision.past_tense())),
            ]));
        }
        StepResult::AlreadyHandled => {
            lines.push(Line::from(vec![Span::styled(
                "[already handled]  ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(format!(
                "grant {entitlement} was already approved or denied by someone else."
            )));
        }
        StepResult::Err(msg) => {
            lines.push(Line::from(vec![Span::styled(
                "[err]  ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(Span::styled(
                msg.clone(),
                Style::default().fg(Color::Red),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press enter / esc / q to dismiss",
        Style::default().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_footer(frame: &mut Frame, modal: &ApproveModal, area: Rect) {
    let keys = match modal.step() {
        Step::Confirm => "a approve | d deny | esc cancel | ctrl-c quit",
        Step::Reason(_) => "enter confirm | esc back | ctrl-c quit",
        Step::Result(_) => "enter / esc / q dismiss | ctrl-c quit",
    };
    let p = Paragraph::new(Span::styled(
        keys,
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(p, area);
}
