use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use crate::backend::DynBackend;
use crate::cache::{Cache, EntitlementRow, GrantRow, ScopeRow, meta_keys, now_unix};
use crate::fuzzy;
use crate::gcp::Scope;
use crate::gcp::grants::GrantState;
use crate::logs::{LogEntry, SharedLogState};
use crate::poller::GrantUpdate;
use crate::refresh::{self, RefreshEvent, RefreshOptions};

use super::action::{Action, Focus, SearchMove};
use super::approval_queue::Queue;
use super::format::format_age;
use super::scope_tree::{NavAction, ScopeKey, ScopeTree, TreeRow};
use super::text_input::TextInput;

/// Column the entitlement list can be ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Scope,
    Approval,
    MaxDuration,
    /// Entitlements with a live grant first: pending ones, then active ones
    /// by expiry. Entitlements without a grant always come last.
    Grant,
}

impl SortColumn {
    pub const ALL: [SortColumn; 5] = [
        SortColumn::Name,
        SortColumn::Scope,
        SortColumn::Approval,
        SortColumn::MaxDuration,
        SortColumn::Grant,
    ];
}

/// A column and direction the entitlement list is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortOrder {
    pub column: SortColumn,
    pub descending: bool,
}

/// What [`BrowseState::apply`] decided to hand back to the frontend. The
/// frontend opens the next view (request flow or approval queue) based on
/// the variant.
#[derive(Debug)]
pub enum BrowseExit {
    Quit,
    Request(Vec<EntitlementRow>),
    OpenApprovalQueue,
}

/// One background message for the browse view: refresh progress or a grant
/// poller wake-up. Obtain it with [`BrowseState::next_update`] and pass it to
/// [`BrowseState::handle_update`].
#[derive(Debug)]
pub struct BrowseUpdate(UpdateKind);

#[derive(Debug)]
enum UpdateKind {
    Refresh(Box<RefreshEvent>),
    Grants,
}

/// How an entitlement's approval requirement is shown next to its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// No approvers. Grants activate without a human in the loop.
    Auto,
    /// At least one approver must act on the grant.
    Manual,
}

/// Coarse state of a live grant, used to pick the badge color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantBadge {
    Active,
    Pending,
}

/// Entitlement browser state: the cached entitlement list, fuzzy search,
/// marks, scope filter, refresh bookkeeping, and the tracked grants strip.
pub struct BrowseState {
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
    /// Column order of the list. `None` keeps the search ranking.
    sort: Option<SortOrder>,
    /// Role names the list is narrowed to. An entitlement stays when each
    /// of these is part of the name of a role it grants.
    role_filters: Vec<String>,

    refresh_in_flight: bool,
    last_refresh: Option<i64>,
    status: String,
    /// The status message [`BrowseState::expire_status`] last saw and when
    /// it first saw it.
    status_seen: Option<(String, i64)>,
    show_raw: bool,
    show_help: bool,
    show_logs: bool,
    only_marked: bool,

    refresh_rx: mpsc::Receiver<RefreshEvent>,
    refresh_tx: mpsc::Sender<RefreshEvent>,

    tracked_grants: Vec<GrantRow>,
    grant_rx: mpsc::Receiver<GrantUpdate>,

    /// Cached organizations, folders and projects the tree is built from.
    scopes: Vec<ScopeRow>,
    tree: ScopeTree,
    /// Scopes the list is narrowed to, in the order they were added. Empty
    /// means every scope.
    scope_nodes: Vec<ScopeKey>,
    /// Navigator cursor. Zero is the "all scopes" line, and `n` is line
    /// `n - 1` of [`ScopeTree::rows`].
    nav_cursor: usize,

    approval_queue: Queue,

    log_state: SharedLogState,
}

impl BrowseState {
    pub(crate) async fn new(
        account: String,
        cache: Arc<Mutex<Cache>>,
        backend: DynBackend,
        refresh_options: RefreshOptions,
        grant_rx: mpsc::Receiver<GrantUpdate>,
        approval_queue: Queue,
        log_state: SharedLogState,
    ) -> Self {
        let (entitlements, last_refresh, tracked_grants, scopes) = {
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
                cache.list_scopes().unwrap_or_default(),
            )
        };
        let tree = ScopeTree::build(&scopes, &entitlements);
        let filtered = (0..entitlements.len()).collect();
        let (refresh_tx, refresh_rx) = mpsc::channel(256);

