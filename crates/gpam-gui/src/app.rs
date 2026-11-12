//! The eframe application: owns the core [`App`], routes keyboard and
//! pointer input to core actions, runs network calls on the tokio runtime,
//! and draws the current view.

#[cfg(feature = "screenshot")]
mod scene;
#[cfg(feature = "screenshot")]
pub use scene::SceneKind;

use std::collections::BTreeSet;
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui::{self, Event, Id, RawInput};
use gpam_core::app::App;
use gpam_core::app::action::{Action, Focus, SearchMove};
use gpam_core::app::approval_queue::{QueueAction, QueueStep, QueueView};
use gpam_core::app::approve::{ApproveAction, ApproveModal, ApproveOutcome, ApproveStep, Step};
use gpam_core::app::browse::{BrowseExit, SortColumn};
use gpam_core::app::request::{
    self, Field, RequestAction, RequestFlow, RequestResult, RequestStep,
};
use gpam_core::app::scope_tree::{NavAction, ScopeKey, split_role_token, split_scope_token};
use gpam_core::gcp::grants::{GrantDetails, GrantState, approval_console_url};
use tokio::runtime::Handle;

use crate::keymap::{self, GrantKey, Key, NavKey, OverlayKey, PanelKey};
use crate::{approve, browse, queue, request as request_view};

/// How often to redraw without input, so countdowns tick and background
/// updates show up.
const IDLE_REPAINT: Duration = Duration::from_millis(250);

/// Upper bound on background updates applied per frame, so a large refresh
/// cannot stall a frame. The rest are applied on the next frames.
const UPDATES_PER_FRAME: usize = 256;

/// Ids of the text fields, used to move keyboard focus between them and the
/// lists.
pub mod ids {
    use eframe::egui::Id;

    pub fn search() -> Id {
        Id::unique("gpam-search")
    }
    pub fn duration() -> Id {
        Id::unique("gpam-duration")
    }
    pub fn justification() -> Id {
        Id::unique("gpam-justification")
    }
    pub fn reason() -> Id {
        Id::unique("gpam-reason")
    }

    pub fn is_text_field(id: Id) -> bool {
        [search(), duration(), justification(), reason()].contains(&id)
    }
}

/// Everything a key press or a pointer gesture can ask for. Keyboard and
/// pointer input both end up in [`GuiApp::apply`].
#[derive(Debug, Clone)]
pub enum Intent {
    Browse(Action),
    SetCursor(usize),
    /// Highlight the rows between two positions, replacing the selection.
    SelectRange {
        from: usize,
        to: usize,
    },
    /// Highlight from the selection anchor to this row.
    ExtendSelection(usize),
    /// Add this row to the selection, or take it out.
    ToggleSelected(usize),
    ToggleMark(usize),
    ClearMarks,
    ScrollToCursor,
    /// Show or hide the scope navigator.
    ToggleNavigator,
    /// Step the theme: auto, light, dark.
    CycleTheme,
    /// Show or hide the details panel.
    ToggleDetails,
    /// Open or close the grants panel.
    ToggleGrants,
    /// Give keyboard focus to the grants panel, or back to the list.
    FocusGrants(bool),
    /// Give keyboard focus to the entitlement table.
    FocusTable,
    /// Select a grant card by grant name. The details panel shows it.
    SelectGrant(String),
    /// Select the grant card this many places on.
    GrantMove(isize),
    /// Select the first grant card, or the last.
    GrantEdge {
        last: bool,
    },
    /// Copy the console approval link of the selected grant.
    CopyGrantLink,
    /// Empty the typed search text. Scope labels stay.
    ClearQuery,
    /// Step the order of one column: ascending, descending, off.
    Sort(SortColumn),
    /// Order the list by the next column.
    CycleSort,
    /// Flip the direction of the column order.
    ReverseSort,
    /// Give keyboard focus to the scope navigator, or back to the list.
    FocusNavigator(bool),
    Nav(NavAction),
    /// Narrow the list to the scope on a navigator line and return to the
    /// list.
    NavSelect(usize),
    /// Add the scope on a navigator line to the active scopes, or take it
    /// out.
    NavToggle(usize),
    NavToggleExpand(usize),
    /// Turn the `in:name` being typed in the search field into a scope.
    CompleteScopeToken,
    /// Step the suggestion for the `in:name` being typed to the next or
    /// previous matching scope.
    CycleScopeToken(isize),
    /// Drop the last label in the search field: a role, or else the most
    /// recently added scope.
    PopScope,
    RemoveScope(ScopeKey),
    RemoveRole(String),
    /// Narrow the list to entitlements granting this role.
    AddRole(String),
    OpenRow(usize),
    CloseOverlay,
    /// Scroll the open overlay by this many points.
    ScrollOverlay(f32),
    Request(RequestAction),
    DismissSummary,
    Queue(QueueAction),
    QueueCursor(usize),
    QueueOpenRow(usize),
    Approve(ApproveAction),
    Quit,
}

pub enum View {
    Browse,
    Request(RequestScreen),
    Queue(QueueScreen),
}

/// A place keyboard focus can be in the browse view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Table,
    Tree,
    Grants,
}

pub struct RequestScreen {
    pub flow: RequestFlow,
    /// A grant creation call is in flight. Input is ignored meanwhile.
    pub busy: bool,
    /// Every entitlement was handled and some failed, so the summary shows.
    pub summary: bool,
}

