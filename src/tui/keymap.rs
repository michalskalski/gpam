use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What part of the UI consumes the next keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Search,
}

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

/// Pure mapping from (focus, key) to an action. Returning `None` means the
/// key is ignored in this focus.
pub fn dispatch(focus: Focus, key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match focus {
        Focus::List => list_action(key, ctrl),
        Focus::Search => search_action(key, ctrl),
    }
}

fn list_action(key: KeyEvent, ctrl: bool) -> Option<Action> {
    use KeyCode::*;
    Some(match key.code {
        Char('q') => Action::Quit,
        Char('c') if ctrl => Action::Quit,
        Char('j') if !ctrl => Action::MoveDown(1),
        Down => Action::MoveDown(1),
        Char('n') if ctrl => Action::MoveDown(1),
        Char('k') if !ctrl => Action::MoveUp(1),
        Up => Action::MoveUp(1),
        Char('p') if ctrl => Action::MoveUp(1),
        Char('d') if ctrl => Action::MoveDown(10),
        Char('u') if ctrl => Action::MoveUp(10),
        Char('g') => Action::JumpTop,
        Char('G') => Action::JumpBottom,
        Home => Action::JumpTop,
        End => Action::JumpBottom,
        Char(' ') => Action::ToggleSelect,
        Enter => Action::Submit,
        Char('R') => Action::Refresh,
        Char('/') => Action::FocusSearch,
        Esc => Action::ClearSelection,
        Char('?') => Action::Help,
        Char('L') => Action::ShowLogs,
        Char('J') => Action::ToggleRaw,
        Char('f') if !ctrl => Action::CycleScopeFilter,
        Char('m') if !ctrl => Action::ToggleMarkedView,
        Char('A') => Action::OpenApprovalQueue,
        _ => return None,
    })
}

fn search_action(key: KeyEvent, ctrl: bool) -> Option<Action> {
    use KeyCode::*;
    Some(match key.code {
        Esc => Action::FocusList,
        Enter => Action::Submit,
        Char('c') if ctrl => Action::Quit,

        // Movement keys are caught here but the BrowseScreen `apply()` returns
        // focus to the list before performing them. That makes Space-select
        // immediately reachable after a peek into the filtered list.
        Up => Action::MoveUp(1),
        Down => Action::MoveDown(1),
        Char('n') if ctrl => Action::MoveDown(1),
        Char('p') if ctrl => Action::MoveUp(1),

        // Readline-style editing inside the input.
        Char('a') if ctrl => Action::SearchCursor(SearchMove::Home),
        Char('e') if ctrl => Action::SearchCursor(SearchMove::End),
        Char('w') if ctrl => Action::SearchDeleteWordBack,

        Backspace => Action::SearchBackspace,
        Left => Action::SearchCursor(SearchMove::Left),
        Right => Action::SearchCursor(SearchMove::Right),
        Home => Action::SearchCursor(SearchMove::Home),
        End => Action::SearchCursor(SearchMove::End),
        Char(c) if !ctrl => Action::SearchInsert(c),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn j_moves_down_in_list() {
        match dispatch(Focus::List, key(KeyCode::Char('j'))).unwrap() {
            Action::MoveDown(1) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn j_types_into_search() {
        match dispatch(Focus::Search, key(KeyCode::Char('j'))).unwrap() {
            Action::SearchInsert('j') => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn space_selects() {
        assert!(matches!(
            dispatch(Focus::List, key(KeyCode::Char(' '))),
            Some(Action::ToggleSelect)
        ));
    }

    #[test]
    fn search_focus_is_action_rich() {
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Down)),
            Some(Action::MoveDown(1))
        ));
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Up)),
            Some(Action::MoveUp(1))
        ));
        assert!(matches!(
            dispatch(Focus::Search, ctrl(KeyCode::Char('n'))),
            Some(Action::MoveDown(1))
        ));
        assert!(matches!(
            dispatch(Focus::Search, ctrl(KeyCode::Char('p'))),
            Some(Action::MoveUp(1))
        ));
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Enter)),
            Some(Action::Submit)
        ));
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Esc)),
            Some(Action::FocusList)
        ));
    }

    #[test]
    fn ctrl_c_quits_from_anywhere() {
        assert!(matches!(
            dispatch(Focus::List, ctrl(KeyCode::Char('c'))),
            Some(Action::Quit)
        ));
        assert!(matches!(
            dispatch(Focus::Search, ctrl(KeyCode::Char('c'))),
            Some(Action::Quit)
        ));
    }

    #[test]
    fn ctrl_d_and_u_only_in_list_focus() {
        assert!(matches!(
            dispatch(Focus::List, ctrl(KeyCode::Char('d'))),
            Some(Action::MoveDown(10))
        ));
        assert!(matches!(
            dispatch(Focus::List, ctrl(KeyCode::Char('u'))),
            Some(Action::MoveUp(10))
        ));
        assert!(dispatch(Focus::Search, ctrl(KeyCode::Char('d'))).is_none());
        assert!(dispatch(Focus::Search, ctrl(KeyCode::Char('u'))).is_none());
    }

    #[test]
    fn f_cycles_scope_filter_in_list_focus_only() {
        assert!(matches!(
            dispatch(Focus::List, key(KeyCode::Char('f'))),
            Some(Action::CycleScopeFilter)
        ));
        // 'f' in the search box must remain a literal character.
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Char('f'))),
            Some(Action::SearchInsert('f'))
        ));
    }

    #[test]
    fn m_toggles_marked_view_in_list_focus_only() {
        assert!(matches!(
            dispatch(Focus::List, key(KeyCode::Char('m'))),
            Some(Action::ToggleMarkedView)
        ));
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Char('m'))),
            Some(Action::SearchInsert('m'))
        ));
    }

    #[test]
    fn capital_a_opens_approval_queue_in_list_focus_only() {
        assert!(matches!(
            dispatch(Focus::List, key(KeyCode::Char('A'))),
            Some(Action::OpenApprovalQueue)
        ));
        // 'A' in the search box stays a literal character.
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Char('A'))),
            Some(Action::SearchInsert('A'))
        ));
    }

    #[test]
    fn capital_l_shows_logs_in_list_focus_only() {
        assert!(matches!(
            dispatch(Focus::List, key(KeyCode::Char('L'))),
            Some(Action::ShowLogs)
        ));
        assert!(matches!(
            dispatch(Focus::Search, key(KeyCode::Char('L'))),
            Some(Action::SearchInsert('L'))
        ));
    }

    #[test]
    fn readline_keys_in_search_focus() {
        assert!(matches!(
            dispatch(Focus::Search, ctrl(KeyCode::Char('a'))),
            Some(Action::SearchCursor(SearchMove::Home))
        ));
        assert!(matches!(
            dispatch(Focus::Search, ctrl(KeyCode::Char('e'))),
            Some(Action::SearchCursor(SearchMove::End))
        ));
        assert!(matches!(
            dispatch(Focus::Search, ctrl(KeyCode::Char('w'))),
            Some(Action::SearchDeleteWordBack)
        ));
    }
}
