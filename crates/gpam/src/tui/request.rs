use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use gpam_core::app::request::{
    self, Field, RequestAction, RequestFlow, RequestModal, RequestResult, RequestStep, ResultState,
};
use gpam_core::app::text_input::TextInput;
use gpam_core::cache::EntitlementRow;
use gpam_core::poller::Poller;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::Term;
use super::keymap::edit_action;

/// Walk the user through duration + justification prompts for each selected
/// entitlement, submitting each one that's confirmed. Returns the grant
/// resource names that were created.
pub async fn run(
    term: &mut Term,
    poller: &Poller,
    selected: Vec<EntitlementRow>,
) -> Result<Vec<String>> {
    let mut events = EventStream::new();
    let mut flow = RequestFlow::new(selected);

    while flow.current().is_some() {
        term.draw(|f| render(f, &flow))?;
        let Some(Ok(event)) = events.next().await else {
            continue;
        };
        let Event::Key(key) = event else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let Some(action) = request_action(key) else {
            continue;
        };
        match flow.apply(action) {
            RequestStep::Continue => {}
            RequestStep::Submit(submission) => {
                let result = request::submit(poller, submission).await;
                flow.complete(result);
            }
            RequestStep::Abort => return Ok(flow.grant_names()),
        }
    }

    // If any request errored, pause on a summary so the user actually sees it
    // before we move on.
    if flow.has_errors() {
        show_summary(term, &mut events, &flow).await?;
    }

    Ok(flow.grant_names())
}

async fn show_summary(term: &mut Term, events: &mut EventStream, flow: &RequestFlow) -> Result<()> {
    loop {
        term.draw(|f| render_summary(f, flow))?;
        let Some(Ok(event)) = events.next().await else {
            continue;
        };
        let Event::Key(key) = event else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ') | KeyCode::Char('q') => {
                return Ok(());
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            _ => {}
        }
    }
}

fn request_action(key: KeyEvent) -> Option<RequestAction> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => Some(RequestAction::Abort),
        KeyCode::Esc => Some(RequestAction::Skip),
        KeyCode::Enter => Some(RequestAction::Confirm),
        KeyCode::Tab => Some(RequestAction::SwapField),
        _ => edit_action(key).map(RequestAction::Edit),
    }
}

fn render(frame: &mut Frame, flow: &RequestFlow) {
    let Some(modal) = flow.current() else { return };
    let area = frame.area();

    // Center the modal: 60% width by 14 lines.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, outer[0], flow.position(), flow.total());
    render_results(frame, outer[1], flow.results());
    render_modal(frame, modal, outer[1]);
    render_footer(frame, outer[2]);
}

fn render_header(frame: &mut Frame, area: Rect, i: usize, total: usize) {
    let line = format!(" requesting access -- {} of {} ", i + 1, total);
    let p = Paragraph::new(Span::styled(
        line,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(p, area);
}

fn render_results(frame: &mut Frame, area: Rect, results: &[RequestResult]) {
    let lines: Vec<Line> = results
        .iter()
        .map(|r| match &r.state {
            ResultState::Ok(name) => Line::from(vec![
                Span::styled(
                    "[ok]   ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{}  ->  {}", r.short_name, name)),
            ]),
            ResultState::Err(msg) => Line::from(vec![
                Span::styled(
                    "[err]  ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{}  {}", r.short_name, msg)),
            ]),
            ResultState::Skipped => Line::from(vec![
                Span::styled("[skip] ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(format!("{}  (skipped)", r.short_name)),
            ]),
        })
        .collect();
    let block = Block::default().borders(Borders::ALL).title(" results ");
    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, area);
}

