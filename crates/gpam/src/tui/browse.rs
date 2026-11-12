use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use gpam_core::app::action::Focus;
use gpam_core::app::browse::{
    ApprovalMode, BrowseExit, BrowseState, GrantBadge, approval_badge, grant_scope_label,
    scope_label, strip_badge, strip_timing,
};
use gpam_core::app::format::{format_age, truncate};
use gpam_core::cache::{EntitlementRow, GrantRow, now_unix};
use gpam_core::logs::LogEntry;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::Term;
use super::keymap::dispatch;

const STRIP_MAX_ROWS: usize = 5;

/// Run the browse view until it hands control back to the outer loop.
pub async fn run(term: &mut Term, state: &mut BrowseState) -> Result<BrowseExit> {
    let mut events = EventStream::new();

    state.activate().await;

    // tick so the strip's countdown stays current even when no events arrive
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        term.draw(|f| render(f, state))?;

        tokio::select! {
            ev = events.next() => {
                if let Some(Ok(ev)) = ev
                    && let Some(outcome) = handle_event(state, ev)
                {
                    return Ok(outcome);
                }
            }
            update = state.next_update() => {
                if let Some(update) = update {
                    state.handle_update(update).await;
                }
            }
            _ = tick.tick() => {}
        }
    }
}

/// Returns `Some(exit)` when the view wants to exit, `None` to stay.
fn handle_event(state: &mut BrowseState, ev: Event) -> Option<BrowseExit> {
    let key = match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => k,
        _ => return None,
    };
    if state.overlay_open() {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, KeyCode::Char('c')) && ctrl {
            return Some(BrowseExit::Quit);
        }
        state.close_overlays();
        return None;
    }
    let action = dispatch(state.focus(), key)?;
    state.apply(action)
}

fn render(frame: &mut Frame, state: &BrowseState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    render_body(frame, state, chunks[0]);
    render_status(frame, state, chunks[1]);

    if state.show_help() {
        render_help(frame, area);
    }
    if state.show_logs() {
        let (last, log_path) = state.log_snapshot();
        render_logs_popup(frame, area, last.as_ref(), &log_path);
    }
}

/// Strip height *including* the bordered block (2 lines for borders).
/// Zero when no live (active or pending) grants exist.
fn strip_height(state: &BrowseState) -> u16 {
    let n = state.live_grants().count();
    if n == 0 {
        0
    } else {
        (n.min(STRIP_MAX_ROWS) as u16) + 2
    }
}

fn render_strip(frame: &mut Frame, state: &BrowseState, area: Rect) {
    let now = now_unix();
    let total = state.live_grants().count();
    let mut lines: Vec<Line> = state
        .live_grants()
        .take(STRIP_MAX_ROWS)
        .map(|g| strip_line(g, now))
        .collect();
    let hidden = total.saturating_sub(STRIP_MAX_ROWS);
    if hidden > 0
        && let Some(last) = lines.last_mut()
    {
        *last = Line::from(vec![Span::styled(
            format!("  ... +{hidden} more not shown"),
            Style::default().add_modifier(Modifier::DIM),
        )]);
    }
    let title = format!(" grants ({total}) ");
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_body(frame: &mut Frame, state: &BrowseState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(columns[0]);

    render_search(frame, state, left[0]);
    render_list(frame, state, left[1]);

    // Right column: preview, with the live grants block pinned to the
    // bottom only when there is something to show. Sized to the actual
    // number of tracked rows so the preview keeps as much space as
    // possible.
    let strip_h = strip_height(state);
    if strip_h == 0 {
        render_preview(frame, state, columns[1]);
    } else {
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(strip_h)])
            .split(columns[1]);
        render_preview(frame, state, right[0]);
        render_strip(frame, state, right[1]);
    }
}

