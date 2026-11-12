/// What part of the browse view consumes the next keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Search,
}

/// Browse-view intents. Frontends translate their input events into these
/// and hand them to [`crate::app::browse::BrowseState::apply`].
#[derive(Debug, Clone)]
pub enum Action {
    Quit,
    MoveUp(usize),
    MoveDown(usize),
    JumpTop,
    JumpBottom,
    ToggleSelect,
    Submit,
    Refresh,
    FocusSearch,
    FocusList,
    ClearSelection,
    SearchInsert(char),
    SearchBackspace,
    SearchCursor(SearchMove),
    SearchDeleteWordBack,
    Help,
    ShowLogs,
    ToggleRaw,
    CycleScopeFilter,
    ToggleMarkedView,
    OpenApprovalQueue,
}

#[derive(Debug, Clone, Copy)]
pub enum SearchMove {
    Left,
    Right,
    Home,
    End,
}
