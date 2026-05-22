use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use tokio::sync::{Mutex, mpsc};

use crate::backend::DynBackend;
use crate::cache::{Cache, EntitlementRow, GrantRow, meta_keys, now_unix};
use crate::fuzzy;
use crate::gcp::Scope;
use crate::gcp::grants::GrantState;
use crate::poller::GrantUpdate;
use crate::refresh::{self, RefreshEvent, RefreshOptions};

use super::Term;
use super::keymap::{Action, Focus, SearchMove, dispatch};
use super::widgets::{TextInput, format_age};

const STRIP_MAX_ROWS: usize = 5;

pub struct BrowseScreen {
    account: String,
    cache: Arc<Mutex<Cache>>,
    backend: DynBackend,
    refresh_options: RefreshOptions,

    focus: Focus,
    query: TextInput,
    entitlements: Vec<EntitlementRow>,
    filtered: Vec<usize>,
    selected: BTreeSet<usize>,
    cursor: usize,
    scope_filter: Option<Scope>,

    refresh_in_flight: bool,
    last_refresh: Option<i64>,
    status: String,
    show_raw: bool,
    show_help: bool,
    only_marked: bool,

    refresh_rx: mpsc::Receiver<RefreshEvent>,
    refresh_tx: mpsc::Sender<RefreshEvent>,

    tracked_grants: Vec<GrantRow>,
    grant_rx: mpsc::Receiver<GrantUpdate>,
}

impl BrowseScreen {
    pub async fn new(
        account: String,
        cache: Arc<Mutex<Cache>>,
        backend: DynBackend,
        refresh_options: RefreshOptions,
        grant_rx: mpsc::Receiver<GrantUpdate>,
    ) -> Result<Self> {
        let (entitlements, last_refresh, tracked_grants) = {
            let cache = cache.lock().await;
            let entitlements: Vec<EntitlementRow> = cache
                .list_entitlements()
                .unwrap_or_default()
                .into_iter()
                .filter(|e| refresh_options.includes(e.scope_type))
                .collect();
            (
                entitlements,
                cache
                    .meta_timestamp(meta_keys::ENTITLEMENTS_SCANNED_AT)
                    .ok()
                    .flatten(),
                cache.list_tracked_grants(now_unix()).unwrap_or_default(),
            )
        };
        let filtered = (0..entitlements.len()).collect();
        let (refresh_tx, refresh_rx) = mpsc::channel(256);

        Ok(Self {
            account,
            cache,
            backend,
            refresh_options,
            focus: Focus::List,
            query: TextInput::new(),
            entitlements,
            filtered,
            selected: BTreeSet::new(),
            cursor: 0,
            scope_filter: None,
            refresh_in_flight: false,
            last_refresh,
            status: String::new(),
            show_raw: false,
            show_help: false,
            only_marked: false,
            refresh_rx,
            refresh_tx,
            tracked_grants,
            grant_rx,
        })
    }