fn render_search(frame: &mut Frame, state: &BrowseState, area: Rect) {
    let focused = state.focus() == Focus::Search;
    let title = format!(
        " search{} ({} of {}) ",
        if focused { "*" } else { "" },
        state.filtered().len(),
        state.entitlements().len()
    );
    let border = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);
    let query = state.query();
    let p = Paragraph::new(query.as_str()).block(block);
    frame.render_widget(p, area);
    if focused {
        frame.set_cursor_position((area.x + 1 + query.display_cursor(), area.y + 1));
    }
}

fn render_list(frame: &mut Frame, state: &BrowseState, area: Rect) {
    let focused = state.focus() == Focus::List;
    let label = if state.only_marked() {
        "marked"
    } else {
        "entitlements"
    };
    let title = if focused {
        format!(" {label}* ")
    } else {
        format!(" {label} ")
    };
    let border = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);

    let items: Vec<ListItem> = state
        .visible_rows()
        .map(|(idx, ent)| render_row(ent, state.is_marked(idx)))
        .collect();

    let filtered_len = state.filtered().len();
    let mut list_state = ListState::default();
    if filtered_len > 0 {
        list_state.select(Some(state.cursor().min(filtered_len.saturating_sub(1))));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_row(ent: &EntitlementRow, selected: bool) -> ListItem<'_> {
    let mark = if selected { "[*]" } else { "[ ]" };
    let mark_style = if selected {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let (badge, mode) = approval_badge(ent);
    let badge_style = match mode {
        ApprovalMode::Auto => Style::default().fg(Color::Cyan),
        ApprovalMode::Manual => Style::default().fg(Color::Yellow),
    };
    ListItem::new(Line::from(vec![
        Span::styled(mark, mark_style),
        Span::raw(" "),
        Span::styled(badge, badge_style),
        Span::raw(" "),
        Span::raw(ent.short_name.clone()),
        Span::raw("  "),
        Span::styled(
            scope_label(ent),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]))
}

fn render_preview(frame: &mut Frame, state: &BrowseState, area: Rect) {
    let title = if state.show_raw() {
        " preview (raw, J to collapse) "
    } else {
        " preview (J for raw) "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let p = Paragraph::new(state.preview())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

fn render_status(frame: &mut Frame, state: &BrowseState, area: Rect) {
    let age = state.cache_age_label(now_unix());
    let spinner = if state.refresh_in_flight() {
        "[refreshing] "
    } else {
        ""
    };
    let scope = state.scope_filter_label();
    let left = format!(
        "{spinner}{} | {} entitlements | scope:{scope} | cache age {age} | {}",
        state.account(),
        state.entitlements().len(),
        state.status(),
    );
    let unread = state.unread_log_count();
    let warn = if unread > 0 {
        format!("[{unread} warn] ")
    } else {
        String::new()
    };
    let pending = state.pending_approvals();
    let pending_badge = if pending > 0 {
        format!("[{pending} pending] ")
    } else {
        String::new()
    };
    let keys = "L logs ? help";
    let dim = Style::default().add_modifier(Modifier::DIM);
    let accent_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let right_len = pending_badge.len() + warn.len() + keys.len() + 1;
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_len as u16)])
        .split(area);
    frame.render_widget(Paragraph::new(Span::styled(left, dim)), chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(pending_badge, accent_style),
            Span::styled(warn, accent_style),
            Span::styled(keys, dim),
        ])),
        chunks[1],
    );
}

