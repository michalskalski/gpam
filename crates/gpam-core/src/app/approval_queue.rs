use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::gcp::grants::parse_grant_name;
use crate::ipc::ApprovalEvent;

use super::approve::ApproveOutcome;
use super::format::format_age;

/// Wraps an inbound event with the time we observed it, so the list can
/// display age since arrival.
#[derive(Debug, Clone)]
pub struct QueuedEvent {
    pub event: ApprovalEvent,
    pub received_at: SystemTime,
}

impl QueuedEvent {
    /// `(entitlement, scope short name, scope id)` parsed from the grant
    /// name, or `?` placeholders when the name does not parse.
    pub fn display_parts(&self) -> (&str, &'static str, &str) {
        match parse_grant_name(&self.event.name) {
            Some(p) => (p.entitlement, p.scope.short(), p.scope_id),
            None => ("?", "?", "?"),
        }
    }

    /// Time since the event arrived, relative to `now`.
    pub fn age_label(&self, now: SystemTime) -> String {
        now.duration_since(self.received_at)
            .map(|d| format_age(d.as_secs() as i64))
            .unwrap_or_else(|_| "0s".into())
    }

    /// The `source` tag, or `-` when the producer did not set one.
    pub fn source_label(&self) -> &str {
        self.event.source.as_deref().unwrap_or("-")
    }
}

/// Shared between the socket drainer (writer), the browse view (reads len
/// for the badge), and the queue view (reads and removes). The lock is
/// short-lived, so `std::sync::Mutex` keeps things simple.
pub type Queue = Arc<Mutex<VecDeque<QueuedEvent>>>;

pub fn new_queue() -> Queue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Append an event, stamped with the current time.
pub fn push_event(queue: &Queue, event: ApprovalEvent) {
    let queued = QueuedEvent {
        event,
        received_at: SystemTime::now(),
    };
    if let Ok(mut q) = queue.lock() {
        q.push_back(queued);
    }
}

/// Queue-view intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Up,
    Down,
    Top,
    Bottom,
    Drop,
    Open,
    Back,
    Quit,
}

/// What the frontend should do after [`QueueView::apply`].
#[derive(Debug, PartialEq, Eq)]
pub enum QueueStep {
    Continue,
    /// Open the approve modal for this grant name, then report the result
    /// with [`QueueView::finish_review`].
    Open(String),
    /// Return to the browse view.
    Back,
    /// Tear down the whole app.
    Quit,
}

/// Cursor over a snapshot of the shared approval queue.
pub struct QueueView {
    queue: Queue,
    snapshot: Vec<QueuedEvent>,
    cursor: usize,
}

impl QueueView {
    pub fn new(queue: Queue) -> Self {
        Self {
            queue,
            snapshot: Vec::new(),
            cursor: 0,
        }
    }

    /// Re-read the shared queue and clamp the cursor. Returns `false` when the
    /// queue is empty, in which case the view should close.
    pub fn sync(&mut self) -> bool {
        self.snapshot = self.queue.lock().unwrap().iter().cloned().collect();
        if self.snapshot.is_empty() {
            return false;
        }
        if self.cursor >= self.snapshot.len() {
            self.cursor = self.snapshot.len() - 1;
        }
        true
    }

    /// Rows as of the last [`QueueView::sync`].
    pub fn rows(&self) -> &[QueuedEvent] {
        &self.snapshot
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Move the cursor to `pos`, clamped to the rows of the last
    /// [`QueueView::sync`]. Used for pointer selection.
    pub fn set_cursor(&mut self, pos: usize) {
        self.cursor = pos.min(self.snapshot.len().saturating_sub(1));
    }

    pub fn apply(&mut self, action: QueueAction) -> QueueStep {
        match action {
            QueueAction::Up => self.cursor = self.cursor.saturating_sub(1),
            QueueAction::Down => {
                if self.cursor + 1 < self.snapshot.len() {
                    self.cursor += 1;
                }
            }
            QueueAction::Top => self.cursor = 0,
            QueueAction::Bottom => self.cursor = self.snapshot.len().saturating_sub(1),
            QueueAction::Drop => self.remove_current(),
            QueueAction::Open => {
                if let Some(q) = self.snapshot.get(self.cursor) {
                    return QueueStep::Open(q.event.name.clone());
                }
            }
            QueueAction::Back => return QueueStep::Back,
            QueueAction::Quit => return QueueStep::Quit,
        }
        QueueStep::Continue
    }

    /// Record how the approve modal for the current row ended. Rows that
    /// reached PAM are removed. Canceled or failed rows stay so the user can
    /// retry.
    pub fn finish_review(&mut self, outcome: &ApproveOutcome) {
        if should_drop(outcome) {
            self.remove_current();
        }
    }

    fn remove_current(&mut self) {
        let mut q = self.queue.lock().unwrap();
        if self.cursor < q.len() {
            q.remove(self.cursor);
        }
    }
}

/// Should the row be removed from the queue after a modal session?
/// Anything that reached PAM (approved, denied, already handled by someone
/// else) is done.
fn should_drop(outcome: &ApproveOutcome) -> bool {
    matches!(
        outcome,
        ApproveOutcome::Approved | ApproveOutcome::Denied | ApproveOutcome::AlreadyHandled
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRANT_A: &str = "projects/fake-proj-1/locations/global/entitlements/ent-a/grants/g-0001";
    const GRANT_B: &str = "folders/111111/locations/global/entitlements/ent-b/grants/g-0002";

    fn queue_with(names: &[&str]) -> Queue {
        let q = new_queue();
        for n in names {
            push_event(
                &q,
                ApprovalEvent {
                    name: (*n).into(),
                    source: None,
                },
            );
        }
        q
    }

    #[test]
    fn empty_queue_closes_view() {
        let mut view = QueueView::new(new_queue());
        assert!(!view.sync());
    }

    #[test]
    fn open_returns_name_under_cursor() {
        let mut view = QueueView::new(queue_with(&[GRANT_A, GRANT_B]));
        assert!(view.sync());
        assert_eq!(view.apply(QueueAction::Down), QueueStep::Continue);
        assert_eq!(
            view.apply(QueueAction::Open),
            QueueStep::Open(GRANT_B.into())
        );
        assert_eq!(
            view.rows()[0].display_parts(),
            ("ent-a", "proj", "fake-proj-1")
        );
    }

    #[test]
    fn set_cursor_clamps_to_snapshot() {
        let mut view = QueueView::new(queue_with(&[GRANT_A, GRANT_B]));
        view.sync();
        view.set_cursor(1);
        assert_eq!(
            view.apply(QueueAction::Open),
            QueueStep::Open(GRANT_B.into())
        );
        view.set_cursor(7);
        assert_eq!(view.cursor(), 1);
    }

    #[test]
    fn handled_rows_are_dropped_and_cursor_clamps() {
        let queue = queue_with(&[GRANT_A, GRANT_B]);
        let mut view = QueueView::new(queue.clone());
        view.sync();
        view.apply(QueueAction::Bottom);
        view.finish_review(&ApproveOutcome::Canceled);
        assert_eq!(queue.lock().unwrap().len(), 2);
        view.finish_review(&ApproveOutcome::Approved);
        assert_eq!(queue.lock().unwrap().len(), 1);
        assert!(view.sync());
        assert_eq!(view.cursor(), 0);
        view.apply(QueueAction::Drop);
        assert!(!view.sync());
    }
}
