use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::backend::DynBackend;
use crate::gcp::grants::GrantDetails;

use super::Term;
use super::widgets::{TextInput, format_age};

/// What the modal decided to do on exit. Caller (queue or CLI) uses this to
/// decide whether to drop the queued row and whether to keep the TUI alive.
pub enum ApproveOutcome {
    Approved,
    Denied,
    /// Grant was already in a terminal state — nothing to do here.
    AlreadyHandled,
    /// User backed out (Esc) without acting.
    Canceled,
    /// Submission failed for a reason other than "already handled".
    Failed,
    /// User pressed Ctrl-C — tear down the whole TUI.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Approve,
    Deny,
}

impl Decision {
    fn label(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Deny => "deny",
        }
    }
}

enum Step {
    /// Grant fetched and actionable. User picks approve / deny / cancel.
    Confirm,
    /// Decision picked, collecting an optional reason.
    Reason(Decision),
    /// Terminal outcome — submit succeeded, already handled, or fetch/submit
    /// errored. User dismisses with Enter / Esc / q.
    Result(StepResult),
}

enum StepResult {
    Ok(Decision),
    AlreadyHandled,
    Err(String),
}

struct Modal {
    reason: TextInput,
    step: Step,
    /// `None` only when the initial fetch failed; in that case the modal
    /// lands directly in `Step::Result(Err)`.
    details: Option<GrantDetails>,
}

/// Open the approve modal for a known grant name. Fetches details first; on
/// fetch error or terminal-state grant the modal opens in the result step.
pub async fn run(term: &mut Term, backend: DynBackend, name: String) -> Result<ApproveOutcome> {
    let mut modal = match backend.get_grant_details(&name).await {
        Ok(d) if d.state.is_terminal() => Modal {
            reason: TextInput::new(),
            step: Step::Result(StepResult::AlreadyHandled),
            details: Some(d),
        },
        Ok(d) => Modal {
            reason: TextInput::new(),
            step: Step::Confirm,
            details: Some(d),
        },
        Err(e) => Modal {
            reason: TextInput::new(),
            step: Step::Result(StepResult::Err(format!("fetch failed: {e:#}"))),
            details: None,
        },
    };

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
        if let Some(outcome) = handle_key(&mut modal, &backend, key).await {
            return Ok(outcome);
        }
    }
}

/// Returns `Some(outcome)` when the modal should close.
async fn handle_key(
    modal: &mut Modal,
    backend: &DynBackend,
    key: KeyEvent,
) -> Option<ApproveOutcome> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if matches!(key.code, KeyCode::Char('c')) && ctrl {
        return Some(ApproveOutcome::Quit);
    }
    match &modal.step {
        Step::Confirm => handle_confirm(modal, key),
        Step::Reason(_) => handle_reason(modal, backend, key).await,
        Step::Result(_) => handle_dismiss(modal, key),
    }
}

fn handle_confirm(modal: &mut Modal, key: KeyEvent) -> Option<ApproveOutcome> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => Some(ApproveOutcome::Canceled),
        KeyCode::Char('a') | KeyCode::Char('A') => {
            modal.step = Step::Reason(Decision::Approve);
            None
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            modal.step = Step::Reason(Decision::Deny);
            None
        }
        _ => None,
    }
}

async fn handle_reason(
    modal: &mut Modal,
    backend: &DynBackend,
    key: KeyEvent,
) -> Option<ApproveOutcome> {
    let Step::Reason(decision) = modal.step else {
        return None;
    };
    match key.code {
        KeyCode::Esc => {
            modal.reason.clear();
            modal.step = Step::Confirm;
            None
        }
        KeyCode::Enter => {
            submit_decision(modal, backend, decision).await;
            None
        }
        KeyCode::Backspace => {
            modal.reason.backspace();
            None
        }
        KeyCode::Left => {
            modal.reason.move_left();
            None
        }
        KeyCode::Right => {
            modal.reason.move_right();
            None
        }
        KeyCode::Home => {
            modal.reason.move_home();
            None
        }
        KeyCode::End => {
            modal.reason.move_end();
            None
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            modal.reason.insert(c);
            None
        }
        _ => None,
    }
}

fn handle_dismiss(modal: &Modal, key: KeyEvent) -> Option<ApproveOutcome> {
    if !matches!(
        key.code,
        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(' ')
    ) {
        return None;
    }
    let Step::Result(result) = &modal.step else {
        return None;
    };
    Some(match result {
        StepResult::Ok(Decision::Approve) => ApproveOutcome::Approved,
        StepResult::Ok(Decision::Deny) => ApproveOutcome::Denied,
        StepResult::AlreadyHandled => ApproveOutcome::AlreadyHandled,
        StepResult::Err(_) => ApproveOutcome::Failed,
    })
}

async fn submit_decision(modal: &mut Modal, backend: &DynBackend, decision: Decision) {
    let Some(details) = &modal.details else {
        modal.step = Step::Result(StepResult::Err("no grant loaded".into()));
        return;
    };
    let reason = {
        let r = modal.reason.as_str().trim();
        if r.is_empty() { None } else { Some(r) }
    };
    let result = match decision {
        Decision::Approve => backend.approve_grant(&details.name, reason).await,
        Decision::Deny => backend.deny_grant(&details.name, reason).await,
    };
    modal.step = match result {
        Ok(()) => Step::Result(StepResult::Ok(decision)),
        Err(e) => {
            // Refetch to distinguish "someone else handled it" from a real
            // failure. Race-with-another-approver is a legitimate outcome,
            // not a retry-worthy error.
            let already_handled = backend
                .get_grant_details(&details.name)
                .await
                .map(|d| d.state.is_terminal())
                .unwrap_or(false);
            if already_handled {
                Step::Result(StepResult::AlreadyHandled)
            } else {
                Step::Result(StepResult::Err(format!("{e:#}")))
            }
        }
    };
}

fn render(frame: &mut Frame, modal: &Modal) {
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

fn render_body(frame: &mut Frame, modal: &Modal, parent: Rect) {
    let w = (parent.width as f32 * 0.8) as u16;
    let h = 20u16.min(parent.height.saturating_sub(2));
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let title = match &modal.details {
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
    match &modal.step {
        Step::Confirm => render_confirm(frame, modal, inner, None),
        Step::Reason(decision) => render_confirm(frame, modal, inner, Some(*decision)),
        Step::Result(r) => render_result(frame, modal, inner, r),
    }
}

fn render_confirm(frame: &mut Frame, modal: &Modal, area: Rect, decision: Option<Decision>) {
    let Some(d) = &modal.details else { return };
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
                Span::raw(modal.reason.as_str().to_string()),
            ]));
            lines.push(Line::from(Span::styled(
                "enter to confirm  |  esc to go back",
                dim,
            )));

            let cursor_x = area.x + prefix_w + modal.reason.display_cursor();
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

fn render_result(frame: &mut Frame, modal: &Modal, area: Rect, r: &StepResult) {
    let mut lines: Vec<Line> = Vec::new();
    let entitlement = modal
        .details
        .as_ref()
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
                Span::raw(format!("grant {entitlement} {}d.", decision.label())),
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

fn render_footer(frame: &mut Frame, modal: &Modal, area: Rect) {
    let keys = match &modal.step {
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
