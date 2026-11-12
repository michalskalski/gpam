use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use crate::backend::DynBackend;
use crate::cache::{Cache, EntitlementRow, GrantRow, meta_keys, now_unix};
use crate::fuzzy;
use crate::gcp::Scope;
use crate::gcp::grants::GrantState;
use crate::logs::{LogEntry, SharedLogState};
use crate::poller::GrantUpdate;
use crate::refresh::{self, RefreshEvent, RefreshOptions};

use super::action::{Action, Focus, SearchMove};
use super::approval_queue::Queue;
use super::format::format_age;
use super::text_input::TextInput;

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

    refresh_in_flight: bool,
    last_refresh: Option<i64>,
    status: String,
    show_raw: bool,
    show_help: bool,
    show_logs: bool,
    only_marked: bool,

    refresh_rx: mpsc::Receiver<RefreshEvent>,
    refresh_tx: mpsc::Sender<RefreshEvent>,

    tracked_grants: Vec<GrantRow>,
    grant_rx: mpsc::Receiver<GrantUpdate>,

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
            refresh_in_flight: false,
            last_refresh,
            status: String::new(),
            show_raw: false,
            show_help: false,
            show_logs: false,
            only_marked: false,
            refresh_rx,
            refresh_tx,
            tracked_grants,
            grant_rx,
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
        let cache = self.cache.lock().await;
        self.tracked_grants = cache.list_tracked_grants(now_unix()).unwrap_or_default();
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
                // Clear the selection so a return visit starts fresh.
                self.selected.clear();
                return Some(BrowseExit::Request(chosen));
            }
            Action::Refresh => self.start_refresh(),
            Action::FocusSearch => self.focus = Focus::Search,
            Action::FocusList => self.focus = Focus::List,
            Action::ClearSelection => {
                // One layer per press: marked view, then filter, then
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
    /// because their final outcome was shown in the request summary.
    pub fn live_grants(&self) -> impl Iterator<Item = &GrantRow> {
        self.tracked_grants
            .iter()
            .filter(|g| !g.state.is_terminal())
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
