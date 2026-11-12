//! Keyboard bindings. egui input events are first reduced to a small [`Key`]
//! type that matches what the terminal frontend sees, then each screen maps
//! keys to core actions with the same bindings as the TUI.
//!
//! Text editing inside a focused field (typing, Backspace, Left, Right,
//! Home, End, Ctrl-W, clipboard) is left to egui's native `TextEdit`, so
//! the maps below only return the keys that leave or confirm a field.

use eframe::egui::{Event, Key as EguiKey, Modifiers};
use gpam_core::app::action::{Action, Focus, SearchMove};
use gpam_core::app::approval_queue::QueueAction;
use gpam_core::app::approve::{ApproveAction, Decision, Step};
use gpam_core::app::request::RequestAction;
use gpam_core::app::scope_tree::NavAction;

/// One key press, reduced to what the bindings care about. Printable keys
/// arrive as [`Key::Char`] from egui text events, so the keyboard layout and
/// Shift are already applied. Control chords arrive as [`Key::Ctrl`] with a
/// lowercase letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    /// Escape with Shift held.
    ShiftEsc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
}

/// Reduce one egui event to a [`Key`]. A physical key press produces both a
/// key event and a text event in egui, and exactly one of them maps here.
pub fn translate(event: &Event) -> Option<Key> {
    match event {
        Event::Text(text) => {
            let mut chars = text.chars();
            let c = chars.next()?;
            chars.next().is_none().then_some(Key::Char(c))
        }
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => from_key(*key, *modifiers),
        _ => None,
    }
}

fn from_key(key: EguiKey, m: Modifiers) -> Option<Key> {
    if m.ctrl && !m.alt && !m.shift && !m.mac_cmd {
        return letter(key).map(Key::Ctrl);
    }
    if m.ctrl || m.alt || m.mac_cmd || m.command {
        return None;
    }
    if m.shift {
        return match key {
            EguiKey::Tab => Some(Key::BackTab),
            EguiKey::Escape => Some(Key::ShiftEsc),
            _ => None,
        };
    }
    Some(match key {
        EguiKey::Enter => Key::Enter,
        EguiKey::Escape => Key::Esc,
        EguiKey::Tab => Key::Tab,
        EguiKey::Backspace => Key::Backspace,
        EguiKey::ArrowUp => Key::Up,
        EguiKey::ArrowDown => Key::Down,
        EguiKey::ArrowLeft => Key::Left,
        EguiKey::ArrowRight => Key::Right,
        EguiKey::Home => Key::Home,
        EguiKey::End => Key::End,
        _ => return None,
    })
}

fn letter(key: EguiKey) -> Option<char> {
    let mut chars = key.name().chars();
    let c = chars.next()?;
    (chars.next().is_none() && c.is_ascii_alphabetic()).then(|| c.to_ascii_lowercase())
}

/// Browse view, mirroring the TUI's list and search bindings.
pub fn browse(focus: Focus, key: Key) -> Option<Action> {
    match focus {
        Focus::List => list_action(key),
        Focus::Search => search_action(key),
    }
}

fn list_action(key: Key) -> Option<Action> {
    use Key::*;
    Some(match key {
        // A window is closed with Ctrl-Q or its close button. A bare `q`
        // is too easy to hit by accident.
        Ctrl('q') => Action::Quit,
        Char('j') | Down | Ctrl('n') => Action::MoveDown(1),
        Char('k') | Up | Ctrl('p') => Action::MoveUp(1),
        Ctrl('d') => Action::MoveDown(10),
        Ctrl('u') => Action::MoveUp(10),
        Char('g') | Home => Action::JumpTop,
        Char('G') | End => Action::JumpBottom,
        Char(' ') => Action::ToggleSelect,
        Enter => Action::Submit,
        Char('R') => Action::Refresh,
        Char('/') => Action::FocusSearch,
        Esc => Action::ClearSelection,
        Char('?') => Action::Help,
        Char('L') => Action::ShowLogs,
        Char('J') => Action::ToggleRaw,
        Char('f') => Action::CycleScopeFilter,
        Char('m') => Action::ToggleMarkedView,
        Char('A') => Action::OpenApprovalQueue,
        _ => return None,
    })
}