pub struct QueueScreen {
    pub view: QueueView,
    pub approve: Option<ApproveScreen>,
    /// Scroll the queue table to the cursor on the next draw.
    pub scroll_to_cursor: bool,
}

pub enum ApproveScreen {
    Loading(String),
    Ready(ApproveModal),
    /// The modal is with a background task. The details are kept for display.
    Submitting {
        details: Option<GrantDetails>,
        step: Step,
    },
}

/// Results of background network calls, sent back to the UI thread.
enum Done {
    Granted(RequestResult),
    ApproveLoaded(ApproveModal),
    ApproveSubmitted(ApproveModal),
}

pub struct GuiApp {
    app: App,
    rt: Handle,
    view: View,
    done_tx: mpsc::Sender<Done>,
    done_rx: mpsc::Receiver<Done>,
    /// The text field this app last asked egui to focus.
    pushed_focus: Option<Id>,
    /// Scroll the entitlement table to the cursor on the next draw.
    scroll_to_cursor: bool,
    /// Rows highlighted with the pointer, as indices into the entitlement
    /// list. A mark set on one of them applies to all of them.
    selection: BTreeSet<usize>,
    /// Row position a Shift-click extends the selection from.
    select_anchor: usize,
    /// The search field. Its text can hold an `in:name` scope token that
    /// core's query does not.
    search: browse::SearchInput,
    /// The scope navigator is shown.
    nav_open: bool,
    /// List keys go to the scope navigator.
    nav_focus: bool,
    /// Scroll the navigator to its cursor on the next draw.
    nav_scroll: bool,
    /// Scopes kept with Space in the navigator. The list shows these plus
    /// the scope under the navigator cursor.
    nav_kept: Vec<ScopeKey>,
    /// Points the open overlay still has to scroll, from the keyboard.
    overlay_scroll: f32,
    /// The window was too narrow for the navigator on the last draw.
    narrow: bool,
    /// The scripted scene for the README screenshot, when asked for.
    #[cfg(feature = "screenshot")]
    scene: Option<scene::Scene>,
    /// List keys currently go to the grants panel.
    grant_focus: bool,
    /// Name of the selected grant. While set, the details panel shows this
    /// grant and its entitlement instead of the row under the cursor.
    grant_selected: Option<String>,
    /// Scroll the grants panel to the selected card on the next draw.
    grant_scroll: bool,
    /// The details panel is shown.
    details_open: bool,
    /// A plain click on a row opens the details panel. Cleared once the
    /// panel is hidden by hand.
    details_on_click: bool,
    /// The grants panel shows its table. Its header is always there.
    grants_open: bool,
}