/// Centered help overlay. Two columns: list-focus bindings and search-focus
/// bindings. Any key dismisses it (handled in `handle_event`).
fn render_help(frame: &mut Frame, area: Rect) {
    let list_keys: &[(&str, &str)] = &[
        ("j / down / ^n", "move down"),
        ("k / up / ^p", "move up"),
        ("^d / ^u", "move 10"),
        ("g / G", "top / bottom"),
        ("space", "toggle select"),
        ("enter", "request selected"),
        ("esc", "clear filter, then selection"),
        ("/", "focus search"),
        ("f", "cycle scope filter"),
        ("m", "show marked only"),
        ("J", "toggle raw JSON"),
        ("A", "open approval queue"),
        ("R", "refresh"),
        ("L", "show warn/error log"),
        ("?", "this help"),
        ("q / ^c", "quit"),
    ];
    let search_keys: &[(&str, &str)] = &[
        ("type", "filter entries"),
        ("up / down", "to list + move"),
        ("^n / ^p", "to list + move"),
        ("^a / ^e", "home / end"),
        ("^w", "delete word"),
        ("enter", "request match"),
        ("esc", "back to list"),
    ];

    let max_pair_w = |rows: &[(&str, &str)]| {
        rows.iter()
            .map(|(k, v)| k.len() + v.len() + 3)
            .max()
            .unwrap_or(0)
    };
    // 3 chars padding between cols + 4 chars window padding + 2 borders.
    let inner_w = max_pair_w(list_keys).max(max_pair_w(search_keys)) as u16;
    let w = (inner_w * 2 + 3 + 4 + 2)
        .min(area.width.saturating_sub(4))
        .max(40);
    let h = (list_keys.len().max(search_keys.len()) as u16 + 4).min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help — press any key to close ")
        .border_style(Style::default().fg(Color::Yellow));

    // Clear what's underneath, then render the block and its two columns.
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(block.clone(), rect);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(2)
        .vertical_margin(1)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(rect);

    let header = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner[0]);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    frame.render_widget(Paragraph::new(Span::styled("list", bold)), header[0]);
    frame.render_widget(Paragraph::new(Span::styled("search", bold)), header[1]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner[1]);

    let render_col = |frame: &mut Frame, area: Rect, rows: &[(&str, &str)]| {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let lines: Vec<Line> = rows
            .iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(format!("{k:<14}"), Style::default().fg(Color::Cyan)),
                    Span::styled((*v).to_string(), dim),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    };
    render_col(frame, body[0], list_keys);
    render_col(frame, body[1], search_keys);
}

fn render_logs_popup(frame: &mut Frame, area: Rect, last: Option<&LogEntry>, log_path: &str) {
    let w = area.width.saturating_sub(4).clamp(40, 80);
    let h = area.height.saturating_sub(2).clamp(8, 16);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" logs - press any key to close ")
        .border_style(Style::default().fg(Color::Yellow));

    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(block.clone(), rect);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(2)
        .vertical_margin(1)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(rect);

    let dim = Style::default().add_modifier(Modifier::DIM);
    let body: Vec<Line> = match last {
        None => vec![Line::from(Span::styled(
            "no warnings or errors recorded yet",
            dim,
        ))],
        Some(entry) => {
            let level_style = match entry.level {
                tracing::Level::ERROR => {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
                _ => Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            };
            let age = entry
                .at
                .elapsed()
                .map(|d| format_age(d.as_secs() as i64))
                .unwrap_or_else(|_| "0s".into());
            vec![
                Line::from(vec![
                    Span::styled(format!("{} ", entry.level), level_style),
                    Span::styled(format!("({age} ago)"), dim),
                ]),
                Line::from(Span::raw(entry.message.clone())),
            ]
        }
    };
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), inner[0]);

    let footer = vec![
        Line::from(Span::styled("full log:", dim)),
        Line::from(Span::raw(log_path.to_string())),
    ];
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), inner[1]);
}

/// Render one grant as a single line in the strip. Format:
///   A   my-entitlement      proj:project-foo            exp: 23m
///   P   my-entitlement      proj:project-foo            wait: 4s
fn strip_line(g: &GrantRow, now: i64) -> Line<'_> {
    let (badge, kind) = strip_badge(&g.state);
    let badge_style = match kind {
        GrantBadge::Active => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        GrantBadge::Pending => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    };
    let timing = strip_timing(g, now);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let ent = truncate(&g.entitlement_short_name, 20);
    let scope = truncate(&grant_scope_label(g), 30);
    Line::from(vec![
        Span::styled(format!("{badge} "), badge_style),
        Span::raw(format!("{ent:<20}  ")),
        Span::styled(format!("{scope:<30}  "), dim),
        Span::styled(timing, dim),
    ])
}