fn search_action(key: Key) -> Option<Action> {
    use Key::*;
    Some(match key {
        Esc => Action::FocusList,
        Enter => Action::Submit,
        Ctrl('q') => Action::Quit,
        // Core returns focus to the list before moving.
        Up | Ctrl('p') => Action::MoveUp(1),
        Down | Ctrl('n') => Action::MoveDown(1),
        // Readline home and end. The frontend also moves the field's cursor.
        Ctrl('a') => Action::SearchCursor(SearchMove::Home),
        Ctrl('e') => Action::SearchCursor(SearchMove::End),
        _ => return None,
    })
}

/// What a key does while the help or logs overlay is open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverlayKey {
    Quit,
    Close,
    /// Scroll the overlay by this many points. Positive is down.
    Scroll(f32),
}

/// List keys that only this frontend has, checked before the shared list
/// bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKey {
    /// Move keyboard focus to the next open panel.
    NextPanel,
    /// Show or hide the scope navigator.
    ToggleNavigator,
    /// Step the colour theme: auto, light, dark.
    CycleTheme,
    /// Show or hide the details panel.
    ToggleDetails,
    /// Open or close the grants panel.
    ToggleGrants,
    /// Order the list by the next column.
    CycleSort,
    /// Flip the direction of the column order.
    ReverseSort,
    /// Empty the typed search text.
    ClearQuery,
    /// Move keyboard focus to the previous open panel.
    PrevPanel,
}

pub fn panel(key: Key) -> Option<PanelKey> {
    match key {
        Key::Tab => Some(PanelKey::NextPanel),
        Key::BackTab => Some(PanelKey::PrevPanel),
        Key::Char('[') => Some(PanelKey::ToggleNavigator),
        Key::Char('T') => Some(PanelKey::CycleTheme),
        Key::Char(']') => Some(PanelKey::ToggleDetails),
        Key::Char('b') => Some(PanelKey::ToggleGrants),
        Key::Char('s') => Some(PanelKey::CycleSort),
        Key::Char('S') => Some(PanelKey::ReverseSort),
        Key::Ctrl('l') => Some(PanelKey::ClearQuery),
        _ => None,
    }
}

/// What a key does while the scope navigator has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    Move(NavAction),
    /// Narrow the list to the scope under the cursor and return to the list.
    Select,
    /// Return to the list without changing the scope.
    Back,
    /// Move keyboard focus to the next open panel.
    NextPanel,
    /// Move keyboard focus to the previous open panel.
    PrevPanel,
    /// Hide the navigator and return to the list.
    Hide,
    /// Leave the navigator for the search field.
    Search,
    /// Empty the typed search text.
    ClearQuery,
    /// Show or hide the details panel.
    ToggleDetails,
    /// Open or close the grants panel.
    ToggleGrants,
    Help,
    Quit,
}

/// Scope navigator, with the list's movement keys plus h and l to close and
/// open a branch.
pub fn navigator(key: Key) -> Option<NavKey> {
    use Key::*;
    Some(match key {
        Char('j') | Down | Ctrl('n') => NavKey::Move(NavAction::Down),
        Char('k') | Up | Ctrl('p') => NavKey::Move(NavAction::Up),
        Char('g') | Home => NavKey::Move(NavAction::Top),
        Char('G') | End => NavKey::Move(NavAction::Bottom),
        Char('l') | Right => NavKey::Move(NavAction::Expand),
        Char('h') | Left => NavKey::Move(NavAction::Collapse),
        Enter => NavKey::Select,
        Char(' ') => NavKey::Move(NavAction::Toggle),
        Tab => NavKey::NextPanel,
        BackTab => NavKey::PrevPanel,
        Esc => NavKey::Back,
        Char('[') => NavKey::Hide,
        Char('/') => NavKey::Search,
        Ctrl('l') => NavKey::ClearQuery,
        Char(']') => NavKey::ToggleDetails,
        Char('b') => NavKey::ToggleGrants,
        Char('?') => NavKey::Help,
        Ctrl('q') => NavKey::Quit,
        _ => return None,
    })
}

