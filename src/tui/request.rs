use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::cache::{EntitlementRow, GrantRow, now_unix};
use crate::gcp::grants::{GrantState, MIN_DURATION_SECS, parse_duration};
use crate::poller::{GrantUpdate, PollMode, Poller};

use super::Term;
use super::widgets::{TextInput, format_age};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Duration,
    Justification,
}

struct Modal {
    entitlement: EntitlementRow,
    duration: TextInput,
    justification: TextInput,
    field: Field,
    error: Option<String>,
}

impl Modal {
    fn new(entitlement: EntitlementRow) -> Self {
        Self {
            duration: TextInput::with_text("1h"),
            justification: TextInput::new(),
            field: Field::Duration,
            error: None,
            entitlement,
        }
    }

    fn validate_duration(&self) -> Result<i64, String> {
        let s = self.duration.as_str();
        let secs = parse_duration(s)
            .ok_or_else(|| format!("invalid duration '{s}'; try 1h, 30m, 5400s, 3600"))?;
        if secs < MIN_DURATION_SECS {
            return Err(format!("below minimum {}", format_age(MIN_DURATION_SECS)));
        }
        if let Some(max) = self.entitlement.max_request_duration_secs
            && max > 0
            && secs > max
        {
            return Err(format!("exceeds max {}", format_age(max)));
        }
        Ok(secs)
    }
}

enum StepOutcome {
    /// Submit this entitlement with these values.
    Submit {
        duration_secs: i64,
        justification: Option<String>,
    },
    /// Skip this entitlement, move on to the next.
    Skip,
    /// Abort the whole flow (user pressed Ctrl-C or quit from the first field).
    Abort,
    /// No outcome yet, stay in the modal.
    Continue,
}

/// Walk the user through duration + justification prompts for each selected
/// entitlement, calling `create_grant` for each one that's confirmed.
/// On success the grant is persisted to the cache and a background poller is
/// spawned, with state changes forwarded to `grant_tx` so the browse strip
/// can render them. Returns the grant resource names that were created.
pub async fn run(
    term: &mut Term,
    poller: Poller,
    selected: Vec<EntitlementRow>,
) -> Result<Vec<String>> {
    let mut events = EventStream::new();
    let mut results: Vec<RequestResult> = Vec::new();
    let total = selected.len();

    for (i, ent) in selected.into_iter().enumerate() {
        let mut modal = Modal::new(ent.clone());

        let outcome = loop {
            term.draw(|f| render(f, &modal, &results, i, total))?;
            let Some(Ok(event)) = events.next().await else {
                continue;
            };
            let Event::Key(key) = event else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle_key(&mut modal, key) {
                StepOutcome::Continue => continue,
                other => break other,
            }
        };

        match outcome {
            StepOutcome::Submit {
                duration_secs,
                justification,
            } => {
                let result = match poller
                    .backend
                    .create_grant(&ent.name, duration_secs, justification.as_deref())
                    .await
                {
                    Ok(name) => {
                        let now = now_unix();
                        let short_id = name.rsplit('/').next().unwrap_or("").to_string();
                        let grant_row = GrantRow {
                            name: name.clone(),
                            entitlement_name: ent.name.clone(),
                            entitlement_short_name: ent.short_name.clone(),
                            scope_type: ent.scope_type,
                            scope_id: ent.scope_id.clone(),
                            scope_display_name: ent.scope_display_name.clone(),
                            short_id,
                            state: GrantState::Requested,
                            requested_duration_secs: duration_secs,
                            justification: justification.clone(),
                            created_at: now,
                            activated_at: None,
                            expires_at: None,
                            last_polled_at: None,
                            raw_json: None,
                        };
                        {
                            let cache = poller.cache.lock().await;
                            let _ = cache.bump_used(&ent.name, now);
                            let _ = cache.upsert_grant(&grant_row);
                        }
                        // Surface the new row to the strip immediately, then
                        // spawn a poller; the first poll observation will
                        // overwrite this provisional state via the same channel.
                        let _ = poller
                            .tx
                            .send(GrantUpdate {
                                name: name.clone(),
                                error: None,
                            })
                            .await;
                        tokio::spawn(poller.clone().poll_grant(
                            name.clone(),
                            duration_secs,
                            grant_row.state.clone(),
                            PollMode::Track,
                        ));
                        RequestResult::ok(ent.short_name.clone(), name)
                    }
                    Err(e) => RequestResult::err(ent.short_name.clone(), format!("{e:#}")),
                };
                results.push(result);
            }
            StepOutcome::Skip => {
                results.push(RequestResult::skipped(ent.short_name.clone()));
            }
            StepOutcome::Abort => return Ok(grant_names(&results)),
            StepOutcome::Continue => unreachable!("the loop only exits on a non-Continue outcome"),
        }
    }

    // If any request errored, pause on a summary so the user actually sees it
    // before we move on to the status screen (or exit if nothing succeeded).
    if results
        .iter()
        .any(|r| matches!(r.state, ResultState::Err(_)))
    {
        show_summary(term, &mut events, &results).await?;
    }

    Ok(grant_names(&results))
}