    pub async fn run(&mut self, term: &mut Term) -> Result<Option<Vec<EntitlementRow>>> {
        let mut events = EventStream::new();

        let decision = {
            let cache = self.cache.lock().await;
            refresh::decide(&cache)
        };
        if matches!(decision, refresh::Decision::Soft | refresh::Decision::Hard) {
            self.start_refresh();
        }

        // Re-read tracked grants whenever we (re-)enter the loop. Picks up
        // anything the request flow inserted while browse was suspended.
        self.reload_tracked_grants().await;

        // tick so the strip's countdown stays current even when no events arrive
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            term.draw(|f| self.render(f))?;

            tokio::select! {
                ev = events.next() => {
                    if let Some(Ok(ev)) = ev
                        && let Some(outcome) = self.handle_event(ev)
                    {
                        if outcome.is_some() {
                            // After a submit, clear the selection so a return
                            // visit starts fresh.
                            self.selected.clear();
                        }
                        return Ok(outcome);
                    }
                }
                ev = self.refresh_rx.recv() => {
                    if let Some(ev) = ev {
                        self.handle_refresh_event(ev).await;
                    }
                }
                update = self.grant_rx.recv() => {
                    if let Some(update) = update {
                        self.apply_grant_update(update).await;
                    }
                }
                _ = tick.tick() => {}
            }
        }
    }

    async fn reload_tracked_grants(&mut self) {
        let cache = self.cache.lock().await;
        self.tracked_grants = cache.list_tracked_grants(now_unix()).unwrap_or_default();
    }

    async fn apply_grant_update(&mut self, update: GrantUpdate) {
        // The poller has already persisted to the DB before sending.
        // reading back is the simplest way to keep state, timestamps, and the
        // window-filter (recent-terminal rows) consistent.
        self.reload_tracked_grants().await;
        if let Some(err) = update.error {
            self.status = format!("grant {}: {err}", short_id(&update.name));
        }
    }

    /// Returns `Some(outcome)` when the screen wants to exit.
    /// `Some(Some(rows))` = submit those; `Some(None)` = user canceled; `None` = stay.
    fn handle_event(&mut self, ev: Event) -> Option<Option<Vec<EntitlementRow>>> {
        let key = match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => k,
            _ => return None,
        };
        if self.show_help {
            let ctrl = key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL);
            if matches!(key.code, crossterm::event::KeyCode::Char('c')) && ctrl {
                return Some(None);
            }
            self.show_help = false;
            return None;
        }
        let action = dispatch(self.focus, key)?;
        self.apply(action)
    }

    fn apply(&mut self, action: Action) -> Option<Option<Vec<EntitlementRow>>> {
        match action {
            Action::Quit => return Some(None),
            Action::MoveUp(n) => {
                if self.focus == Focus::Search {
                    self.focus = Focus::List;
                }
                self.cursor = self.cursor.saturating_sub(n);
            }
            Action::MoveDown(n) => {
                if self.focus == Focus::Search {
                    self.focus = Focus::List;
                }
                if !self.filtered.is_empty() {
                    self.cursor = (self.cursor + n).min(self.filtered.len() - 1);
                }
            }
            Action::JumpTop => self.cursor = 0,
            Action::JumpBottom => {
                self.cursor = self.filtered.len().saturating_sub(1);
            }
            Action::ToggleSelect => {
                if let Some(&idx) = self.filtered.get(self.cursor)
                    && !self.selected.insert(idx)
                {
                    self.selected.remove(&idx);
                }
                if !self.filtered.is_empty() {
                    self.cursor = (self.cursor + 1).min(self.filtered.len() - 1);
                }
                // In marked view the list mirrors `self.selected`, so an
                // unmark has to drop the row immediately.
                if self.only_marked {
                    self.reindex();
                }
            }
            Action::Submit => {
                if self.selected.is_empty()
                    && let Some(&idx) = self.filtered.get(self.cursor)
                {
                    self.selected.insert(idx);
                }
                let chosen: Vec<_> = self
                    .selected
                    .iter()
                    .filter_map(|i| self.entitlements.get(*i).cloned())
                    .collect();
                if chosen.is_empty() {
                    return None;
                }
                return Some(Some(chosen));
            }
            Action::Refresh => self.start_refresh(),
            Action::FocusSearch => self.focus = Focus::Search,
            Action::FocusList => self.focus = Focus::List,
            Action::ClearSelection => {
                // one layer per press: marked view, then filter, then
                // selections, then quit. Arrow from search hands focus back
                // to the list while the query is still active, so a single
                // Esc must not wipe more than one layer at a time.
                if self.only_marked {
                    self.only_marked = false;
                    self.reindex();
                } else if !self.query.is_empty() {
                    self.query.clear();
                    self.reindex();
                } else if !self.selected.is_empty() {
                    self.selected.clear();
                } else {
                    return Some(None);
                }
            }
            Action::SearchInsert(c) => {
                self.query.insert(c);
                self.reindex();
            }
            Action::SearchBackspace => {
                self.query.backspace();
                self.reindex();
            }
            Action::SearchCursor(mv) => match mv {
                SearchMove::Left => self.query.move_left(),
                SearchMove::Right => self.query.move_right(),
                SearchMove::Home => self.query.move_home(),
                SearchMove::End => self.query.move_end(),
            },
            Action::SearchDeleteWordBack => {
                self.query.delete_word_back();
                self.reindex();
            }
            Action::Help => {
                self.show_help = true;
            }
            Action::ToggleRaw => {
                self.show_raw = !self.show_raw;
            }
            Action::ToggleMarkedView => {
                self.only_marked = !self.only_marked;
                self.status = if self.only_marked {
                    format!("view: marked ({})", self.selected.len())
                } else {
                    "view: all".into()
                };
                self.reindex();
            }
            Action::CycleScopeFilter => {
                self.scope_filter = match self.scope_filter {
                    None => Some(Scope::Project),
                    Some(Scope::Project) => Some(Scope::Folder),
                    Some(Scope::Folder) => Some(Scope::Organization),
                    Some(Scope::Organization) => None,
                };
                self.status = match self.scope_filter {
                    None => "scope filter: all".into(),
                    Some(s) => format!("scope filter: {s}"),
                };
                self.reindex();
            }
        }
        None
    }

    fn start_refresh(&mut self) {
        if self.refresh_in_flight {
            return;
        }
        self.refresh_in_flight = true;
        self.status = "refreshing...".into();
        refresh::spawn(
            self.cache.clone(),
            self.backend.clone(),
            self.refresh_options,
            self.refresh_tx.clone(),
        );
    }

    async fn handle_refresh_event(&mut self, ev: RefreshEvent) {
        match ev {
            RefreshEvent::Started => self.refresh_in_flight = true,
            RefreshEvent::EntitlementUpserted(row) => {
                if !self.refresh_options.includes(row.scope_type) {
                    return;
                }
                upsert_in_place(&mut self.entitlements, row);
                self.reindex();
            }
            RefreshEvent::Finished { total, error } => {
                self.refresh_in_flight = false;
                self.last_refresh = Some(now_unix());
                self.status = match error {
                    None => format!("refresh complete: {total} entitlements"),
                    Some(e) => format!("refresh failed: {e}"),
                };
                let rows = {
                    let cache = self.cache.lock().await;
                    cache.list_entitlements().unwrap_or_default()
                };
                self.entitlements = rows
                    .into_iter()
                    .filter(|e| self.refresh_options.includes(e.scope_type))
                    .collect();
                self.reindex();
            }
        }
    }

    fn reindex(&mut self) {
        let strings: Vec<String> = self
            .entitlements
            .iter()
            .map(|e| {
                format!(
                    "{} {} {} {} {}",
                    e.short_name,
                    e.scope_type.as_str(),
                    e.scope_type.short(),
                    e.scope_id,
                    e.approvers.join(" ")
                )
            })
            .collect();
        let ranked = fuzzy::rank(&strings, self.query.as_str());
        self.filtered = match self.scope_filter {
            None => ranked,
            Some(want) => ranked
                .into_iter()
                .filter(|&i| {
                    self.entitlements
                        .get(i)
                        .is_some_and(|e| e.scope_type == want)
                })
                .collect(),
        };
        // Marked view is an override: keep only rows the user has selected,
        // preserving the rank order computed above so the listing isn't
        // arbitrary.
        if self.only_marked {
            self.filtered.retain(|i| self.selected.contains(i));
        }
        if !self.filtered.is_empty() && self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len() - 1;
        }
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);

        self.render_body(frame, chunks[0]);
        self.render_status(frame, chunks[1]);

        if self.show_help {
            render_help(frame, area);
        }
    }

    /// Strip height *including* the bordered block (2 lines for borders).
    /// Zero when no live (active or pending) grants exist. Terminal grants
    /// are hidden from the strip. Their final outcome was shown in the
    /// request summary.
    fn strip_height(&self) -> u16 {
        let n = self.live_grants().count();
        if n == 0 {
            0
        } else {
            (n.min(STRIP_MAX_ROWS) as u16) + 2
        }
    }

    fn live_grants(&self) -> impl Iterator<Item = &GrantRow> {
        self.tracked_grants
            .iter()
            .filter(|g| !g.state.is_terminal())
    }

    fn render_strip(&self, frame: &mut Frame, area: Rect) {
        let now = now_unix();
        let total = self.live_grants().count();
        let mut lines: Vec<Line> = self
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

    fn render_body(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(columns[0]);

        self.render_search(frame, left[0]);
        self.render_list(frame, left[1]);

        // Right column: preview, with the live grants block pinned to the
        // bottom only when there is something to show. Sized to the actual
        // number of tracked rows so the preview keeps as much space as
        // possible.
        let strip_h = self.strip_height();
        if strip_h == 0 {
            self.render_preview(frame, columns[1]);
        } else {
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(strip_h)])
                .split(columns[1]);
            self.render_preview(frame, right[0]);
            self.render_strip(frame, right[1]);
        }
    }

    fn render_search(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Search;
        let title = format!(
            " search{} ({} of {}) ",
            if focused { "*" } else { "" },
            self.filtered.len(),
            self.entitlements.len()
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
        let p = Paragraph::new(self.query.as_str()).block(block);
        frame.render_widget(p, area);
        if focused {
            frame.set_cursor_position((area.x + 1 + self.query.display_cursor(), area.y + 1));
        }
    }

    fn render_list(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::List;
        let label = if self.only_marked {
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

        let items: Vec<ListItem> = self
            .filtered
            .iter()
            .filter_map(|&idx| self.entitlements.get(idx).map(|e| (idx, e)))
            .map(|(idx, ent)| self.render_row(idx, ent))
            .collect();

        let mut state = ListState::default();
        if !self.filtered.is_empty() {
            state.select(Some(self.cursor.min(self.filtered.len().saturating_sub(1))));
        }

        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_row<'a>(&self, idx: usize, ent: &'a EntitlementRow) -> ListItem<'a> {
        let selected = self.selected.contains(&idx);
        let mark = if selected { "[*]" } else { "[ ]" };
        let mark_style = if selected {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let auto = ent.approvers.is_empty();
        let badge = if auto { "[auto]" } else { "[appr]" };
        let badge_style = if auto {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Yellow)
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

    fn render_preview(&self, frame: &mut Frame, area: Rect) {
        let title = if self.show_raw {
            " preview (raw, J to collapse) "
        } else {
            " preview (J for raw) "
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let body = match self
            .filtered
            .get(self.cursor)
            .and_then(|&idx| self.entitlements.get(idx))
        {
            None => "no entitlement selected".to_string(),
            Some(ent) => preview_text(ent, self.show_raw),
        };
        let p = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
        frame.render_widget(p, area);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let age = self
            .last_refresh
            .map(|t| format_age(now_unix() - t))
            .unwrap_or_else(|| "never".into());
        let spinner = if self.refresh_in_flight {
            "[refreshing] "
        } else {
            ""
        };
        let scope = match self.scope_filter {
            None => "all".to_string(),
            Some(s) => s.to_string(),
        };
        let left = format!(
            "{spinner}{} | {} entitlements | scope:{scope} | cache age {age} | {}",
            self.account,
            self.entitlements.len(),
            self.status,
        );
        let keys = "? help";
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(keys.len() as u16 + 1),
            ])
            .split(area);
        let dim = Style::default().add_modifier(Modifier::DIM);
        frame.render_widget(Paragraph::new(Span::styled(left, dim)), chunks[0]);
        frame.render_widget(Paragraph::new(Span::styled(keys, dim)), chunks[1]);
    }
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
        ("R", "refresh"),
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

fn upsert_in_place(rows: &mut Vec<EntitlementRow>, row: EntitlementRow) {
    if let Some(existing) = rows.iter_mut().find(|r| r.name == row.name) {
        *existing = row;
    } else {
        rows.push(row);
    }
}

fn short_id(grant_name: &str) -> &str {
    grant_name.rsplit('/').next().unwrap_or(grant_name)
}

/// Render one grant as a single line in the strip. Format:
///   [..]  my-entitlement      proj:project-foo            wait 4s
///   [ok]  my-entitlement      proj:project-foo            exp: 23m
///   [err] my-entitlement      project-foo     Denied
fn strip_line(g: &GrantRow, now: i64) -> Line<'_> {
    let (badge, badge_style) = strip_badge(&g.state);
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

fn scope_label(ent: &EntitlementRow) -> String {
    let name = ent.scope_display_name.as_deref().filter(|s| !s.is_empty());
    match name {
        Some(n) => format!("{}:{}", ent.scope_type.short(), n),
        None => format!("{}:{}", ent.scope_type.short(), ent.scope_id),
    }
}

fn grant_scope_label(g: &GrantRow) -> String {
    let name = g.scope_display_name.as_deref().filter(|s| !s.is_empty());
    match name {
        Some(n) => format!("{}:{}", g.scope_type.short(), n),
        None => format!("{}:{}", g.scope_type.short(), g.scope_id),
    }
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

fn strip_badge(state: &GrantState) -> (&'static str, Style) {
    if state.is_active() {
        (
            "A",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "P",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    }
}

fn strip_timing(g: &GrantRow, now: i64) -> String {
    if g.state.is_active() {
        match g.expires_at {
            Some(exp) if exp > now => format!("exp: {}", format_age(exp - now)),
            Some(_) => "expired".into(),
            None => String::new(),
        }
    } else {
        format!("wait: {}", format_age(now - g.created_at))
    }
}

fn preview_text(ent: &EntitlementRow, show_raw: bool) -> String {
    if show_raw {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&ent.raw_json)
            && let Ok(pretty) = serde_json::to_string_pretty(&value)
        {
            return pretty;
        }
        return ent.raw_json.clone();
    }

    let mut out = String::new();
    out.push_str(&format!("name:    {}\n", ent.short_name));
    out.push_str(&format!(
        "scope:   {}/{}\n",
        ent.scope_type.plural(),
        ent.scope_id
    ));
    if let Some(secs) = ent.max_request_duration_secs {
        out.push_str(&format!("max:     {}\n", format_age(secs)));
    }
    out.push_str(&format!(
        "justify: {}\n",
        if ent.justification_required {
            "required"
        } else {
            "optional"
        }
    ));
    if ent.approvers.is_empty() {
        out.push_str("approval: auto\n");
    } else {
        out.push_str("approvers:\n");
        for a in &ent.approvers {
            out.push_str(&format!("  - {a}\n"));
        }
    }
    if ent.roles.is_empty() {
        out.push_str("roles:    (none listed)\n");
    } else {
        out.push_str("roles:\n");
        for r in &ent.roles {
            out.push_str(&format!("  - {r}\n"));
        }
    }
    out
}