        Self {
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
            sort: None,
            role_filters: Vec::new(),
            refresh_in_flight: false,
            last_refresh,
            status: String::new(),
            status_seen: None,
            show_raw: false,
            show_help: false,
            show_logs: false,
            only_marked: false,
            refresh_rx,
            refresh_tx,
            tracked_grants,
            grant_rx,
            scopes,
            tree,
            scope_nodes: Vec::new(),
            nav_cursor: 0,
            approval_queue,
            log_state,
        }
    }

    /// Call each time the browse view becomes visible. Starts a background
    /// refresh when the cache is stale, and re-reads tracked grants so rows
    /// inserted by the request flow show up.
    pub async fn activate(&mut self) {
        let decision = {
            let cache = self.cache.lock().await;
            refresh::decide(&cache)
        };
        if matches!(decision, refresh::Decision::Soft | refresh::Decision::Hard) {
            self.start_refresh();
        }
        self.reload_tracked_grants().await;
    }

    /// Wait for the next background message. Cancel-safe, so it can sit in a
    /// `select!` next to the frontend's own input source.
    pub async fn next_update(&mut self) -> Option<BrowseUpdate> {
        tokio::select! {
            ev = self.refresh_rx.recv() => {
                ev.map(|ev| BrowseUpdate(UpdateKind::Refresh(Box::new(ev))))
            }
            update = self.grant_rx.recv() => update.map(|_| BrowseUpdate(UpdateKind::Grants)),
        }
    }

    /// Non-blocking counterpart of [`BrowseState::next_update`] for frontends
    /// that poll once per frame. Returns `None` when nothing is queued.
    pub fn try_next_update(&mut self) -> Option<BrowseUpdate> {
        if let Ok(ev) = self.refresh_rx.try_recv() {
            return Some(BrowseUpdate(UpdateKind::Refresh(Box::new(ev))));
        }
        self.grant_rx
            .try_recv()
            .ok()
            .map(|_| BrowseUpdate(UpdateKind::Grants))
    }

    pub async fn handle_update(&mut self, update: BrowseUpdate) {
        match update.0 {
            UpdateKind::Refresh(ev) => self.handle_refresh_event(*ev).await,
            // The poller has already persisted to the DB before sending.
            // Reading back is the simplest way to keep state, timestamps, and
            // the window filter (recent-terminal rows) consistent. Poll errors
            // flow through the tracing layer into the log file and the logs
            // popup, so the status text stays free for in-band UI feedback.
            UpdateKind::Grants => self.reload_tracked_grants().await,
        }
    }

    async fn reload_tracked_grants(&mut self) {
        self.tracked_grants = {
            let cache = self.cache.lock().await;
            cache.list_tracked_grants(now_unix()).unwrap_or_default()
        };
        if self.sort.is_some_and(|o| o.column == SortColumn::Grant) {
            self.resort();
        }
    }

    /// Role names the list is narrowed to, in the order they were added.
    pub fn role_filters(&self) -> &[String] {
        &self.role_filters
    }

    /// Narrow the list to entitlements granting a role whose name contains
    /// `role`. Empty and repeated names are ignored.
    pub fn add_role_filter(&mut self, role: &str) {
        let role = role.trim();
        let known = self
            .role_filters
            .iter()
            .any(|r| r.eq_ignore_ascii_case(role));
        if !role.is_empty() && !known {
            self.role_filters.push(role.to_string());
            self.cursor = 0;
            self.reindex();
        }
    }

    pub fn remove_role_filter(&mut self, role: &str) {
        let before = self.role_filters.len();
        self.role_filters.retain(|r| r != role);
        if self.role_filters.len() != before {
            self.cursor = 0;
            self.reindex();
        }
    }

    /// Drop the role filter added last.
    pub fn pop_role_filter(&mut self) {
        if self.role_filters.pop().is_some() {
            self.cursor = 0;
            self.reindex();
        }
    }

    /// What a typed role name could stand for, best first. When several
    /// roles match, the typed text itself comes first and stands for all of
    /// them. Then come the matching role names, those starting with the
    /// text ahead of the others.
    pub fn role_candidates(&self, text: &str) -> Vec<String> {
        let typed = text.trim();
        let needle = typed.to_lowercase();
        let needle = needle.strip_prefix("roles/").unwrap_or(&needle);
        let roles: BTreeSet<&str> = self
            .entitlements
            .iter()
            .flat_map(|e| e.roles.iter().map(String::as_str))
            .filter(|role| role.to_lowercase().contains(needle))
            .collect();
        let (mut out, rest): (Vec<&str>, Vec<&str>) = roles
            .into_iter()
            .partition(|role| role_label(role).to_lowercase().starts_with(needle));
        out.extend(rest);
        let mut out: Vec<String> = out.into_iter().map(str::to_string).collect();
        if out.len() > 1 && !typed.is_empty() {
            out.insert(0, typed.to_string());
        }
        out
    }

    /// Column order of the list, or `None` while it follows the search
    /// ranking.
    pub fn sort(&self) -> Option<SortOrder> {
        self.sort
    }

    /// Step the order of one column: ascending, descending, then off.
    pub fn toggle_sort(&mut self, column: SortColumn) {
        self.sort = match self.sort {
            Some(order) if order.column == column && !order.descending => Some(SortOrder {
                column,
                descending: true,
            }),
            Some(order) if order.column == column => None,
            _ => Some(SortOrder {
                column,
                descending: false,
            }),
        };
        self.resort();
    }

    /// Order by the next column, ascending. After the last column the list
    /// goes back to the search ranking.
    pub fn cycle_sort(&mut self) {
        let next = match self.sort {
            None => SortColumn::ALL.first().copied(),
            Some(order) => SortColumn::ALL
                .iter()
                .position(|c| *c == order.column)
                .and_then(|i| SortColumn::ALL.get(i + 1).copied()),
        };
        self.sort = next.map(|column| SortOrder {
            column,
            descending: false,
        });
        self.resort();
    }

    /// Flip the direction of the current column order, if there is one.
    pub fn reverse_sort(&mut self) {
        if let Some(order) = &mut self.sort {
            order.descending = !order.descending;
            self.resort();
        }
    }

    /// Rebuild the list and keep the cursor on the entitlement it was on.
    fn resort(&mut self) {
        let current = self.filtered.get(self.cursor).copied();
        self.reindex();
        if let Some(pos) = current.and_then(|idx| self.filtered.iter().position(|&i| i == idx)) {
            self.cursor = pos;
        }
    }

    /// Value an entitlement is ordered by under `column`. `None` sorts last
    /// in both directions.
    fn sort_key(
        &self,
        column: SortColumn,
        ent: &EntitlementRow,
        grants: &HashMap<&str, &GrantRow>,
    ) -> Option<(i64, i64, String)> {
        Some(match column {
            SortColumn::Name => (0, 0, ent.short_name.to_lowercase()),
            SortColumn::Scope => (0, 0, scope_label(ent).to_lowercase()),
            SortColumn::Approval => (i64::from(!ent.approvers.is_empty()), 0, String::new()),
            SortColumn::MaxDuration => {
                (0, ent.max_request_duration_secs.unwrap_or(0), String::new())
            }
            SortColumn::Grant => {
                let g = grants.get(ent.name.as_str())?;
                if grant_expired(g, now_unix()) {
                    (2, g.expires_at.unwrap_or(i64::MAX), String::new())
                } else if g.state.is_active() {
                    (1, g.expires_at.unwrap_or(i64::MAX), String::new())
                } else {
                    (0, g.created_at, String::new())
                }
            }
        })
    }

    /// Put `filtered` in column order. The sort is stable, so equal rows
    /// keep their search ranking.
    fn apply_sort(&mut self, order: SortOrder) {
        let grants = self.grants_by_entitlement();
        let mut keyed: Vec<_> = self
            .filtered
            .iter()
            .filter_map(|&i| {
                let ent = self.entitlements.get(i)?;
                Some((i, self.sort_key(order.column, ent, &grants)))
            })
            .collect();
        keyed.sort_by(|(_, a), (_, b)| match (a, b) {
            (Some(a), Some(b)) if order.descending => b.cmp(a),
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
        self.filtered = keyed.into_iter().map(|(i, _)| i).collect();
    }

    /// True while the help or logs overlay is shown. The frontend should
    /// dismiss it on the next key instead of dispatching an [`Action`].
    pub fn overlay_open(&self) -> bool {
        self.show_help || self.show_logs
    }

    pub fn close_overlays(&mut self) {
        self.show_help = false;
        self.show_logs = false;
    }

    /// Apply one action. Returns `Some(exit)` when the frontend should leave
    /// the browse view, `None` to stay.
    pub fn apply(&mut self, action: Action) -> Option<BrowseExit> {
        match action {
            Action::Quit => return Some(BrowseExit::Quit),
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
                let implicit = self.selected.is_empty();
                if implicit && let Some(&idx) = self.filtered.get(self.cursor) {
                    self.selected.insert(idx);
                }
                let picked: Vec<_> = self
                    .selected
                    .iter()
                    .filter_map(|i| self.entitlements.get(*i).cloned())
                    .collect();
                // PAM rejects a request for an entitlement that already has
                // a pending or running grant, so those are left out.
                let now = now_unix();
                let held: Vec<String> = {
                    let grants = self.grants_by_entitlement();
                    picked
                        .iter()
                        .filter_map(|ent| {
                            let g = grants.get(ent.name.as_str())?;
                            if grant_expired(g, now) {
                                return None;
                            }
                            Some(if g.state.is_active() {
                                format!(
                                    "{}: already active, {}",
                                    ent.short_name,
                                    strip_timing(g, now)
                                )
                            } else {
                                format!("{}: already requested, waiting", ent.short_name)
                            })
                        })
                        .collect()
                };
                let chosen: Vec<_> = {
                    let grants = self.grants_by_entitlement();
                    picked
                        .into_iter()
                        .filter(|ent| {
                            grants
                                .get(ent.name.as_str())
                                .is_none_or(|g| grant_expired(g, now))
                        })
                        .collect()
                };
                match held.as_slice() {
                    [] => {}
                    [only] => self.status = only.clone(),
                    many => {
                        self.status = format!("left out {} that already have a grant", many.len())
                    }
                }
                if chosen.is_empty() {
                    // A row picked only by the cursor does not stay marked.
                    if implicit {
                        self.selected.clear();
                    }
                    return None;
                }
                // Clear the selection so a return visit starts fresh.
                self.selected.clear();
                return Some(BrowseExit::Request(chosen));
            }
            Action::Refresh => self.start_refresh(),
            Action::FocusSearch => self.focus = Focus::Search,
            Action::FocusList => self.focus = Focus::List,
            Action::ClearSelection => {
                // One layer per press: marked view, then filter, then role
                // filters, then the navigator scope, then selections, then
                // quit. Arrow from search hands focus back to the list while
                // the query is still active, so a single Esc must not wipe
                // more than one layer at a time.
                if self.only_marked {
                    self.only_marked = false;
                    self.reindex();
                } else if !self.query.is_empty() {
                    self.query.clear();
                    self.reindex();
                } else if !self.role_filters.is_empty() {
                    self.role_filters.clear();
                    self.reindex();
                } else if !self.scope_nodes.is_empty() {
                    self.scope_nodes.clear();
                    self.nav_cursor = 0;
                    self.reindex();
                } else if !self.selected.is_empty() {
                    self.selected.clear();
                } else {
                    return Some(BrowseExit::Quit);
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
            Action::ShowLogs => {
                self.show_logs = true;
                if let Ok(mut state) = self.log_state.lock() {
                    state.clear_unread();
                }
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
                // The type filter and the chosen scopes would hide each
                // other's rows, so whichever was chosen last wins.
                self.scope_nodes.clear();
                self.nav_cursor = 0;
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
            Action::OpenApprovalQueue => {
                if self.pending_approvals() == 0 {
                    self.status = "no pending approvals".into();
                } else {
                    return Some(BrowseExit::OpenApprovalQueue);
                }
            }
        }
        None
    }

    /// Replace the whole search query, for frontends with a native text
    /// field. Re-filters like a typed character does.
    pub fn set_query(&mut self, query: &str) {
        if self.query.as_str() == query {
            return;
        }
        self.query.set_text(query);
        self.reindex();
    }

    /// Move the cursor to a position within [`BrowseState::filtered`],
    /// clamped to the list. Used for pointer selection.
    pub fn set_cursor(&mut self, pos: usize) {
        if !self.filtered.is_empty() {
            self.cursor = pos.min(self.filtered.len() - 1);
        }
    }

    /// Toggle the mark on the row at `pos` within [`BrowseState::filtered`]
    /// without moving the cursor. Used for pointer marking.
    pub fn toggle_mark_at(&mut self, pos: usize) {
        let Some(&idx) = self.filtered.get(pos) else {
            return;
        };
        if !self.selected.insert(idx) {
            self.selected.remove(&idx);
        }
        if self.only_marked {
            self.reindex();
        }
    }

    /// Mark or unmark the rows at the given positions within
    /// [`BrowseState::filtered`]. Positions past the end are ignored. Used to
    /// apply one mark state to a multi-row pointer selection.
    pub fn set_marks_at(&mut self, positions: &[usize], marked: bool) {
        for &idx in positions.iter().filter_map(|pos| self.filtered.get(*pos)) {
            if marked {
                self.selected.insert(idx);
            } else {
                self.selected.remove(&idx);
            }
        }
        if self.only_marked && !marked {
            self.reindex();
        }
    }

    /// Drop every mark. In marked view the list empties with it.
    pub fn clear_marks(&mut self) {
        if self.selected.is_empty() {
            return;
        }
        self.selected.clear();
        if self.only_marked {
            self.reindex();
        }
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
                self.rebuild_tree();
                self.reindex();
            }
            RefreshEvent::Finished { total, error } => {
                self.refresh_in_flight = false;
                self.last_refresh = Some(now_unix());
                self.status = match error {
                    None => format!("refresh complete: {total} entitlements"),
                    Some(e) => format!("refresh failed: {e}"),
                };
                let (rows, scopes) = {
                    let cache = self.cache.lock().await;
                    (
                        cache.list_entitlements().unwrap_or_default(),
                        cache.list_scopes().unwrap_or_default(),
                    )
                };
                self.entitlements = rows
                    .into_iter()
                    .filter(|e| self.refresh_options.includes(e.scope_type))
                    .collect();
                self.scopes = scopes;
                self.rebuild_tree();
                self.reindex();
            }
        }
    }

    /// Recompute the scope tree after the entitlements or scopes changed.
    /// Active scopes that no longer hold entitlements are dropped.
    fn rebuild_tree(&mut self) {
        self.tree.rebuild(&self.scopes, &self.entitlements);
        self.scope_nodes.retain(|key| self.tree.contains(key));
        self.nav_cursor = self.nav_cursor.min(self.tree.rows().len());
    }

    /// Visible navigator lines below the "all scopes" line.
    pub fn scope_rows(&self) -> Vec<TreeRow> {
        self.tree.rows()
    }

    /// Navigator cursor. Zero is the "all scopes" line, and `n` is line
    /// `n - 1` of [`BrowseState::scope_rows`].
    pub fn nav_cursor(&self) -> usize {
        self.nav_cursor
    }

    /// The scopes the list is narrowed to, in the order they were added.
    /// Empty means every scope.
    pub fn scope_nodes(&self) -> &[ScopeKey] {
        &self.scope_nodes
    }

    /// Display name of a scope in the tree.
    pub fn scope_label(&self, key: &ScopeKey) -> Option<&str> {
        self.tree.label(key)
    }

    /// The scope a typed name refers to, if any. See [`ScopeTree::resolve`].
    pub fn resolve_scope(&self, text: &str) -> Option<ScopeKey> {
        self.tree.resolve(text)
    }

    /// Every scope a typed name could refer to, best first.
    pub fn scope_candidates(&self, text: &str) -> Vec<ScopeKey> {
        self.tree.candidates(text)
    }

    pub fn set_nav_cursor(&mut self, pos: usize) {
        self.nav_cursor = pos.min(self.tree.rows().len());
    }

    /// Open or close the branch on navigator line `pos`.
    pub fn toggle_scope_expanded_at(&mut self, pos: usize) {
        let rows = self.tree.rows();
        if let Some(row) = pos.checked_sub(1).and_then(|i| rows.get(i)) {
            self.tree.set_expanded(&row.key, !row.expanded);
        }
    }

    /// Narrow the list to the scope on navigator line `pos` alone. Line zero
    /// shows every scope again.
    pub fn select_scope_at(&mut self, pos: usize) {
        let rows = self.tree.rows();
        self.set_nav_cursor(pos);
        self.scope_nodes = self
            .nav_cursor
            .checked_sub(1)
            .and_then(|i| rows.get(i))
            .map(|row| row.key.clone())
            .into_iter()
            .collect();
        self.scopes_changed();
    }

    /// Replace the active scopes. Unknown and repeated scopes are dropped.
    pub fn set_scopes(&mut self, keys: Vec<ScopeKey>) {
        let mut scopes: Vec<ScopeKey> = Vec::new();
        for key in keys {
            if self.tree.contains(&key) && !scopes.contains(&key) {
                scopes.push(key);
            }
        }
        if scopes != self.scope_nodes {
            self.scope_nodes = scopes;
            self.scopes_changed();
        }
    }

    /// Add the scope on navigator line `pos` to the active scopes, or take
    /// it out when it is already one of them.
    pub fn toggle_scope_at(&mut self, pos: usize) {
        let rows = self.tree.rows();
        self.set_nav_cursor(pos);
        let Some(row) = self.nav_cursor.checked_sub(1).and_then(|i| rows.get(i)) else {
            return;
        };
        if self.scope_nodes.contains(&row.key) {
            self.remove_scope(&row.key);
        } else {
            self.add_scope(row.key.clone());
        }
    }

    /// Add a scope to the active scopes. Unknown and repeated scopes are
    /// ignored.
    pub fn add_scope(&mut self, key: ScopeKey) {
        if self.tree.contains(&key) && !self.scope_nodes.contains(&key) {
            self.scope_nodes.push(key);
            self.scopes_changed();
        }
    }

    pub fn remove_scope(&mut self, key: &ScopeKey) {
        let before = self.scope_nodes.len();
        self.scope_nodes.retain(|k| k != key);
        if self.scope_nodes.len() != before {
            self.scopes_changed();
        }
    }

    /// Drop the most recently added scope.
    pub fn pop_scope(&mut self) {
        if self.scope_nodes.pop().is_some() {
            self.scopes_changed();
        }
    }

    fn scopes_changed(&mut self) {
        // A specific scope implies its type, so the type filter gives way.
        if !self.scope_nodes.is_empty() {
            self.scope_filter = None;
        }
        self.cursor = 0;
        self.reindex();
    }

    /// Apply one navigator action.
    pub fn apply_nav(&mut self, action: NavAction) {
        let rows = self.tree.rows();
        let current = self.nav_cursor.checked_sub(1).and_then(|i| rows.get(i));
        match action {
            NavAction::Up => self.nav_cursor = self.nav_cursor.saturating_sub(1),
            NavAction::Down => self.nav_cursor = (self.nav_cursor + 1).min(rows.len()),
            NavAction::Top => self.nav_cursor = 0,
            NavAction::Bottom => self.nav_cursor = rows.len(),
            NavAction::Expand => {
                if let Some(row) = current
                    && row.has_children
                {
                    self.tree.set_expanded(&row.key, true);
                }
            }
            NavAction::Collapse => {
                let Some(row) = current else {
                    return;
                };
                if row.has_children && row.expanded {
                    self.tree.set_expanded(&row.key, false);
                } else if let Some(parent) = self.tree.parent(&row.key)
                    && let Some(i) = rows.iter().position(|r| &r.key == parent)
                {
                    self.nav_cursor = i + 1;
                }
            }
            NavAction::Select => self.select_scope_at(self.nav_cursor),
            NavAction::Toggle => self.toggle_scope_at(self.nav_cursor),
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
        // `role:name` and `in:name` words in the query narrow by role and by
        // scope. They are not part of the fuzzy search.
        let query = self.query.as_str();
        let value_of = |word: &'_ str, prefix: &str| -> Option<String> {
            let head = word.get(..prefix.len())?;
            head.eq_ignore_ascii_case(prefix)
                .then(|| word[prefix.len()..].to_string())
        };
        let mut roles: Vec<String> = self.role_filters.iter().map(|r| r.to_lowercase()).collect();
        let mut scopes = self.scope_nodes.clone();
        // A typed scope word narrows even when no scope has that name, so a
        // mistyped name shows nothing instead of everything.
        let mut typed_scope = false;
        let mut plain: Vec<&str> = Vec::new();
        for word in query.split_whitespace() {
            if let Some(role) = value_of(word, "role:") {
                if !role.is_empty() {
                    roles.push(role.to_lowercase());
                }
            } else if let Some(name) = value_of(word, "in:") {
                if !name.is_empty() {
                    typed_scope = true;
                    scopes.extend(self.tree.resolve(&name));
                }
            } else {
                plain.push(word);
            }
        }
        // A query of plain words goes to the search as typed.
        let ranked = if plain.len() == query.split_whitespace().count() {
            fuzzy::rank(&strings, query)
        } else {
            fuzzy::rank(&strings, &plain.join(" "))
        };
        // A scope named in the query says more than the scope type filter,
        // which gives way to it.
        self.filtered = match self.scope_filter.filter(|_| !typed_scope) {
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
        // Active scopes narrow the list to entitlements defined on those
        // scopes themselves, not on the scopes below them.
        if typed_scope || !scopes.is_empty() {
            self.filtered.retain(|&i| {
                self.entitlements.get(i).is_some_and(|e| {
                    scopes
                        .iter()
                        .any(|node| e.scope_type == node.scope && e.scope_id == node.id)
                })
            });
        }
        if !roles.is_empty() {
            self.filtered.retain(|&i| {
                self.entitlements.get(i).is_some_and(|e| {
                    let granted: Vec<String> = e.roles.iter().map(|r| r.to_lowercase()).collect();
                    roles
                        .iter()
                        .all(|want| granted.iter().any(|role| role.contains(want.as_str())))
                })
            });
        }
        // Marked view is an override: keep only rows the user has selected,
        // preserving the rank order computed above so the listing isn't
        // arbitrary.
        if self.only_marked {
            self.filtered.retain(|i| self.selected.contains(i));
        }
        if let Some(order) = self.sort {
            self.apply_sort(order);
        }
        if !self.filtered.is_empty() && self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len() - 1;
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn query(&self) -> &TextInput {
        &self.query
    }

    /// Every entitlement visible under the current refresh options.
    pub fn entitlements(&self) -> &[EntitlementRow] {
        &self.entitlements
    }

    /// Indices into [`BrowseState::entitlements`] that pass the current
    /// search, scope filter and marked view, in display order.
    pub fn filtered(&self) -> &[usize] {
        &self.filtered
    }

    /// Visible rows in display order, paired with their entitlement index.
    pub fn visible_rows(&self) -> impl Iterator<Item = (usize, &EntitlementRow)> {
        self.filtered
            .iter()
            .filter_map(|&idx| self.entitlements.get(idx).map(|e| (idx, e)))
    }

    /// Position of the highlighted row within [`BrowseState::filtered`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_marked(&self, idx: usize) -> bool {
        self.selected.contains(&idx)
    }

    pub fn marked_count(&self) -> usize {
        self.selected.len()
    }

    /// The entitlement under the cursor, if any.
    pub fn current(&self) -> Option<&EntitlementRow> {
        self.filtered
            .get(self.cursor)
            .and_then(|&idx| self.entitlements.get(idx))
    }

    pub fn scope_filter(&self) -> Option<Scope> {
        self.scope_filter
    }

    /// Scope filter for display: `all` or the scope name.
    pub fn scope_filter_label(&self) -> String {
        match self.scope_filter {
            None => "all".to_string(),
            Some(s) => s.to_string(),
        }
    }

    pub fn only_marked(&self) -> bool {
        self.only_marked
    }

    pub fn show_raw(&self) -> bool {
        self.show_raw
    }

    pub fn show_help(&self) -> bool {
        self.show_help
    }

    pub fn show_logs(&self) -> bool {
        self.show_logs
    }

    pub fn refresh_in_flight(&self) -> bool {
        self.refresh_in_flight
    }

    /// Latest one-line feedback for the status bar.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Clear a status message that has been showing for [`STATUS_SECS`].
    /// Frontends call this regularly, for example before each draw. A
    /// message stays while a refresh is running, since progress replaces it.
    pub fn expire_status(&mut self, now: i64) {
        if self.status.is_empty() || self.refresh_in_flight {
            self.status_seen = None;
            return;
        }
        match &self.status_seen {
            Some((text, since)) if *text == self.status => {
                if now - since >= STATUS_SECS {
                    self.status.clear();
                    self.status_seen = None;
                }
            }
            _ => self.status_seen = Some((self.status.clone(), now)),
        }
    }

    /// Age of the last completed entitlement scan, or `never`.
    pub fn cache_age_label(&self, now: i64) -> String {
        self.last_refresh
            .map(|t| format_age(now - t))
            .unwrap_or_else(|| "never".into())
    }

    /// Preview text for the entitlement under the cursor.
    pub fn preview(&self) -> String {
        match self.current() {
            None => "no entitlement selected".to_string(),
            Some(ent) => preview_text(ent, self.show_raw),
        }
    }

    /// Tracked grants that are pending or active. Terminal grants are hidden
    /// because their final outcome was shown in the request summary. A grant
    /// that ran out stays listed for [`EXPIRED_LINGER_SECS`] and then goes
    /// too, since an active grant is not polled again to see it end.
    pub fn live_grants(&self) -> impl Iterator<Item = &GrantRow> {
        let now = now_unix();
        self.tracked_grants.iter().filter(move |g| {
            let gone = g.state.is_active()
                && g.expires_at
                    .is_some_and(|exp| exp + EXPIRED_LINGER_SECS <= now);
            !g.state.is_terminal() && !gone
        })
    }

    /// The live grant to show for each entitlement. When an entitlement has
    /// several, one still running wins over an expired one, and a newer one
    /// over an older one.
    pub fn grants_by_entitlement(&self) -> HashMap<&str, &GrantRow> {
        let now = now_unix();
        let rank = |g: &GrantRow| (!grant_expired(g, now), g.created_at);
        let mut out: HashMap<&str, &GrantRow> = HashMap::new();
        for g in self.live_grants() {
            let slot = out.entry(g.entitlement_name.as_str()).or_insert(g);
            if rank(g) > rank(slot) {
                *slot = g;
            }
        }
        out
    }

    /// Number of forwarded approval events waiting in the queue.
    pub fn pending_approvals(&self) -> usize {
        self.approval_queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Warnings and errors logged since the logs view was last opened.
    pub fn unread_log_count(&self) -> usize {
        self.log_state.lock().map(|s| s.unread).unwrap_or(0)
    }

    /// Most recent warning or error, plus the full log file path.
    pub fn log_snapshot(&self) -> (Option<LogEntry>, String) {
        self.log_state
            .lock()
            .map(|s| (s.last.clone(), s.log_path.display().to_string()))
            .unwrap_or((None, String::new()))
    }
}

fn upsert_in_place(rows: &mut Vec<EntitlementRow>, row: EntitlementRow) {
    if let Some(existing) = rows.iter_mut().find(|r| r.name == row.name) {
        *existing = row;
    } else {
        rows.push(row);
    }
}

/// `[auto]` for entitlements without approvers, `[appr]` otherwise.
pub fn approval_badge(ent: &EntitlementRow) -> (&'static str, ApprovalMode) {
    if ent.approvers.is_empty() {
        ("[auto]", ApprovalMode::Auto)
    } else {
        ("[appr]", ApprovalMode::Manual)
    }
}

/// Short scope tag such as `proj:My Project`, falling back to the scope ID
/// when no display name is known.
pub fn scope_label(ent: &EntitlementRow) -> String {
    let name = ent.scope_display_name.as_deref().filter(|s| !s.is_empty());
    match name {
        Some(n) => format!("{}:{}", ent.scope_type.short(), n),
        None => format!("{}:{}", ent.scope_type.short(), ent.scope_id),
    }
}

/// Seconds a status message stays before it is cleared.
pub const STATUS_SECS: i64 = 8;

/// A role name without its `roles/` prefix, for display.
pub fn role_label(role: &str) -> &str {
    role.strip_prefix("roles/").unwrap_or(role)
}

/// Seconds a grant stays in the live list after it ran out, so its end is
/// seen before it goes.
pub const EXPIRED_LINGER_SECS: i64 = 60;

/// True for an active grant whose time is up.
pub fn grant_expired(g: &GrantRow, now: i64) -> bool {
    g.state.is_active() && g.expires_at.is_some_and(|exp| exp <= now)
}

/// Same as [`scope_label`] for a tracked grant.
pub fn grant_scope_label(g: &GrantRow) -> String {
    let name = g.scope_display_name.as_deref().filter(|s| !s.is_empty());
    match name {
        Some(n) => format!("{}:{}", g.scope_type.short(), n),
        None => format!("{}:{}", g.scope_type.short(), g.scope_id),
    }
}

/// One-letter grant badge: `A` for active, `P` for anything still pending.
pub fn strip_badge(state: &GrantState) -> (&'static str, GrantBadge) {
    if state.is_active() {
        ("A", GrantBadge::Active)
    } else {
        ("P", GrantBadge::Pending)
    }
}

/// Countdown for an active grant or the wait time of a pending one.
pub fn strip_timing(g: &GrantRow, now: i64) -> String {
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

/// Multi-line entitlement summary, or the pretty-printed raw JSON when
/// `show_raw` is set.
pub fn preview_text(ent: &EntitlementRow, show_raw: bool) -> String {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::app::approval_queue::{new_queue, push_event};
    use crate::backend::demo::DemoBackend;
    use crate::ipc::ApprovalEvent;
    use crate::logs::LogState;

    fn ent(scope: Scope, scope_id: &str, short: &str, approvers: &[&str]) -> EntitlementRow {
        EntitlementRow {
            name: format!(
                "{}/{scope_id}/locations/global/entitlements/{short}",
                scope.plural()
            ),
            scope_type: scope,
            scope_id: scope_id.into(),
            scope_display_name: None,
            short_name: short.into(),
            max_request_duration_secs: Some(3600),
            justification_required: false,
            approvers: approvers.iter().map(|s| (*s).into()).collect(),
            roles: vec!["roles/viewer".into()],
            raw_json: "{}".into(),
            fetched_at: 1_700_000_000,
            last_used_at: None,
        }
    }

    struct Fixture {
        state: BrowseState,
        queue: Queue,
        _dir: tempfile::TempDir,
    }

    /// Cache order (never used, so by scope type, scope id, short name):
    /// 0 fldr-admin, 1 org-audit, 2 logs-read, 3 sre-break-glass.
    async fn fixture(options: RefreshOptions) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open_at(&dir.path().join("test.db")).unwrap();
        for row in [
            ent(
                Scope::Project,
                "fake-proj-1",
                "sre-break-glass",
                &["group:oncall@example.test"],
            ),
            ent(Scope::Project, "fake-proj-1", "logs-read", &[]),
            ent(Scope::Folder, "111111", "fldr-admin", &[]),
            ent(
                Scope::Organization,
                "222222",
                "org-audit",
                &["group:audit@example.test"],
            ),
        ] {
            cache.upsert_entitlement(&row).unwrap();
        }
        let (_grant_tx, grant_rx) = mpsc::channel(4);
        let queue = new_queue();
        let log_state = Arc::new(std::sync::Mutex::new(LogState {
            last: None,
            unread: 2,
            log_path: PathBuf::from("gpam-test.log"),
        }));
        let state = BrowseState::new(
            "tester@example.test".into(),
            Arc::new(Mutex::new(cache)),
            Arc::new(DemoBackend::new()),
            options,
            grant_rx,
            queue.clone(),
            log_state,
        )
        .await;
        Fixture {
            state,
            queue,
            _dir: dir,
        }
    }

    fn names(state: &BrowseState) -> Vec<&str> {
        state
            .visible_rows()
            .map(|(_, e)| e.short_name.as_str())
            .collect()
    }

    #[tokio::test]
    async fn movement_clamps_to_list() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        assert_eq!(
            names(s),
            ["fldr-admin", "org-audit", "logs-read", "sre-break-glass"]
        );
        s.apply(Action::MoveDown(1));
        assert_eq!(s.cursor(), 1);
        s.apply(Action::MoveDown(10));
        assert_eq!(s.cursor(), 3);
        s.apply(Action::MoveUp(10));
        assert_eq!(s.cursor(), 0);
        s.apply(Action::JumpBottom);
        assert_eq!(s.current().unwrap().short_name, "sre-break-glass");
        s.apply(Action::JumpTop);
        assert_eq!(s.cursor(), 0);

        // Moving from the search box hands focus back to the list.
        s.apply(Action::FocusSearch);
        assert_eq!(s.focus(), Focus::Search);
        s.apply(Action::MoveDown(1));
        assert_eq!(s.focus(), Focus::List);
        assert_eq!(s.cursor(), 1);
    }

    fn grant(id: &str, state: GrantState, created_at: i64, expires_at: Option<i64>) -> GrantRow {
        let entitlement = "projects/fake-proj-1/locations/global/entitlements/logs-read";
        GrantRow {
            name: format!("{entitlement}/grants/{id}"),
            entitlement_name: entitlement.into(),
            entitlement_short_name: "logs-read".into(),
            scope_type: Scope::Project,
            scope_id: "fake-proj-1".into(),
            scope_display_name: None,
            short_id: id.into(),
            state,
            requested_duration_secs: 3600,
            justification: None,
            created_at,
            activated_at: None,
            expires_at,
            last_polled_at: None,
            raw_json: None,
        }
    }

    #[tokio::test]
    async fn expired_grants_linger_then_leave_the_live_list() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        let now = now_unix();
        s.tracked_grants = vec![
            grant("g-running", GrantState::Active, now - 100, Some(now + 600)),
            grant(
                "g-just-expired",
                GrantState::Active,
                now - 4000,
                Some(now - 5),
            ),
            grant(
                "g-long-expired",
                GrantState::Active,
                now - 9000,
                Some(now - EXPIRED_LINGER_SECS - 5),
            ),
            grant("g-denied", GrantState::Denied, now - 50, None),
        ];
        let live: Vec<&str> = s.live_grants().map(|g| g.short_id.as_str()).collect();
        assert_eq!(live, ["g-running", "g-just-expired"]);
        assert!(grant_expired(&s.tracked_grants[1], now));
        assert!(!grant_expired(&s.tracked_grants[0], now));

        // The running grant stands for the entitlement, not the expired one.
        let by_entitlement = s.grants_by_entitlement();
        assert_eq!(by_entitlement.len(), 1);
        let shown = by_entitlement
            .get("projects/fake-proj-1/locations/global/entitlements/logs-read")
            .unwrap();
        assert_eq!(shown.short_id, "g-running");

        // Requesting the entitlement again is refused with a reason.
        s.set_query("logs-read");
        assert_eq!(names(s), ["logs-read"]);
        assert!(s.apply(Action::Submit).is_none());
        assert!(s.status().contains("logs-read: already active"));
        assert_eq!(s.marked_count(), 0);

        // Other entitlements still go through, without the held one.
        s.set_query("");
        s.set_marks_at(&[0, 1, 2, 3], true);
        let Some(BrowseExit::Request(chosen)) = s.apply(Action::Submit) else {
            panic!("expected a request");
        };
        assert_eq!(chosen.len(), 3);
        assert!(chosen.iter().all(|e| e.short_name != "logs-read"));
    }

    #[tokio::test]
    async fn role_filters_and_role_words_narrow_the_list() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        // Order: 0 fldr-admin, 1 org-audit, 2 logs-read, 3 sre-break-glass.
        s.entitlements[0].roles = vec!["roles/resourcemanager.folderAdmin".into()];
        s.entitlements[1].roles = vec!["roles/logging.viewer".into(), "roles/viewer".into()];
        s.entitlements[2].roles = vec!["roles/logging.viewer".into()];
        s.entitlements[3].roles = vec!["roles/owner".into()];

        // Several roles match, so the typed text leads and stands for all.
        assert_eq!(
            s.role_candidates("viewer"),
            ["viewer", "roles/viewer", "roles/logging.viewer"]
        );
        assert_eq!(s.role_candidates("roles/own"), ["roles/owner"]);
        assert!(s.role_candidates("nothing-like-it").is_empty());
        assert_eq!(role_label("roles/owner"), "owner");

        s.add_role_filter("logging");
        s.add_role_filter("LOGGING");
        assert_eq!(s.role_filters(), ["logging"]);
        assert_eq!(names(s), ["org-audit", "logs-read"]);
        // Every filter has to match a role of the entitlement.
        s.add_role_filter("roles/viewer");
        assert_eq!(names(s), ["org-audit"]);
        s.pop_role_filter();
        s.remove_role_filter("logging");
        assert_eq!(names(s).len(), 4);

        // A role word in the query narrows the same way, and the other
        // words still search by name.
        s.set_query("role:logging audit");
        assert_eq!(names(s), ["org-audit"]);
        s.set_query("role:");
        assert_eq!(names(s).len(), 4);

        // A scope word in the query narrows to the scope it names.
        s.set_query("in:fake-proj-1");
        assert_eq!(names(s), ["logs-read", "sre-break-glass"]);
        s.set_query("in:fake-proj-1 role:owner");
        assert_eq!(names(s), ["sre-break-glass"]);
        s.set_query("in:no-such-scope-anywhere");
        assert!(names(s).is_empty());
        s.set_query("in:");
        assert_eq!(names(s).len(), 4);

        // Esc drops the query first, then the role filters.
        s.set_query("logs");
        s.add_role_filter("logging");
        assert_eq!(names(s), ["logs-read"]);
        assert!(s.apply(Action::ClearSelection).is_none());
        assert_eq!(names(s), ["org-audit", "logs-read"]);
        assert!(s.apply(Action::ClearSelection).is_none());
        assert!(s.role_filters().is_empty());
    }

    #[tokio::test]
    async fn status_message_expires() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.status = "left out 2 that already have a grant".into();
        s.expire_status(1_000);
        s.expire_status(1_000 + STATUS_SECS - 1);
        assert!(!s.status().is_empty());
        s.expire_status(1_000 + STATUS_SECS);
        assert!(s.status().is_empty());

        // A new message starts its own clock.
        s.status = "first".into();
        s.expire_status(2_000);
        s.status = "second".into();
        s.expire_status(2_000 + STATUS_SECS);
        assert_eq!(s.status(), "second");
        s.expire_status(2_000 + 2 * STATUS_SECS);
        assert!(s.status().is_empty());
    }

    #[tokio::test]
    async fn column_sort_steps_and_keeps_cursor() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.apply(Action::JumpBottom);
        assert_eq!(s.current().unwrap().short_name, "sre-break-glass");

        s.toggle_sort(SortColumn::Name);
        assert_eq!(
            names(s),
            ["fldr-admin", "logs-read", "org-audit", "sre-break-glass"]
        );
        s.toggle_sort(SortColumn::Name);
        assert_eq!(
            names(s),
            ["sre-break-glass", "org-audit", "logs-read", "fldr-admin"]
        );
        // The cursor follows the entitlement, not the position.
        assert_eq!(s.current().unwrap().short_name, "sre-break-glass");
        s.toggle_sort(SortColumn::Name);
        assert_eq!(s.sort(), None);
        assert_eq!(
            names(s),
            ["fldr-admin", "org-audit", "logs-read", "sre-break-glass"]
        );

        // Auto-approved entitlements come before those needing approval,
        // and equal rows keep their previous order.
        s.toggle_sort(SortColumn::Approval);
        assert_eq!(
            names(s),
            ["fldr-admin", "logs-read", "org-audit", "sre-break-glass"]
        );
        s.reverse_sort();
        assert_eq!(
            names(s),
            ["org-audit", "sre-break-glass", "fldr-admin", "logs-read"]
        );

        // Cycling walks every column and ends back at the search ranking.
        s.toggle_sort(SortColumn::Approval);
        assert_eq!(s.sort(), None);
        for column in SortColumn::ALL {
            s.cycle_sort();
            assert_eq!(s.sort().map(|o| o.column), Some(column));
        }
        s.cycle_sort();
        assert_eq!(s.sort(), None);
    }

    #[tokio::test]
    async fn marking_marked_view_and_layered_escape() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.apply(Action::ToggleSelect);
        s.apply(Action::ToggleSelect);
        assert!(s.is_marked(0) && s.is_marked(1));
        assert_eq!(s.cursor(), 2);

        s.apply(Action::ToggleMarkedView);
        assert_eq!(s.status(), "view: marked (2)");
        assert_eq!(names(s), ["fldr-admin", "org-audit"]);
        assert_eq!(s.cursor(), 1);

        // Unmarking in marked view drops the row right away.
        s.apply(Action::ToggleSelect);
        assert_eq!(names(s), ["fldr-admin"]);

        // Esc peels one layer per press: marked view, then marks, then quit.
        assert!(s.apply(Action::ClearSelection).is_none());
        assert!(!s.only_marked());
        assert_eq!(names(s).len(), 4);
        assert!(s.apply(Action::ClearSelection).is_none());
        assert_eq!(s.marked_count(), 0);
        assert!(matches!(
            s.apply(Action::ClearSelection),
            Some(BrowseExit::Quit)
        ));
    }

    #[tokio::test]
    async fn marking_several_rows_and_clearing_marks() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.set_marks_at(&[0, 2, 3, 99], true);
        assert!(s.is_marked(0) && !s.is_marked(1));
        assert_eq!(s.marked_count(), 3);
        assert_eq!(s.cursor(), 0);

        s.set_marks_at(&[2, 1], false);
        assert_eq!(s.marked_count(), 2);

        // Unmarking in marked view drops the rows from the list.
        s.apply(Action::ToggleMarkedView);
        assert_eq!(names(s).len(), 2);
        s.set_marks_at(&[0], false);
        assert_eq!(names(s).len(), 1);
        s.clear_marks();
        assert!(names(s).is_empty());
    }

    #[tokio::test]
    async fn navigator_narrows_to_chosen_scopes() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        // No scope rows are cached in this fixture, so every scope is a
        // top-level line: the organization, the folder, then the project.
        let labels: Vec<String> = s.scope_rows().iter().map(|r| r.label.clone()).collect();
        assert_eq!(labels, ["222222", "111111", "fake-proj-1"]);
        assert_eq!(s.nav_cursor(), 0);
        let project = ScopeKey::new(Scope::Project, "fake-proj-1");
        let folder = ScopeKey::new(Scope::Folder, "111111");

        s.apply_nav(NavAction::Bottom);
        s.apply_nav(NavAction::Select);
        assert_eq!(s.scope_nodes(), std::slice::from_ref(&project));
        assert_eq!(s.scope_label(&project), Some("fake-proj-1"));
        assert_eq!(names(s), ["logs-read", "sre-break-glass"]);

        // The type filter and the chosen scopes replace each other, so
        // neither can hide the other's rows.
        s.apply(Action::CycleScopeFilter);
        assert!(s.scope_nodes().is_empty());
        assert_eq!(s.scope_filter(), Some(Scope::Project));
        s.apply(Action::CycleScopeFilter);
        assert_eq!(names(s), ["fldr-admin"]);
        s.apply_nav(NavAction::Bottom);
        s.apply_nav(NavAction::Select);
        assert_eq!(s.scope_filter(), None);
        assert_eq!(names(s), ["logs-read", "sre-break-glass"]);

        // A second scope widens the list, and toggling it again narrows it.
        s.apply_nav(NavAction::Up);
        s.apply_nav(NavAction::Toggle);
        assert_eq!(s.scope_nodes(), [project.clone(), folder.clone()]);
        assert_eq!(names(s), ["fldr-admin", "logs-read", "sre-break-glass"]);
        s.apply_nav(NavAction::Toggle);
        assert_eq!(s.scope_nodes(), std::slice::from_ref(&project));

        // Scopes can also be added by name and dropped from the end.
        let resolved = s.resolve_scope("111").unwrap();
        assert_eq!(resolved, folder);
        s.add_scope(resolved.clone());
        s.add_scope(resolved);
        s.add_scope(ScopeKey::new(Scope::Project, "fake-missing"));
        assert_eq!(s.scope_nodes().len(), 2);
        s.pop_scope();
        s.remove_scope(&folder);
        assert_eq!(s.scope_nodes(), [project]);

        // Esc drops the chosen scopes before the marks.
        s.apply(Action::ToggleSelect);
        assert!(s.apply(Action::ClearSelection).is_none());
        assert!(s.scope_nodes().is_empty());
        assert_eq!(s.nav_cursor(), 0);
        assert_eq!(s.marked_count(), 1);
        assert_eq!(names(s).len(), 4);

        s.apply_nav(NavAction::Down);
        s.apply_nav(NavAction::Up);
        s.apply_nav(NavAction::Up);
        assert_eq!(s.nav_cursor(), 0);
        s.select_scope_at(2);
        assert_eq!(names(s), ["fldr-admin"]);
        s.select_scope_at(0);
        assert_eq!(names(s).len(), 4);
    }

    #[tokio::test]
    async fn scope_filter_cycles_through_all_scopes() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.apply(Action::CycleScopeFilter);
        assert_eq!(s.status(), "scope filter: project");
        assert_eq!(names(s), ["logs-read", "sre-break-glass"]);
        s.apply(Action::CycleScopeFilter);
        assert_eq!(names(s), ["fldr-admin"]);
        s.apply(Action::CycleScopeFilter);
        assert_eq!(s.scope_filter(), Some(Scope::Organization));
        assert_eq!(names(s), ["org-audit"]);
        s.apply(Action::CycleScopeFilter);
        assert_eq!(s.status(), "scope filter: all");
        assert_eq!(s.scope_filter_label(), "all");
        assert_eq!(names(s).len(), 4);
    }

    #[tokio::test]
    async fn search_filters_and_submit_takes_cursor_row() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.apply(Action::FocusSearch);
        for c in "glass".chars() {
            s.apply(Action::SearchInsert(c));
        }
        assert_eq!(names(s), ["sre-break-glass"]);
        assert!(s.preview().starts_with("name:    sre-break-glass\n"));

        // Esc in search returns to the list. A second Esc clears the query.
        s.apply(Action::FocusList);
        assert!(s.apply(Action::ClearSelection).is_none());
        assert!(s.query().is_empty());
        assert_eq!(names(s).len(), 4);

        s.apply(Action::SearchInsert('x'));
        s.apply(Action::SearchBackspace);
        s.apply(Action::JumpBottom);
        let Some(BrowseExit::Request(chosen)) = s.apply(Action::Submit) else {
            panic!("expected a request");
        };
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].short_name, "sre-break-glass");
        assert_eq!(s.marked_count(), 0);
    }

    #[tokio::test]
    async fn overlays_logs_and_approval_queue() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.apply(Action::Help);
        assert!(s.overlay_open());
        s.close_overlays();
        assert_eq!(s.unread_log_count(), 2);
        s.apply(Action::ShowLogs);
        assert!(s.show_logs());
        assert_eq!(s.unread_log_count(), 0);
        s.close_overlays();

        assert!(s.apply(Action::OpenApprovalQueue).is_none());
        assert_eq!(s.status(), "no pending approvals");
        push_event(
            &f.queue,
            ApprovalEvent {
                name: "projects/fake-proj-1/locations/global/entitlements/logs-read/grants/g-1"
                    .into(),
                source: None,
            },
        );
        assert_eq!(s.pending_approvals(), 1);
        assert!(matches!(
            s.apply(Action::OpenApprovalQueue),
            Some(BrowseExit::OpenApprovalQueue)
        ));
    }

    #[tokio::test]
    async fn set_query_filters_and_pointer_helpers() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        s.set_query("glass");
        assert_eq!(s.query().as_str(), "glass");
        assert_eq!(names(s), ["sre-break-glass"]);
        s.set_query("");
        assert_eq!(names(s).len(), 4);

        s.set_cursor(2);
        assert_eq!(s.cursor(), 2);
        s.set_cursor(99);
        assert_eq!(s.cursor(), 3);

        // Pointer marking keeps the cursor where it is.
        s.toggle_mark_at(1);
        assert!(s.is_marked(1));
        assert_eq!(s.cursor(), 3);
        s.apply(Action::ToggleMarkedView);
        assert_eq!(names(s), ["org-audit"]);
        s.toggle_mark_at(0);
        assert!(names(s).is_empty());
        s.toggle_mark_at(5);
        assert_eq!(s.marked_count(), 0);
    }

    #[tokio::test]
    async fn try_next_update_drains_without_waiting() {
        let mut f = fixture(RefreshOptions::default()).await;
        let s = &mut f.state;
        assert!(s.try_next_update().is_none());
        s.refresh_tx
            .try_send(RefreshEvent::Finished {
                total: 4,
                error: None,
            })
            .unwrap();
        let update = s.try_next_update().expect("queued refresh event");
        s.handle_update(update).await;
        assert_eq!(s.status(), "refresh complete: 4 entitlements");
        assert!(s.try_next_update().is_none());
    }

    #[tokio::test]
    async fn refresh_events_respect_scope_options() {
        let options = RefreshOptions {
            folders: false,
            ..RefreshOptions::default()
        };
        let mut f = fixture(options).await;
        let s = &mut f.state;
        assert_eq!(names(s), ["org-audit", "logs-read", "sre-break-glass"]);

        let refresh = |ev| BrowseUpdate(UpdateKind::Refresh(Box::new(ev)));
        s.handle_update(refresh(RefreshEvent::EntitlementUpserted(ent(
            Scope::Folder,
            "111111",
            "hidden-folder-ent",
            &[],
        ))))
        .await;
        s.handle_update(refresh(RefreshEvent::EntitlementUpserted(ent(
            Scope::Project,
            "fake-proj-2",
            "new-ent",
            &[],
        ))))
        .await;
        assert_eq!(names(s).len(), 4);
        assert_eq!(names(s)[3], "new-ent");

        // Finished re-reads the cache, which never saw the streamed row.
        s.handle_update(refresh(RefreshEvent::Finished {
            total: 3,
            error: None,
        }))
        .await;
        assert_eq!(s.status(), "refresh complete: 3 entitlements");
        assert!(!s.refresh_in_flight());
        assert_eq!(names(s).len(), 3);
        assert_ne!(s.cache_age_label(now_unix()), "never");
    }
}