/// What a key does while the grants panel has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKey {
    /// Select the card this many places on.
    Move(isize),
    First,
    Last,
    /// Copy the console approval link of the selected grant.
    CopyLink,
    /// Return to the list.
    Back,
    /// Move keyboard focus to the next open panel.
    NextPanel,
    /// Move keyboard focus to the previous open panel.
    PrevPanel,
    /// Close the grants panel and return to the list.
    Hide,
    ToggleDetails,
    /// Empty the typed search text.
    ClearQuery,
    Help,
    Quit,
}

/// Grants panel. Cards are laid out left to right and wrap, so every
/// movement key steps along that order.
pub fn grants(key: Key) -> Option<GrantKey> {
    use Key::*;
    Some(match key {
        Char('l') | Char('j') | Right | Down | Ctrl('n') => GrantKey::Move(1),
        Char('h') | Char('k') | Left | Up | Ctrl('p') => GrantKey::Move(-1),
        Char('g') | Home => GrantKey::First,
        Char('G') | End => GrantKey::Last,
        Char('y') => GrantKey::CopyLink,
        Tab => GrantKey::NextPanel,
        BackTab => GrantKey::PrevPanel,
        Esc => GrantKey::Back,
        Char('b') => GrantKey::Hide,
        Char(']') => GrantKey::ToggleDetails,
        Ctrl('l') => GrantKey::ClearQuery,
        Char('?') => GrantKey::Help,
        Ctrl('q') => GrantKey::Quit,
        _ => return None,
    })
}

/// Movement keys scroll an overlay, Ctrl-Q quits and any other key closes
/// it.
pub fn overlay(key: Key) -> OverlayKey {
    use Key::*;
    match key {
        Ctrl('q') => OverlayKey::Quit,
        Char('j') | Down | Ctrl('n') => OverlayKey::Scroll(40.0),
        Char('k') | Up | Ctrl('p') => OverlayKey::Scroll(-40.0),
        Ctrl('d') => OverlayKey::Scroll(240.0),
        Ctrl('u') => OverlayKey::Scroll(-240.0),
        _ => OverlayKey::Close,
    }
}

/// Request prompts for one entitlement.
pub fn request(key: Key) -> Option<RequestAction> {
    match key {
        Key::ShiftEsc => Some(RequestAction::Abort),
        Key::Esc => Some(RequestAction::Skip),
        Key::Enter => Some(RequestAction::Confirm),
        Key::Tab => Some(RequestAction::SwapField),
        _ => None,
    }
}

/// True when the key dismisses the request error summary.
pub fn dismiss_summary(key: Key) -> bool {
    matches!(key, Key::Enter | Key::Esc | Key::Char(' ') | Key::Char('q'))
}

/// Approve modal. Letters are commands in the confirm and result steps but
/// text while typing a reason, so the mapping depends on the step.
pub fn approve(step: &Step, key: Key) -> Option<ApproveAction> {
    if key == Key::Ctrl('q') {
        return Some(ApproveAction::Quit);
    }
    match step {
        Step::Confirm => match key {
            Key::Esc | Key::Char('q') => Some(ApproveAction::Cancel),
            Key::Char('a' | 'A') => Some(ApproveAction::Choose(Decision::Approve)),
            Key::Char('d' | 'D') => Some(ApproveAction::Choose(Decision::Deny)),
            _ => None,
        },
        Step::Reason(_) => match key {
            Key::Esc => Some(ApproveAction::Back),
            Key::Enter => Some(ApproveAction::Submit),
            _ => None,
        },
        Step::Result(_) => match key {
            Key::Enter | Key::Esc | Key::Char('q') | Key::Char(' ') => Some(ApproveAction::Dismiss),
            _ => None,
        },
    }
}