impl GuiApp {
    pub fn new(mut app: App, rt: Handle) -> Self {
        rt.block_on(app.browse.activate());
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            app,
            rt,
            view: View::Browse,
            done_tx,
            done_rx,
            pushed_focus: None,
            scroll_to_cursor: true,
            selection: BTreeSet::new(),
            select_anchor: 0,
            search: browse::SearchInput::default(),
            nav_open: true,
            nav_focus: false,
            nav_scroll: false,
            nav_kept: Vec::new(),
            overlay_scroll: 0.0,
            narrow: false,
            #[cfg(feature = "screenshot")]
            scene: None,
            grant_focus: false,
            grant_selected: None,
            grant_scroll: false,
            details_open: false,
            details_on_click: true,
            grants_open: true,
        }
    }

    /// The text field that should hold keyboard focus according to core
    /// state, or `None` when keys belong to a list or a button row.
    fn desired_focus(&self) -> Option<Id> {
        match &self.view {
            View::Browse => {
                let state = &self.app.browse;
                (!state.overlay_open() && state.focus() == Focus::Search).then(ids::search)
            }
            View::Request(r) if r.busy || r.summary => None,
            View::Request(r) => r.flow.current().and_then(|modal| match modal.field() {
                Field::Duration => Some(ids::duration()),
                Field::Justification => modal
                    .entitlement()
                    .justification_required
                    .then(ids::justification),
            }),
            View::Queue(q) => match &q.approve {
                Some(ApproveScreen::Ready(modal)) if matches!(modal.step(), Step::Reason(_)) => {
                    Some(ids::reason())
                }
                _ => None,
            },
        }
    }

    /// Map one key to an intent for the current view. `None` leaves the key
    /// to egui, which only matters while a text field has focus.
    /// Scope on the navigator line under the cursor. The "all scopes" line
    /// has none.
    fn nav_cursor_scope(&self) -> Option<ScopeKey> {
        let state = &self.app.browse;
        let i = state.nav_cursor().checked_sub(1)?;
        state.scope_rows().into_iter().nth(i).map(|row| row.key)
    }

    /// Select another grant card. `pick` gets the current position and the
    /// number of cards and returns the new position.
    fn move_grant(&mut self, pick: impl Fn(usize, usize) -> usize) {
        let names: Vec<String> = browse::ordered_grants(&self.app.browse)
            .iter()
            .map(|g| g.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        let at = self
            .grant_selected
            .as_ref()
            .and_then(|name| names.iter().position(|n| n == name))
            .unwrap_or(0);
        self.grant_selected = names.get(pick(at, names.len())).cloned();
        self.grant_scroll = true;
    }

    /// Narrow the list to the kept scopes plus the one under the navigator
    /// cursor, so moving through the tree shows what each scope holds.
    fn show_nav_scopes(&mut self) {
        let mut scopes = self.nav_kept.clone();
        if let Some(key) = self.nav_cursor_scope()
            && !scopes.contains(&key)
        {
            scopes.push(key);
        }
        if scopes != self.app.browse.scope_nodes() {
            self.app.browse.set_scopes(scopes);
            self.selection.clear();
            self.select_anchor = 0;
            self.scroll_to_cursor = true;
        }
    }

    /// The intent that moves keyboard focus one stop along the focus ring.
    /// The ring is the table plus the side panels that are open, so a
    /// hidden navigator and a closed or empty grants panel are skipped.
    fn ring_step(&self, from: Stop, forward: bool) -> Intent {
        let mut ring = vec![Stop::Table];
        if self.nav_open && !self.narrow {
            ring.push(Stop::Tree);
        }
        if self.grants_open && self.app.browse.live_grants().next().is_some() {
            ring.push(Stop::Grants);
        }
        let at = ring.iter().position(|s| *s == from).unwrap_or(0);
        let step = if forward { 1 } else { ring.len() - 1 };
        match ring[(at + step) % ring.len()] {
            Stop::Table => Intent::FocusTable,
            Stop::Tree => Intent::FocusNavigator(true),
            Stop::Grants => Intent::FocusGrants(true),
        }
    }

    /// Run the scripted demo scene and save a picture of the window to
    /// `path`.
    #[cfg(feature = "screenshot")]
    pub fn with_screenshot(mut self, path: std::path::PathBuf, kind: SceneKind) -> Self {
        self.scene = Some(scene::Scene::new(path, kind));
        self
    }

    fn key_intent(&self, key: Key) -> Option<Intent> {
        match &self.view {
            View::Browse => {
                let state = &self.app.browse;
                if state.overlay_open() {
                    return Some(match keymap::overlay(key) {
                        OverlayKey::Quit => Intent::Quit,
                        OverlayKey::Close => Intent::CloseOverlay,
                        OverlayKey::Scroll(by) => Intent::ScrollOverlay(by),
                    });
                }
                if self.nav_open && self.nav_focus {
                    let cursor = state.nav_cursor();
                    return keymap::navigator(key).map(|k| match k {
                        NavKey::Move(action) => Intent::Nav(action),
                        // With scopes kept, Enter goes to the list as it is
                        // shown. Without, it narrows to the scope alone.
                        NavKey::Select if !self.nav_kept.is_empty() => {
                            Intent::FocusNavigator(false)
                        }
                        NavKey::Select => Intent::NavSelect(cursor),
                        NavKey::Back => Intent::FocusNavigator(false),
                        NavKey::NextPanel => self.ring_step(Stop::Tree, true),
                        NavKey::PrevPanel => self.ring_step(Stop::Tree, false),
                        NavKey::Hide => Intent::ToggleNavigator,
                        NavKey::Search => Intent::Browse(Action::FocusSearch),
                        NavKey::ClearQuery => Intent::ClearQuery,
                        NavKey::ToggleDetails => Intent::ToggleDetails,
                        NavKey::ToggleGrants => Intent::ToggleGrants,
                        NavKey::Help => Intent::Browse(Action::Help),
                        NavKey::Quit => Intent::Quit,
                    });
                }
                if self.grant_focus {
                    return keymap::grants(key).map(|k| match k {
                        GrantKey::Move(step) => Intent::GrantMove(step),
                        GrantKey::First => Intent::GrantEdge { last: false },
                        GrantKey::Last => Intent::GrantEdge { last: true },
                        GrantKey::CopyLink => Intent::CopyGrantLink,
                        GrantKey::Back => Intent::FocusGrants(false),
                        GrantKey::NextPanel => self.ring_step(Stop::Grants, true),
                        GrantKey::PrevPanel => self.ring_step(Stop::Grants, false),
                        GrantKey::Hide => Intent::ToggleGrants,
                        GrantKey::ToggleDetails => Intent::ToggleDetails,
                        GrantKey::ClearQuery => Intent::ClearQuery,
                        GrantKey::Help => Intent::Browse(Action::Help),
                        GrantKey::Quit => Intent::Quit,
                    });
                }
                if state.focus() == Focus::List
                    && let Some(panel) = keymap::panel(key)
                {
                    return Some(match panel {
                        PanelKey::NextPanel => self.ring_step(Stop::Table, true),
                        PanelKey::ToggleNavigator => Intent::ToggleNavigator,
                        PanelKey::CycleTheme => Intent::CycleTheme,
                        PanelKey::ToggleDetails => Intent::ToggleDetails,
                        PanelKey::ToggleGrants => Intent::ToggleGrants,
                        PanelKey::CycleSort => Intent::CycleSort,
                        PanelKey::ReverseSort => Intent::ReverseSort,
                        PanelKey::ClearQuery => Intent::ClearQuery,
                        PanelKey::PrevPanel => self.ring_step(Stop::Table, false),
                    });
                }
                if state.focus() == Focus::Search {
                    // While an `in:name` token is being typed, Tab steps
                    // through the matching scopes and Enter takes the one
                    // shown. Backspace in an empty field drops the last
                    // scope label.
                    let typing_scope = split_scope_token(&self.search.text).token.is_some()
                        || split_role_token(&self.search.text).token.is_some();
                    if typing_scope {
                        match key {
                            Key::Enter => return Some(Intent::CompleteScopeToken),
                            Key::Tab => return Some(Intent::CycleScopeToken(1)),
                            Key::BackTab => return Some(Intent::CycleScopeToken(-1)),
                            _ => {}
                        }
                    }
                    // Ctrl-L clears from anywhere. Ctrl-U is the readline
                    // key for it and only means that in the field.
                    if matches!(key, Key::Ctrl('l' | 'u')) {
                        return Some(Intent::ClearQuery);
                    }
                    if key == Key::Backspace
                        && self.search.text.is_empty()
                        && !(state.scope_nodes().is_empty() && state.role_filters().is_empty())
                    {
                        return Some(Intent::PopScope);
                    }
                }
                keymap::browse(state.focus(), key).map(Intent::Browse)
            }
            View::Request(r) if r.busy => None,
            View::Request(r) if r.summary => {
                keymap::dismiss_summary(key).then_some(Intent::DismissSummary)
            }
            View::Request(_) => keymap::request(key).map(Intent::Request),
            View::Queue(q) => match &q.approve {
                None => keymap::queue(key).map(Intent::Queue),
                Some(ApproveScreen::Ready(modal)) => {
                    keymap::approve(modal.step(), key).map(Intent::Approve)
                }
                Some(_) => None,
            },
        }
    }

    /// Apply one intent from the keyboard or the pointer.
    pub fn apply(&mut self, ctx: &egui::Context, intent: Intent) {
        // Acting on the table or the navigator takes the keyboard away from
        // the grants panel, and picking a table row lets the details panel
        // follow the table again.
        let on_table = matches!(
            intent,
            Intent::SetCursor(_)
                | Intent::SelectRange { .. }
                | Intent::ExtendSelection(_)
                | Intent::ToggleSelected(_)
                | Intent::OpenRow(_)
        );
        if on_table {
            self.grant_selected = None;
        }
        if on_table
            || matches!(
                intent,
                Intent::NavSelect(_) | Intent::NavToggle(_) | Intent::FocusNavigator(true)
            )
        {
            self.grant_focus = false;
        }
        match intent {
            Intent::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Intent::Browse(action) => self.apply_browse(ctx, action),
            Intent::SetCursor(pos) => {
                self.nav_focus = false;
                self.selection.clear();
                self.select_anchor = pos;
                self.app.browse.set_cursor(pos);
                if self.details_on_click {
                    self.details_open = true;
                }
            }
            Intent::SelectRange { from, to } => {
                self.nav_focus = false;
                self.select_anchor = from;
                self.select_range(from, to);
            }
            Intent::ExtendSelection(pos) => {
                self.nav_focus = false;
                self.select_range(self.select_anchor, pos);
            }
            Intent::ToggleSelected(pos) => {
                self.nav_focus = false;
                self.toggle_selected(pos);
            }
            Intent::ToggleMark(pos) => self.toggle_mark(pos),
            Intent::ClearMarks => self.app.browse.clear_marks(),
            Intent::ScrollToCursor => self.scroll_to_cursor = true,
            Intent::CycleTheme => {
                use egui::ThemePreference::{Dark, Light, System};
                let next = match ctx.options(|o| o.theme_preference) {
                    System => Light,
                    Light => Dark,
                    Dark => System,
                };
                ctx.set_theme(next);
            }
            Intent::ToggleDetails => {
                self.details_open = !self.details_open;
                // Hidden by hand, the panel stays hidden when rows are
                // clicked.
                if !self.details_open {
                    self.details_on_click = false;
                }
            }
            Intent::ToggleGrants => {
                self.grants_open = !self.grants_open;
                if !self.grants_open {
                    self.grant_focus = false;
                }
            }
            Intent::FocusTable => {
                self.nav_focus = false;
                self.grant_focus = false;
            }
            Intent::FocusGrants(focus) => {
                let first = browse::ordered_grants(&self.app.browse)
                    .first()
                    .map(|g| g.name.clone());
                if focus && let Some(first) = first {
                    self.app.browse.apply(Action::FocusList);
                    self.nav_focus = false;
                    self.grants_open = true;
                    self.grant_focus = true;
                    self.grant_selected.get_or_insert(first);
                    self.grant_scroll = true;
                    self.details_open = true;
                } else {
                    self.grant_focus = false;
                }
            }
            Intent::SelectGrant(name) => {
                self.app.browse.apply(Action::FocusList);
                self.nav_focus = false;
                self.grant_focus = true;
                self.grant_selected = Some(name);
                self.details_open = true;
            }
            Intent::GrantMove(step) => {
                self.move_grant(|at, len| (at as isize + step).clamp(0, len as isize - 1) as usize)
            }
            Intent::GrantEdge { last } => self.move_grant(|_, len| if last { len - 1 } else { 0 }),
            Intent::CopyGrantLink => {
                let link = self.grant_selected.as_deref().and_then(|name| {
                    let g = self.app.browse.live_grants().find(|g| g.name == name)?;
                    (g.state == GrantState::ApprovalAwaited)
                        .then(|| approval_console_url(&g.name))
                        .flatten()
                });
                if let (Some(url), Some(name)) = (link, self.grant_selected.as_deref()) {
                    ctx.copy_text(url);
                    browse::mark_link_copied(ctx, name);
                }
            }
            Intent::Sort(column) => {
                self.app.browse.toggle_sort(column);
                self.scroll_to_cursor = true;
            }
            Intent::CycleSort => {
                self.app.browse.cycle_sort();
                self.scroll_to_cursor = true;
            }
            Intent::ReverseSort => {
                self.app.browse.reverse_sort();
                self.scroll_to_cursor = true;
            }
            Intent::ToggleNavigator => {
                self.nav_open = !self.nav_open;
                self.nav_focus = false;
            }
            Intent::FocusNavigator(focus) => {
                if focus {
                    self.app.browse.apply(Action::FocusList);
                    self.nav_open = true;
                    self.nav_scroll = true;
                    // Several active scopes are a set to keep. A single one
                    // is where the cursor starts, and moving replaces it.
                    let state = &mut self.app.browse;
                    let active = state.scope_nodes().to_vec();
                    if let [only] = active.as_slice() {
                        self.nav_kept.clear();
                        if let Some(i) = state.scope_rows().iter().position(|r| r.key == *only) {
                            state.set_nav_cursor(i + 1);
                        }
                    } else {
                        self.nav_kept = active;
                    }
                }
                self.nav_focus = focus;
            }
            Intent::Nav(action) => {
                self.nav_scroll = true;
                if action == NavAction::Toggle {
                    // Space keeps the scope under the cursor, or lets it go.
                    if let Some(key) = self.nav_cursor_scope() {
                        match self.nav_kept.iter().position(|k| *k == key) {
                            Some(i) => {
                                self.nav_kept.remove(i);
                            }
                            None => self.nav_kept.push(key),
                        }
                    }
                } else {
                    self.app.browse.apply_nav(action);
                }
                self.show_nav_scopes();
            }
            Intent::NavSelect(pos) => {
                self.nav_kept.clear();
                self.app.browse.select_scope_at(pos);
                self.selection.clear();
                self.select_anchor = 0;
                self.nav_focus = false;
                self.scroll_to_cursor = true;
            }
            Intent::NavToggle(pos) => {
                self.app.browse.toggle_scope_at(pos);
                self.nav_kept = self.app.browse.scope_nodes().to_vec();
                self.selection.clear();
                self.scroll_to_cursor = true;
            }
            Intent::CompleteScopeToken => {
                let query = split_scope_token(&self.search.text);
                let state = &mut self.app.browse;
                let candidates = query
                    .token
                    .map(|t| state.scope_candidates(&t))
                    .unwrap_or_default();
                if !candidates.is_empty() {
                    let key = candidates[self.search.scope_choice % candidates.len()].clone();
                    state.add_scope(key);
                    state.set_query(&query.rest);
                    self.search.text = query.rest;
                    self.search.scope_choice = 0;
                    move_text_cursor(ctx, ids::search(), &self.search.text, SearchMove::End);
                    self.selection.clear();
                    return;
                }
                // Without an `in:` token the one being typed is a role.
                let query = split_role_token(&self.search.text);
                let candidates = query
                    .token
                    .map(|t| state.role_candidates(&t))
                    .unwrap_or_default();
                if !candidates.is_empty() {
                    state.add_role_filter(&candidates[self.search.scope_choice % candidates.len()]);
                    state.set_query(&query.rest);
                    self.search.text = query.rest;
                    self.search.scope_choice = 0;
                    move_text_cursor(ctx, ids::search(), &self.search.text, SearchMove::End);
                    self.selection.clear();
                }
            }
            Intent::CycleScopeToken(step) => {
                let count = match split_scope_token(&self.search.text).token {
                    Some(t) => self.app.browse.scope_candidates(&t).len(),
                    None => split_role_token(&self.search.text)
                        .token
                        .map(|t| self.app.browse.role_candidates(&t).len())
                        .unwrap_or(0),
                };
                if count > 0 {
                    let at = (self.search.scope_choice % count) as isize;
                    self.search.scope_choice = (at + step).rem_euclid(count as isize) as usize;
                }
            }
            Intent::ClearQuery => {
                self.search.text.clear();
                self.search.scope_choice = 0;
                self.app.browse.set_query("");
                self.selection.clear();
                self.scroll_to_cursor = true;
            }
            // Role labels come after the scope labels, so they go first.
            Intent::PopScope if !self.app.browse.role_filters().is_empty() => {
                self.app.browse.pop_role_filter()
            }
            Intent::PopScope => self.app.browse.pop_scope(),
            Intent::RemoveScope(key) => self.app.browse.remove_scope(&key),
            Intent::RemoveRole(role) => self.app.browse.remove_role_filter(&role),
            Intent::AddRole(role) => {
                self.app.browse.add_role_filter(&role);
                self.selection.clear();
                self.select_anchor = 0;
                self.scroll_to_cursor = true;
            }
            Intent::NavToggleExpand(pos) => {
                self.app.browse.set_nav_cursor(pos);
                self.app.browse.toggle_scope_expanded_at(pos);
            }
            Intent::OpenRow(pos) => {
                self.app.browse.set_cursor(pos);
                self.apply_browse(ctx, Action::Submit);
            }
            Intent::CloseOverlay => self.app.browse.close_overlays(),
            Intent::ScrollOverlay(by) => self.overlay_scroll += by,
            Intent::Request(action) => self.apply_request(ctx, action),
            Intent::DismissSummary => self.enter_browse(),
            Intent::Queue(action) => self.apply_queue(ctx, action),
            Intent::QueueCursor(pos) => {
                if let View::Queue(q) = &mut self.view {
                    q.view.set_cursor(pos);
                }
            }
            Intent::QueueOpenRow(pos) => {
                if let View::Queue(q) = &mut self.view {
                    q.view.set_cursor(pos);
                }
                self.apply_queue(ctx, QueueAction::Open);
            }
            Intent::Approve(action) => self.apply_approve(ctx, action),
        }
    }

    /// Highlight every row between two positions and move the cursor to the
    /// second one.
    fn select_range(&mut self, from: usize, to: usize) {
        let state = &mut self.app.browse;
        let filtered = state.filtered();
        if filtered.is_empty() {
            return;
        }
        let last = filtered.len() - 1;
        let (lo, hi) = (from.min(to).min(last), from.max(to).min(last));
        self.selection = filtered[lo..=hi].iter().copied().collect();
        state.set_cursor(to);
    }

    /// Add the row at `pos` to the selection or take it out, starting from
    /// the cursor row when nothing is selected yet.
    fn toggle_selected(&mut self, pos: usize) {
        let state = &mut self.app.browse;
        let Some(&idx) = state.filtered().get(pos) else {
            return;
        };
        if self.selection.is_empty()
            && let Some(&current) = state.filtered().get(state.cursor())
        {
            self.selection.insert(current);
        }
        if !self.selection.insert(idx) {
            self.selection.remove(&idx);
        }
        self.select_anchor = pos;
        state.set_cursor(pos);
    }

    /// Positions of the visible rows that are part of the selection.
    fn selected_positions(&self) -> Vec<usize> {
        self.app
            .browse
            .filtered()
            .iter()
            .enumerate()
            .filter(|(_, idx)| self.selection.contains(idx))
            .map(|(pos, _)| pos)
            .collect()
    }

    /// Flip the mark on the row at `pos`. When that row is part of a
    /// multi-row selection, every selected row gets the same new state.
    fn toggle_mark(&mut self, pos: usize) {
        let positions = self.selected_positions();
        let state = &mut self.app.browse;
        let Some(&idx) = state.filtered().get(pos) else {
            return;
        };
        if positions.len() > 1 && positions.contains(&pos) {
            state.set_marks_at(&positions, !state.is_marked(idx));
        } else {
            state.toggle_mark_at(pos);
        }
    }

    fn apply_browse(&mut self, ctx: &egui::Context, action: Action) {
        // Typing goes to the search field, so the navigator gives up the
        // keys.
        if matches!(action, Action::FocusSearch) {
            self.nav_focus = false;
            self.grant_focus = false;
        }
        // Esc first lets go of a selected grant, so the details panel shows
        // the row under the cursor again.
        if matches!(action, Action::ClearSelection)
            && !self.app.browse.overlay_open()
            && self.grant_selected.is_some()
        {
            self.grant_selected = None;
            self.grant_focus = false;
            return;
        }
        // With several rows highlighted, Space marks or unmarks all of them
        // and Esc drops the highlight before anything else.
        if !self.app.browse.overlay_open() && self.selected_positions().len() > 1 {
            match action {
                Action::ToggleSelect => {
                    self.toggle_mark(self.app.browse.cursor());
                    return;
                }
                Action::ClearSelection => {
                    self.selection.clear();
                    return;
                }
                _ => {}
            }
        }
        // Esc first drops a half-typed `in:name` token, which core's query
        // does not hold.
        if matches!(action, Action::ClearSelection)
            && !self.search.text.is_empty()
            && self.app.browse.query().is_empty()
            && !self.app.browse.only_marked()
        {
            self.search.text.clear();
            return;
        }
        let state = &mut self.app.browse;
        let cursor = state.cursor();
        if let Action::SearchCursor(mv) = action {
            move_text_cursor(ctx, ids::search(), state.query().as_str(), mv);
        }
        // Esc peels layers (marked view, filter, marks) and then does
        // nothing. A window is closed with Ctrl-Q or the window controls.
        let is_escape = matches!(action, Action::ClearSelection);
        let exit = state.apply(action);
        if state.cursor() != cursor {
            self.grant_selected = None;
            self.scroll_to_cursor = true;
            // Moving with the keyboard goes back to a single highlighted row.
            self.selection.clear();
            self.select_anchor = state.cursor();
        }
        match exit {
            None => {}
            Some(BrowseExit::Quit) if is_escape => {}
            Some(BrowseExit::Quit) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Some(BrowseExit::Request(selection)) => {
                self.view = View::Request(RequestScreen {
                    flow: RequestFlow::new(selection),
                    busy: false,
                    summary: false,
                });
            }
            Some(BrowseExit::OpenApprovalQueue) => {
                let mut view = QueueView::new(self.app.approval_queue.clone());
                if view.sync() {
                    self.view = View::Queue(QueueScreen {
                        view,
                        approve: None,
                        scroll_to_cursor: true,
                    });
                }
            }
        }
    }

    /// Return to the browse view and re-activate it, like the TUI does after
    /// every other screen.
    fn enter_browse(&mut self) {
        self.view = View::Browse;
        self.rt.block_on(self.app.browse.activate());
        self.scroll_to_cursor = true;
    }

    fn apply_request(&mut self, ctx: &egui::Context, action: RequestAction) {
        let View::Request(r) = &mut self.view else {
            return;
        };
        if r.busy || r.summary {
            return;
        }
        match r.flow.apply(action) {
            RequestStep::Continue => self.finish_request_if_done(),
            RequestStep::Submit(submission) => {
                r.busy = true;
                let poller = self.app.poller.clone();
                let tx = self.done_tx.clone();
                let ctx = ctx.clone();
                self.rt.spawn(async move {
                    let result = request::submit(&poller, submission).await;
                    let _ = tx.send(Done::Granted(result));
                    ctx.request_repaint();
                });
            }
            RequestStep::Abort => self.enter_browse(),
        }
    }

    /// Close the request flow once every entitlement is handled, pausing on
    /// the summary when something failed.
    fn finish_request_if_done(&mut self) {
        let View::Request(r) = &mut self.view else {
            return;
        };
        if r.flow.current().is_some() {
            return;
        }
        if r.flow.has_errors() {
            r.summary = true;
        } else {
            self.enter_browse();
        }
    }

    fn apply_queue(&mut self, ctx: &egui::Context, action: QueueAction) {
        let View::Queue(q) = &mut self.view else {
            return;
        };
        if q.approve.is_some() {
            return;
        }
        q.scroll_to_cursor = true;
        match q.view.apply(action) {
            QueueStep::Continue => {}
            QueueStep::Open(name) => {
                q.approve = Some(ApproveScreen::Loading(name.clone()));
                let backend = self.app.backend.clone();
                let tx = self.done_tx.clone();
                let ctx = ctx.clone();
                self.rt.spawn(async move {
                    let modal = ApproveModal::load(&backend, &name).await;
                    let _ = tx.send(Done::ApproveLoaded(modal));
                    ctx.request_repaint();
                });
            }
            QueueStep::Back => self.enter_browse(),
            QueueStep::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    fn apply_approve(&mut self, ctx: &egui::Context, action: ApproveAction) {
        let View::Queue(q) = &mut self.view else {
            return;
        };
        let Some(ApproveScreen::Ready(modal)) = &mut q.approve else {
            return;
        };
        match modal.apply(action) {
            ApproveStep::Continue => {}
            ApproveStep::Submit => {
                let Some(ApproveScreen::Ready(mut modal)) = q.approve.take() else {
                    return;
                };
                q.approve = Some(ApproveScreen::Submitting {
                    details: modal.details().cloned(),
                    step: modal.step().clone(),
                });
                let backend = self.app.backend.clone();
                let tx = self.done_tx.clone();
                let ctx = ctx.clone();
                self.rt.spawn(async move {
                    modal.submit(&backend).await;
                    let _ = tx.send(Done::ApproveSubmitted(modal));
                    ctx.request_repaint();
                });
            }
            ApproveStep::Close(ApproveOutcome::Quit) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ApproveStep::Close(outcome) => {
                q.view.finish_review(&outcome);
                q.approve = None;
                if !q.view.sync() {
                    self.enter_browse();
                }
            }
        }
    }

    fn on_done(&mut self, done: Done) {
        match (done, &mut self.view) {
            (Done::Granted(result), View::Request(r)) => {
                r.busy = false;
                r.flow.complete(result);
                self.finish_request_if_done();
            }
            (Done::ApproveLoaded(modal) | Done::ApproveSubmitted(modal), View::Queue(q)) => {
                q.approve = Some(ApproveScreen::Ready(modal));
            }
            // The view changed while the call was in flight. Browse picks up
            // created grants through the poller either way.
            _ => {}
        }
    }

    /// Apply queued background work: refresh progress, grant poller
    /// wake-ups, and finished network calls.
    fn drain_background(&mut self) {
        let state = &mut self.app.browse;
        for _ in 0..UPDATES_PER_FRAME {
            let Some(update) = state.try_next_update() else {
                break;
            };
            self.rt.block_on(state.handle_update(update));
        }
        state.expire_status(gpam_core::cache::now_unix());
        while let Ok(done) = self.done_rx.try_recv() {
            self.on_done(done);
        }
        if let View::Queue(q) = &mut self.view
            && q.approve.is_none()
            && !q.view.sync()
        {
            self.enter_browse();
        }
    }

    /// Move egui focus to the field core state wants, when that changed.
    fn push_focus(&mut self, ctx: &egui::Context) {
        let want = self.desired_focus();
        if want == self.pushed_focus {
            return;
        }
        ctx.memory_mut(|m| match want {
            Some(id) => m.request_focus(id),
            None => {
                if let Some(id) = m.focused().filter(|id| ids::is_text_field(*id)) {
                    m.surrender_focus(id);
                }
            }
        });
        self.pushed_focus = want;
    }

    /// After drawing, adopt focus changes made with the pointer: clicking
    /// into or out of the search field, or into the other request field.
    /// Fields of a modal keep focus, so a click on its background does not
    /// strand the keyboard.
    fn pull_focus(&mut self, ctx: &egui::Context) {
        let actual = ctx
            .memory(|m| m.focused())
            .filter(|id| ids::is_text_field(*id));
        let want = self.desired_focus();
        if actual == want {
            return;
        }
        match &mut self.view {
            View::Browse if !self.app.browse.overlay_open() => {
                let action = if actual == Some(ids::search()) {
                    self.nav_focus = false;
                    Action::FocusSearch
                } else {
                    Action::FocusList
                };
                self.app.browse.apply(action);
            }
            View::Request(r) => {
                let clicked = if actual == Some(ids::duration()) {
                    Some(Field::Duration)
                } else if actual == Some(ids::justification()) {
                    Some(Field::Justification)
                } else {
                    None
                };
                if let Some(field) = clicked
                    && r.flow.current().is_some_and(|m| m.field() != field)
                {
                    r.flow.apply(RequestAction::SwapField);
                }
            }
            _ => {}
        }
        // Push again next frame if core still disagrees with egui.
        self.pushed_focus = actual;
    }
}

impl eframe::App for GuiApp {
    /// Keyboard handling happens here, before egui sees the events. Keys
    /// bound for the current view are applied and removed. While no text
    /// field should have focus, every other key press is dropped too, so
    /// egui's own focus navigation and button activation never compete with
    /// the bindings.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut RawInput) {
        let events = std::mem::take(&mut raw_input.events);
        for event in events {
            if is_zoom_shortcut(&event) {
                raw_input.events.push(event);
                continue;
            }
            let text_target = self.desired_focus();
            let intent = keymap::translate(&event).and_then(|key| self.key_intent(key));
            if let Some(intent) = intent {
                self.apply(ctx, intent);
                continue;
            }
            if text_target.is_none() && is_keyboard(&event) {
                continue;
            }
            raw_input.events.push(event);
        }
    }

    /// Runs before every frame, and on its own while the window is hidden.
    /// It schedules the next run, so pollers and refreshes keep draining and
    /// countdowns tick without input.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_background();
        #[cfg(feature = "screenshot")]
        if let Some(mut scene) = self.scene.take() {
            scene.tick(self, ctx);
            self.scene = Some(scene);
        }
        ctx.request_repaint_after(IDLE_REPAINT);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }
}