fn render_modal(frame: &mut Frame, modal: &RequestModal, parent: Rect) {
    let w = (parent.width as f32 * 0.7) as u16;
    let h = 16u16.min(parent.height.saturating_sub(2));
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", modal.entitlement().short_name));
    frame.render_widget(block.clone(), area);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(2)
        .vertical_margin(1)
        .constraints([
            Constraint::Length(1), // project info
            Constraint::Length(1), // max info
            Constraint::Length(1), // spacer
            Constraint::Length(1), // duration field
            Constraint::Length(1), // spacer
            Constraint::Min(3),    // justification field (wraps)
            Constraint::Length(1), // error
        ])
        .split(area);

    let info_style = Style::default().add_modifier(Modifier::DIM);
    frame.render_widget(
        Paragraph::new(modal.scope_line()).style(info_style),
        inner[0],
    );
    frame.render_widget(
        Paragraph::new(modal.limits_line()).style(info_style),
        inner[1],
    );

    render_field(
        frame,
        inner[3],
        "duration",
        modal.duration(),
        modal.field() == Field::Duration,
    );

    if modal.entitlement().justification_required {
        render_field_wrapped(
            frame,
            inner[5],
            "justify ",
            modal.justification(),
            modal.field() == Field::Justification,
        );
    } else {
        frame.render_widget(
            Paragraph::new("justification: not required").style(info_style),
            inner[5],
        );
    }

    if let Some(err) = modal.error() {
        frame.render_widget(
            Paragraph::new(err)
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true }),
            inner[6],
        );
    }
}

fn render_field(frame: &mut Frame, area: Rect, label: &str, input: &TextInput, focused: bool) {
    let arrow = if focused { "> " } else { "  " };
    let label_style = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let line = Line::from(vec![
        Span::styled(arrow, label_style),
        Span::styled(format!("{label}: "), label_style),
        Span::raw(input.as_str()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    if focused {
        let prefix = (arrow.len() + label.len() + 2) as u16;
        frame.set_cursor_position((area.x + prefix + input.display_cursor(), area.y));
    }
}

/// Render a text input that wraps onto multiple lines when its content exceeds
/// the available width. Character-wraps (not word-wraps) so the cursor position
/// is exact.
fn render_field_wrapped(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    input: &TextInput,
    focused: bool,
) {
    let arrow = if focused { "> " } else { "  " };
    let label_style = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let prefix = format!("{arrow}{label}: ");
    let prefix_w = prefix.chars().count() as u16;
    let text_w = area.width.saturating_sub(prefix_w).max(1) as usize;

    let chars: Vec<char> = input.as_str().chars().collect();
    let mut lines: Vec<Line> = Vec::new();
    if chars.is_empty() {
        lines.push(Line::from(Span::styled(prefix.clone(), label_style)));
    } else {
        for (i, chunk) in chars.chunks(text_w).enumerate() {
            let s: String = chunk.iter().collect();
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix.clone(), label_style),
                    Span::raw(s),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(prefix_w as usize)),
                    Span::raw(s),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), area);

    if focused {
        let cursor_chars = input.display_cursor() as usize;
        let row_idx = cursor_chars / text_w;
        let col_idx = cursor_chars % text_w;
        let row = (row_idx as u16).min(area.height.saturating_sub(1));
        frame.set_cursor_position((area.x + prefix_w + col_idx as u16, area.y + row));
    }
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let keys = "enter confirm | tab swap field | esc skip this | ctrl-c abort all";
    let p = Paragraph::new(Span::styled(
        keys,
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(p, area);
}

fn render_summary(frame: &mut Frame, flow: &RequestFlow) {
    let results = flow.results();
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    let (ok, err, skip) = flow.counts();
    let header = format!(" request summary -- {ok} ok, {err} error, {skip} skipped ");
    frame.render_widget(
        Paragraph::new(Span::styled(
            header,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        outer[0],
    );

    let lines: Vec<Line> = results
        .iter()
        .flat_map(|r| match &r.state {
            ResultState::Ok(name) => vec![Line::from(vec![
                Span::styled(
                    "[ok]   ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{}  ->  {}", r.short_name, name)),
            ])],
            ResultState::Err(msg) => vec![
                Line::from(vec![
                    Span::styled(
                        "[err]  ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(r.short_name.clone()),
                ]),
                Line::from(vec![
                    Span::raw("       "),
                    Span::styled(msg.clone(), Style::default().fg(Color::Red)),
                ]),
            ],
            ResultState::Skipped => vec![Line::from(vec![
                Span::styled("[skip] ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(format!("{}  (skipped)", r.short_name)),
            ])],
        })
        .collect();
    let block = Block::default().borders(Borders::ALL).title(" results ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        outer[1],
    );

    let keys = "press enter / esc / q to continue";
    frame.render_widget(
        Paragraph::new(Span::styled(
            keys,
            Style::default().add_modifier(Modifier::DIM),
        )),
        outer[2],
    );
}
