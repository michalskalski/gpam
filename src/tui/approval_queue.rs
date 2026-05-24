use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::ipc::{self, ApprovalEvent};

use super::Term;
use super::widgets::format_age;

/// What the queue screen decided to do on exit. `Back` re-enters browse;
/// `Quit` asks the outer loop to tear down the whole TUI (Ctrl-C).
pub enum QueueExit {
    Back,
    Quit,
}

/// Wraps an inbound event with the time we observed it, so the list can
/// display age since arrival.
#[derive(Debug, Clone)]
pub struct QueuedEvent {
    pub event: ApprovalEvent,
    pub received_at: SystemTime,
}

/// Shared between the socket drainer (writer), the browse screen (reads len
/// for the badge), and this screen (reads + removes). Lock is short-lived so
/// `std::sync::Mutex` keeps things simple
pub type Queue = Arc<Mutex<VecDeque<QueuedEvent>>>;

pub fn new_queue() -> Queue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Enter the queue screen. Returns when the user presses Esc/q, when Ctrl-C
/// is pressed (full TUI quit), or when the queue empties (e.g. all rows
/// dropped).
pub async fn run(term: &mut Term, queue: Queue) -> Result<QueueExit> {
    let mut events = EventStream::new();
    let mut cursor = 0usize;
    // Periodic redraw so age timestamps tick and newly-arrived events appear
    // without requiring a keystroke.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let snapshot: Vec<QueuedEvent> = queue.lock().unwrap().iter().cloned().collect();
        if snapshot.is_empty() {
            return Ok(QueueExit::Back);
        }
        if cursor >= snapshot.len() {
            cursor = snapshot.len() - 1;
        }

        term.draw(|f| render(f, &snapshot, cursor))?;

        tokio::select! {
            ev = events.next() => {
                let Some(Ok(ev)) = ev else { continue };
                let Event::Key(key) = ev else { continue };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match decide(key) {
                    Some(QueueAction::Up) => cursor = cursor.saturating_sub(1),
                    Some(QueueAction::Down) => {
                        if cursor + 1 < snapshot.len() {
                            cursor += 1;
                        }
                    }
                    Some(QueueAction::Top) => cursor = 0,
                    Some(QueueAction::Bottom) => cursor = snapshot.len().saturating_sub(1),
                    Some(QueueAction::Drop) => {
                        let mut q = queue.lock().unwrap();
                        if cursor < q.len() {
                            q.remove(cursor);
                        }
                    }
                    Some(QueueAction::Back) => return Ok(QueueExit::Back),
                    Some(QueueAction::Quit) => return Ok(QueueExit::Quit),
                    None => {}
                }
            }
            _ = tick.tick() => {}
        }
    }
}

enum QueueAction {
    Up,
    Down,
    Top,
    Bottom,
    Drop,
    Back,
    Quit,
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
        KeyCode::Esc | KeyCode::Char('q') => Some(QueueAction::Back),
        _ => None,
    }
}

fn render(frame: &mut Frame, snapshot: &[QueuedEvent], cursor: usize) {
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
        format!(" pending approvals ({}) ", snapshot.len()),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(header, chunks[0]);

    let now = SystemTime::now();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let items: Vec<ListItem> = snapshot
        .iter()
        .map(|q| {
            let (ent, scope_short, scope_id) = match ipc::parse_grant_name(&q.event.name) {
                Some(p) => (p.entitlement, p.scope.short(), p.scope_id),
                None => ("?", "?", "?"),
            };
            let age = now
                .duration_since(q.received_at)
                .map(|d| format_age(d.as_secs() as i64))
                .unwrap_or_else(|_| "0s".into());
            let source = q.event.source.as_deref().unwrap_or("-");
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<24}  ", truncate(ent, 24))),
                Span::styled(format!("{scope_short}:{scope_id:<16}  "), dim),
                Span::styled(format!("({source})  "), dim),
                Span::styled(format!("{age} ago"), dim),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(cursor.min(snapshot.len().saturating_sub(1))));
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" queue "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, chunks[1], &mut state);

    let footer = Paragraph::new(Span::styled(
        "j/k navigate | g/G top/bottom | d/x drop | esc/q back",
        dim,
    ));
    frame.render_widget(footer, chunks[2]);
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('~');
        out
    }
}
