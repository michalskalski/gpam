use std::time::SystemTime;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use gpam_core::app::approval_queue::{Queue, QueueAction, QueueStep, QueueView};
use gpam_core::app::approve::ApproveOutcome;
use gpam_core::app::format::truncate;
use gpam_core::backend::DynBackend;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::Term;
use super::approve;

/// What the queue screen decided to do on exit. `Back` re-enters browse.
/// `Quit` asks the outer loop to tear down the whole TUI (Ctrl-C).
pub enum QueueExit {
    Back,
    Quit,
}

/// Enter the queue screen. Returns when the user presses Esc/q, when Ctrl-C
/// is pressed (full TUI quit), or when the queue empties (e.g. all rows
/// dropped).
pub async fn run(term: &mut Term, backend: DynBackend, queue: Queue) -> Result<QueueExit> {
    let mut events = EventStream::new();
    let mut view = QueueView::new(queue);
    // Periodic redraw so age timestamps tick and newly-arrived events appear
    // without requiring a keystroke.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if !view.sync() {
            return Ok(QueueExit::Back);
        }

        term.draw(|f| render(f, &view))?;

        tokio::select! {
            ev = events.next() => {
                let Some(Ok(ev)) = ev else { continue };
                let Event::Key(key) = ev else { continue };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let Some(action) = decide(key) else { continue };
                match view.apply(action) {
                    QueueStep::Continue => {}
                    QueueStep::Open(name) => {
                        match approve::run(term, backend.clone(), name).await? {
                            ApproveOutcome::Quit => return Ok(QueueExit::Quit),
                            outcome => view.finish_review(&outcome),
                        }
                    }
                    QueueStep::Back => return Ok(QueueExit::Back),
                    QueueStep::Quit => return Ok(QueueExit::Quit),
                }
            }
            _ = tick.tick() => {}
        }
    }
}

fn decide(key: KeyEvent) -> Option<QueueAction> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if matches!(key.code, KeyCode::Char('c')) && ctrl {
        return Some(QueueAction::Quit);
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => Some(QueueAction::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(QueueAction::Up),
        KeyCode::Char('g') => Some(QueueAction::Top),
        KeyCode::Char('G') => Some(QueueAction::Bottom),
        KeyCode::Char('d') | KeyCode::Char('x') => Some(QueueAction::Drop),
        KeyCode::Enter => Some(QueueAction::Open),
        KeyCode::Esc | KeyCode::Char('q') => Some(QueueAction::Back),
        _ => None,
    }
}

fn render(frame: &mut Frame, view: &QueueView) {
    let rows = view.rows();
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Paragraph::new(Span::styled(
        format!(" pending approvals ({}) ", rows.len()),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(header, chunks[0]);

    let now = SystemTime::now();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|q| {
            let (ent, scope_short, scope_id) = q.display_parts();
            let age = q.age_label(now);
            let source = q.source_label();
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<24}  ", truncate(ent, 24))),
                Span::styled(format!("{scope_short}:{scope_id:<16}  "), dim),
                Span::styled(format!("({source})  "), dim),
                Span::styled(format!("{age} ago"), dim),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(view.cursor().min(rows.len().saturating_sub(1))));
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" queue "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, chunks[1], &mut state);

    let footer = Paragraph::new(Span::styled(
        "j/k navigate | enter review | d/x drop | esc/q back | ctrl-c quit",
        dim,
    ));
    frame.render_widget(footer, chunks[2]);
}