impl GuiApp {
    /// One frame: draw, then apply pointer input and adopt focus changes.
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.push_focus(&ctx);

        let mut intents = Vec::new();
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        // The navigator is not drawn on a narrow window, so it cannot keep
        // the keyboard there.
        self.narrow = ui.available_width() < browse::NARROW_WIDTH;
        if self.narrow {
            self.nav_focus = false;
        }
        // Core's query can change without the field, for example when Esc
        // clears it. The field follows unless it only differs by a token.
        if split_scope_token(&self.search.text).rest != self.app.browse.query().as_str() {
            self.search.text = self.app.browse.query().as_str().to_owned();
        }
        // A kept scope taken out elsewhere, for example by clicking its label
        // in the search field, is no longer kept.
        let active = self.app.browse.scope_nodes();
        self.nav_kept.retain(|key| active.contains(key));
        let nav = browse::NavView {
            open: self.nav_open,
            focused: self.nav_focus,
            kept: &self.nav_kept,
            scroll_to_cursor: std::mem::take(&mut self.nav_scroll),
        };
        // A grant that left the list cannot stay selected, and a panel with
        // nothing to show cannot keep the keyboard.
        let live = browse::ordered_grants(&self.app.browse);
        if let Some(name) = &self.grant_selected
            && !live.iter().any(|g| g.name == *name)
        {
            self.grant_selected = None;
        }
        if live.is_empty() || !self.grants_open {
            self.grant_focus = false;
        }
        let panels = browse::Panels {
            details_open: self.details_open,
            grants_open: self.grants_open,
            scroll_to_cursor: scroll,
            grants: browse::GrantsView {
                focused: self.grant_focus,
                selected: self.grant_selected.as_deref(),
                scroll_to_selected: std::mem::take(&mut self.grant_scroll),
            },
        };
        browse::show(
            ui,
            &mut self.app.browse,
            &mut self.search,
            &self.selection,
            nav,
            panels,
            &mut intents,
        );