/// Approval queue list.
pub fn queue(key: Key) -> Option<QueueAction> {
    use Key::*;
    Some(match key {
        Ctrl('q') => QueueAction::Quit,
        Char('j') | Down => QueueAction::Down,
        Char('k') | Up => QueueAction::Up,
        Char('g') => QueueAction::Top,
        Char('G') => QueueAction::Bottom,
        Char('d') | Char('x') => QueueAction::Drop,
        Enter => QueueAction::Open,
        Esc | Char('q') => QueueAction::Back,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use gpam_core::app::approve::StepResult;

    use super::*;

    fn key_event(key: EguiKey, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn printable_keys_come_from_text_events() {
        assert_eq!(translate(&Event::Text("G".into())), Some(Key::Char('G')));
        assert_eq!(translate(&Event::Text(" ".into())), Some(Key::Char(' ')));
        // The key event of the same press is ignored, so a press maps once.
        assert_eq!(translate(&key_event(EguiKey::G, Modifiers::SHIFT)), None);
        assert_eq!(translate(&key_event(EguiKey::Space, Modifiers::NONE)), None);
        // Composed multi-character text is not a key binding.
        assert_eq!(translate(&Event::Text("ab".into())), None);
    }

    #[test]
    fn named_keys_and_ctrl_chords() {
        let none = Modifiers::NONE;
        assert_eq!(translate(&key_event(EguiKey::Escape, none)), Some(Key::Esc));
        assert_eq!(
            translate(&key_event(EguiKey::Enter, none)),
            Some(Key::Enter)
        );
        assert_eq!(
            translate(&key_event(EguiKey::Tab, Modifiers::SHIFT)),
            Some(Key::BackTab)
        );
        assert_eq!(
            translate(&key_event(EguiKey::N, Modifiers::CTRL)),
            Some(Key::Ctrl('n'))
        );
        // Shift-selection and Alt-word movement stay with the text field.
        assert_eq!(
            translate(&key_event(EguiKey::ArrowLeft, Modifiers::SHIFT)),
            None
        );
        assert_eq!(
            translate(&key_event(EguiKey::ArrowLeft, Modifiers::ALT)),
            None
        );
        let released = Event::Key {
            key: EguiKey::Enter,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: none,
        };
        assert_eq!(translate(&released), None);
    }

    #[test]
    fn copy_event_is_left_to_egui() {
        assert_eq!(translate(&Event::Copy), None);
    }

    #[test]
    fn j_moves_down_in_list() {
        assert!(matches!(
            browse(Focus::List, Key::Char('j')),
            Some(Action::MoveDown(1))
        ));
    }

    #[test]
    fn letters_go_to_the_search_field() {
        for c in ['j', 'f', 'm', 'A', 'L', 'q', '/'] {
            assert!(browse(Focus::Search, Key::Char(c)).is_none(), "{c}");
        }
    }

    #[test]
    fn space_selects() {
        assert!(matches!(
            browse(Focus::List, Key::Char(' ')),
            Some(Action::ToggleSelect)
        ));
    }

    #[test]
    fn search_focus_is_action_rich() {
        assert!(matches!(
            browse(Focus::Search, Key::Down),
            Some(Action::MoveDown(1))
        ));
        assert!(matches!(
            browse(Focus::Search, Key::Up),
            Some(Action::MoveUp(1))
        ));
        assert!(matches!(
            browse(Focus::Search, Key::Ctrl('n')),
            Some(Action::MoveDown(1))
        ));
        assert!(matches!(
            browse(Focus::Search, Key::Ctrl('p')),
            Some(Action::MoveUp(1))
        ));
        assert!(matches!(
            browse(Focus::Search, Key::Enter),
            Some(Action::Submit)
        ));
        assert!(matches!(
            browse(Focus::Search, Key::Esc),
            Some(Action::FocusList)
        ));
    }

    #[test]
    fn ctrl_q_quits_from_anywhere() {
        assert!(matches!(
            browse(Focus::List, Key::Ctrl('q')),
            Some(Action::Quit)
        ));
        assert!(matches!(
            browse(Focus::Search, Key::Ctrl('q')),
            Some(Action::Quit)
        ));
        assert_eq!(overlay(Key::Ctrl('q')), OverlayKey::Quit);
        assert_eq!(overlay(Key::Char('j')), OverlayKey::Scroll(40.0));
        assert_eq!(overlay(Key::Char('x')), OverlayKey::Close);
        assert_eq!(request(Key::ShiftEsc), Some(RequestAction::Abort));
        // Ctrl-C is left for copying and is not a binding anywhere.
        assert!(browse(Focus::List, Key::Ctrl('c')).is_none());
        assert_eq!(request(Key::Ctrl('c')), None);
        assert!(!dismiss_summary(Key::Ctrl('c')));
        assert_eq!(
            approve(&Step::Confirm, Key::Ctrl('q')),
            Some(ApproveAction::Quit)
        );
        assert_eq!(queue(Key::Ctrl('q')), Some(QueueAction::Quit));
    }

    #[test]
    fn ctrl_d_and_u_only_in_list_focus() {
        assert!(matches!(
            browse(Focus::List, Key::Ctrl('d')),
            Some(Action::MoveDown(10))
        ));
        assert!(matches!(
            browse(Focus::List, Key::Ctrl('u')),
            Some(Action::MoveUp(10))
        ));
        assert!(browse(Focus::Search, Key::Ctrl('d')).is_none());
        assert!(browse(Focus::Search, Key::Ctrl('u')).is_none());
    }

    #[test]
    fn list_letters_map_like_the_tui() {
        let cases = [
            ('f', "CycleScopeFilter"),
            ('m', "ToggleMarkedView"),
            ('A', "OpenApprovalQueue"),
            ('L', "ShowLogs"),
            ('J', "ToggleRaw"),
            ('R', "Refresh"),
            ('?', "Help"),
            ('/', "FocusSearch"),
            ('g', "JumpTop"),
            ('G', "JumpBottom"),
        ];
        for (c, want) in cases {
            let got = browse(Focus::List, Key::Char(c)).expect("mapped");
            assert_eq!(format!("{got:?}"), want, "{c}");
        }
        assert!(matches!(
            browse(Focus::List, Key::Esc),
            Some(Action::ClearSelection)
        ));
        // A bare `q` does not close the window.
        assert!(browse(Focus::List, Key::Char('q')).is_none());
        assert_eq!(navigator(Key::Char('q')), None);
        assert!(matches!(
            browse(Focus::List, Key::Ctrl('q')),
            Some(Action::Quit)
        ));
    }

    #[test]
    fn readline_home_and_end_in_search_focus() {
        assert!(matches!(
            browse(Focus::Search, Key::Ctrl('a')),
            Some(Action::SearchCursor(SearchMove::Home))
        ));
        assert!(matches!(
            browse(Focus::Search, Key::Ctrl('e')),
            Some(Action::SearchCursor(SearchMove::End))
        ));
        // Ctrl-W is handled by the text field itself.
        assert!(browse(Focus::Search, Key::Ctrl('w')).is_none());
    }

    #[test]
    fn request_keys() {
        assert_eq!(request(Key::Esc), Some(RequestAction::Skip));
        assert_eq!(request(Key::Enter), Some(RequestAction::Confirm));
        assert_eq!(request(Key::Tab), Some(RequestAction::SwapField));
        assert_eq!(request(Key::Char('x')), None);
        assert!(dismiss_summary(Key::Char('q')));
        assert!(!dismiss_summary(Key::Char('x')));
    }

    #[test]
    fn approve_keys_depend_on_step() {
        assert_eq!(
            approve(&Step::Confirm, Key::Char('A')),
            Some(ApproveAction::Choose(Decision::Approve))
        );
        assert_eq!(
            approve(&Step::Confirm, Key::Char('d')),
            Some(ApproveAction::Choose(Decision::Deny))
        );
        assert_eq!(
            approve(&Step::Confirm, Key::Esc),
            Some(ApproveAction::Cancel)
        );
        let reason = Step::Reason(Decision::Approve);
        assert_eq!(approve(&reason, Key::Char('a')), None);
        assert_eq!(approve(&reason, Key::Enter), Some(ApproveAction::Submit));
        assert_eq!(approve(&reason, Key::Esc), Some(ApproveAction::Back));
        let result = Step::Result(StepResult::AlreadyHandled);
        assert_eq!(
            approve(&result, Key::Char(' ')),
            Some(ApproveAction::Dismiss)
        );
        assert_eq!(approve(&result, Key::Char('a')), None);
    }

    #[test]
    fn queue_keys() {
        assert_eq!(queue(Key::Char('j')), Some(QueueAction::Down));
        assert_eq!(queue(Key::Up), Some(QueueAction::Up));
        assert_eq!(queue(Key::Char('G')), Some(QueueAction::Bottom));
        assert_eq!(queue(Key::Char('x')), Some(QueueAction::Drop));
        assert_eq!(queue(Key::Enter), Some(QueueAction::Open));
        assert_eq!(queue(Key::Char('q')), Some(QueueAction::Back));
        assert_eq!(queue(Key::Char('z')), None);
    }

    #[test]
    fn navigator_keys() {
        assert_eq!(panel(Key::Tab), Some(PanelKey::NextPanel));
        assert_eq!(panel(Key::Char('[')), Some(PanelKey::ToggleNavigator));
        assert_eq!(panel(Key::Char('T')), Some(PanelKey::CycleTheme));
        assert_eq!(panel(Key::Char(']')), Some(PanelKey::ToggleDetails));
        assert_eq!(panel(Key::Char('b')), Some(PanelKey::ToggleGrants));
        assert_eq!(panel(Key::Char('s')), Some(PanelKey::CycleSort));
        assert_eq!(panel(Key::Char('S')), Some(PanelKey::ReverseSort));
        assert_eq!(panel(Key::Ctrl('l')), Some(PanelKey::ClearQuery));
        assert_eq!(navigator(Key::Ctrl('l')), Some(NavKey::ClearQuery));
        assert_eq!(navigator(Key::Char(']')), Some(NavKey::ToggleDetails));
        assert_eq!(navigator(Key::Char('b')), Some(NavKey::ToggleGrants));
        assert_eq!(grants(Key::Ctrl('l')), Some(GrantKey::ClearQuery));
        assert_eq!(panel(Key::Char('j')), None);
        // Tab and Shift-Tab walk the open panels from every one of them.
        assert_eq!(panel(Key::BackTab), Some(PanelKey::PrevPanel));
        assert_eq!(navigator(Key::Tab), Some(NavKey::NextPanel));
        assert_eq!(navigator(Key::BackTab), Some(NavKey::PrevPanel));
        assert_eq!(grants(Key::Tab), Some(GrantKey::NextPanel));
        assert_eq!(grants(Key::BackTab), Some(GrantKey::PrevPanel));
        assert_eq!(grants(Key::Esc), Some(GrantKey::Back));
        assert_eq!(grants(Key::Char('l')), Some(GrantKey::Move(1)));
        assert_eq!(grants(Key::Char('k')), Some(GrantKey::Move(-1)));
        assert_eq!(grants(Key::Char('y')), Some(GrantKey::CopyLink));
        assert_eq!(grants(Key::Char('x')), None);

        assert_eq!(
            navigator(Key::Char('j')),
            Some(NavKey::Move(NavAction::Down))
        );
        assert_eq!(navigator(Key::Up), Some(NavKey::Move(NavAction::Up)));
        assert_eq!(
            navigator(Key::Char('l')),
            Some(NavKey::Move(NavAction::Expand))
        );
        assert_eq!(
            navigator(Key::Left),
            Some(NavKey::Move(NavAction::Collapse))
        );
        assert_eq!(navigator(Key::Enter), Some(NavKey::Select));
        assert_eq!(
            navigator(Key::Char(' ')),
            Some(NavKey::Move(NavAction::Toggle))
        );
        assert_eq!(navigator(Key::Esc), Some(NavKey::Back));
        assert_eq!(navigator(Key::Char('[')), Some(NavKey::Hide));
        assert_eq!(navigator(Key::Char('/')), Some(NavKey::Search));
        assert_eq!(navigator(Key::Ctrl('q')), Some(NavKey::Quit));
        assert_eq!(navigator(Key::Char('x')), None);
    }
}