async fn show_summary(
    term: &mut Term,
    events: &mut EventStream,
    results: &[RequestResult],
) -> Result<()> {
    loop {
        term.draw(|f| render_summary(f, results))?;
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

fn handle_key(modal: &mut Modal, key: KeyEvent) -> StepOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Global keys.
    match key.code {
        KeyCode::Char('c') if ctrl => return StepOutcome::Abort,
        KeyCode::Esc => return StepOutcome::Skip,
        _ => {}
    }

    let need_justification = modal.entitlement.justification_required;

    match (modal.field, key.code) {
        // Duration field.
        (Field::Duration, KeyCode::Enter) => match modal.validate_duration() {
            Ok(_) => {
                modal.error = None;
                if need_justification {
                    modal.field = Field::Justification;
                } else {
                    return submit(modal);
                }
            }
            Err(e) => modal.error = Some(e),
        },
        (Field::Duration, KeyCode::Tab) => {
            modal.field = Field::Justification;
        }
        (Field::Duration, KeyCode::Backspace) => modal.duration.backspace(),
        (Field::Duration, KeyCode::Left) => modal.duration.move_left(),
        (Field::Duration, KeyCode::Right) => modal.duration.move_right(),
        (Field::Duration, KeyCode::Home) => modal.duration.move_home(),
        (Field::Duration, KeyCode::End) => modal.duration.move_end(),
        (Field::Duration, KeyCode::Char(c)) if !ctrl => modal.duration.insert(c),

        // Justification field.
        (Field::Justification, KeyCode::Enter) => {
            if need_justification && modal.justification.as_str().trim().is_empty() {
                modal.error = Some("justification cannot be empty".into());
            } else {
                return submit(modal);
            }
        }
        (Field::Justification, KeyCode::Tab) => modal.field = Field::Duration,
        (Field::Justification, KeyCode::Backspace) => modal.justification.backspace(),
        (Field::Justification, KeyCode::Left) => modal.justification.move_left(),
        (Field::Justification, KeyCode::Right) => modal.justification.move_right(),
        (Field::Justification, KeyCode::Home) => modal.justification.move_home(),
        (Field::Justification, KeyCode::End) => modal.justification.move_end(),
        (Field::Justification, KeyCode::Char(c)) if !ctrl => modal.justification.insert(c),

        _ => {}
    }
    StepOutcome::Continue
}

fn submit(modal: &Modal) -> StepOutcome {
    match modal.validate_duration() {
        Ok(secs) => StepOutcome::Submit {
            duration_secs: secs,
            justification: if modal.entitlement.justification_required {
                Some(modal.justification.as_str().to_string())
            } else {
                None
            },
        },
        Err(_) => StepOutcome::Continue,
    }
}

#[derive(Debug, Clone)]
struct RequestResult {
    short_name: String,
    state: ResultState,
}

#[derive(Debug, Clone)]
enum ResultState {
    Ok(String),
    Err(String),
    Skipped,
}

impl RequestResult {
    fn ok(short: String, grant: String) -> Self {
        Self {
            short_name: short,
            state: ResultState::Ok(grant),
        }
    }
    fn err(short: String, msg: String) -> Self {
        Self {
            short_name: short,
            state: ResultState::Err(msg),
        }
    }
    fn skipped(short: String) -> Self {
        Self {
            short_name: short,
            state: ResultState::Skipped,
        }
    }
}

fn grant_names(results: &[RequestResult]) -> Vec<String> {
    results
        .iter()
        .filter_map(|r| match &r.state {
            ResultState::Ok(name) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn render(frame: &mut Frame, modal: &Modal, results: &[RequestResult], i: usize, total: usize) {
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

    render_header(frame, outer[0], i, total);
    render_results(frame, outer[1], results);
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

fn render_modal(frame: &mut Frame, modal: &Modal, parent: Rect) {
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
        .title(format!(" {} ", modal.entitlement.short_name));
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
    let scope_line = match &modal.entitlement.scope_display_name {
        Some(name) if !name.is_empty() => format!(
            "scope: {}/{} ({})",
            modal.entitlement.scope_type.plural(),
            modal.entitlement.scope_id,
            name,
        ),
        _ => format!(
            "scope: {}/{}",
            modal.entitlement.scope_type.plural(),
            modal.entitlement.scope_id,
        ),
    };
    frame.render_widget(Paragraph::new(scope_line).style(info_style), inner[0]);
    let max_info = match modal.entitlement.max_request_duration_secs {
        Some(s) => format!(
            "min {} | max {}",
            format_age(MIN_DURATION_SECS),
            format_age(s)
        ),
        None => format!("min {}", format_age(MIN_DURATION_SECS)),
    };
    frame.render_widget(Paragraph::new(max_info).style(info_style), inner[1]);

    render_field(
        frame,
        inner[3],
        "duration",
        &modal.duration,
        modal.field == Field::Duration,
    );

    if modal.entitlement.justification_required {
        render_field_wrapped(
            frame,
            inner[5],
            "justify ",
            &modal.justification,
            modal.field == Field::Justification,
        );
    } else {
        frame.render_widget(
            Paragraph::new("justification: not required").style(info_style),
            inner[5],
        );
    }

    if let Some(err) = &modal.error {
        frame.render_widget(
            Paragraph::new(err.as_str())
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

fn render_summary(frame: &mut Frame, results: &[RequestResult]) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    let ok = results
        .iter()
        .filter(|r| matches!(r.state, ResultState::Ok(_)))
        .count();
    let err = results
        .iter()
        .filter(|r| matches!(r.state, ResultState::Err(_)))
        .count();
    let skip = results
        .iter()
        .filter(|r| matches!(r.state, ResultState::Skipped))
        .count();
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