        match &mut self.view {
            View::Browse => {
                let scroll = std::mem::take(&mut self.overlay_scroll);
                browse::show_overlays(&ctx, &self.app.browse, scroll, &mut intents)
            }
            View::Request(r) => request_view::show(&ctx, r, &mut intents),
            View::Queue(q) => match &mut q.approve {
                None => {
                    let scroll = std::mem::take(&mut q.scroll_to_cursor);
                    queue::show(&ctx, &q.view, scroll, &mut intents);
                }
                Some(screen) => approve::show(&ctx, screen, &mut intents),
            },
        }

        for intent in intents {
            self.apply(&ctx, intent);
        }
        self.pull_focus(&ctx);
    }
}

/// Cmd or Ctrl with plus, equals, minus or zero. egui handles these itself
/// and scales the whole interface, so they bypass the key bindings.
fn is_zoom_shortcut(event: &Event) -> bool {
    matches!(
        event,
        Event::Key {
            key: egui::Key::Plus | egui::Key::Equals | egui::Key::Minus | egui::Key::Num0,
            pressed: true,
            modifiers,
            ..
        } if modifiers.command && !modifiers.alt
    )
}

fn is_keyboard(event: &Event) -> bool {
    matches!(
        event,
        Event::Key { pressed: true, .. } | Event::Text(_) | Event::Cut | Event::Paste(_)
    )
}

/// Put the cursor of a text field at its start or end, for the readline
/// Ctrl-A and Ctrl-E bindings.
fn move_text_cursor(ctx: &egui::Context, id: Id, text: &str, mv: SearchMove) {
    let pos = match mv {
        SearchMove::Home => 0,
        SearchMove::End => text.chars().count(),
        SearchMove::Left | SearchMove::Right => return,
    };
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let cursor = egui::text::CCursor::new(pos);
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
        state.store(ctx, id);
    }
}
