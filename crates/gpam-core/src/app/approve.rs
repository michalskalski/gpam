use anyhow::Result;

use crate::backend::DynBackend;
use crate::gcp::grants::{GrantDetails, RoleBindingView};

use super::format::format_age;
use super::text_input::{EditAction, TextInput};

/// How the approve modal ended. The caller (queue or CLI) uses this to
/// decide whether to drop the queued row and whether to keep the app alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproveOutcome {
    Approved,
    Denied,
    /// Grant was already in a terminal state. Nothing to do here.
    AlreadyHandled,
    /// User backed out without acting.
    Canceled,
    /// Submission failed for a reason other than "already handled".
    Failed,
    /// User asked to quit the whole app.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
}

impl Decision {
    pub fn label(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Deny => "deny",
        }
    }

    /// `approved` or `denied`.
    pub fn past_tense(self) -> &'static str {
        match self {
            Decision::Approve => "approved",
            Decision::Deny => "denied",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Grant fetched and actionable. User picks approve, deny or cancel.
    Confirm,
    /// Decision picked, collecting an optional reason.
    Reason(Decision),
    /// Final state: submit succeeded, already handled, or fetch/submit
    /// errored. User dismisses it.
    Result(StepResult),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    Ok(Decision),
    AlreadyHandled,
    Err(String),
}

impl StepResult {
    /// Bracketed headline tag, for example `[approve]` or `[err]`.
    pub fn tag(&self) -> String {
        match self {
            StepResult::Ok(decision) => format!("[{}]", decision.label()),
            StepResult::AlreadyHandled => "[already handled]".into(),
            StepResult::Err(_) => "[err]".into(),
        }
    }

    /// Sentence describing the result for the grant of `entitlement`.
    pub fn message(&self, entitlement: &str) -> String {
        match self {
            StepResult::Ok(decision) => {
                format!("grant {entitlement} {}.", decision.past_tense())
            }
            StepResult::AlreadyHandled => {
                format!("grant {entitlement} was already approved or denied by someone else.")
            }
            StepResult::Err(msg) => msg.clone(),
        }
    }
}

/// Labelled single-value rows describing a grant for the confirm step:
/// requester, scope, entitlement, duration and state.
pub fn detail_fields(d: &GrantDetails) -> Vec<(&'static str, String)> {
    vec![
        ("requester", d.requester.clone()),
        ("scope", format!("{}/{}", d.scope_type.plural(), d.scope_id)),
        ("entitlement", d.entitlement_short_name.clone()),
        ("duration", format_age(d.requested_duration_secs)),
        ("state", d.state.to_string()),
    ]
}

/// A role binding as one line, with its condition in parentheses.
pub fn role_binding_label(rb: &RoleBindingView) -> String {
    match &rb.condition {
        Some(c) => format!("{}  ({})", rb.role, c),
        None => rb.role.clone(),
    }
}

/// Approve-modal intents. Each one is only meaningful in some steps and is
/// ignored in the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproveAction {
    /// Quit the whole app. Valid in every step.
    Quit,
    /// Close without acting. Valid in [`Step::Confirm`].
    Cancel,
    /// Pick a decision and move to the reason prompt. Valid in [`Step::Confirm`].
    Choose(Decision),
    /// Drop the reason and return to [`Step::Confirm`]. Valid in [`Step::Reason`].
    Back,
    /// Send the decision to PAM. Valid in [`Step::Reason`].
    Submit,
    /// Edit the reason. Valid in [`Step::Reason`].
    Edit(EditAction),
    /// Close the result. Valid in [`Step::Result`].
    Dismiss,
}

/// What the frontend should do after [`ApproveModal::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproveStep {
    Continue,
    /// Call [`ApproveModal::submit`] next.
    Submit,
    Close(ApproveOutcome),
}

/// Approve/deny state machine for one grant.
pub struct ApproveModal {
    reason: TextInput,
    step: Step,
    /// `None` only when the initial fetch failed. In that case the modal
    /// lands directly in `Step::Result(Err)`.
    details: Option<GrantDetails>,
}

impl ApproveModal {
    /// Fetch the grant and open the modal. On fetch error or a terminal-state
    /// grant the modal opens in the result step.
    pub async fn load(backend: &DynBackend, name: &str) -> Self {
        Self::from_fetch(backend.get_grant_details(name).await)
    }

    pub fn from_fetch(fetched: Result<GrantDetails>) -> Self {
        let (step, details) = match fetched {
            Ok(d) if d.state.is_terminal() => (Step::Result(StepResult::AlreadyHandled), Some(d)),
            Ok(d) => (Step::Confirm, Some(d)),
            Err(e) => (
                Step::Result(StepResult::Err(format!("fetch failed: {e:#}"))),
                None,
            ),
        };
        Self {
            reason: TextInput::new(),
            step,
            details,
        }
    }

    pub fn step(&self) -> &Step {
        &self.step
    }

    pub fn details(&self) -> Option<&GrantDetails> {
        self.details.as_ref()
    }

    pub fn reason(&self) -> &TextInput {
        &self.reason
    }

    /// Replace the whole reason, for frontends with a native text field.
    /// Like typing, it only has an effect in [`Step::Reason`].
    pub fn set_reason(&mut self, text: &str) {
        if matches!(self.step, Step::Reason(_)) {
            self.reason.set_text(text);
        }
    }

    pub fn apply(&mut self, action: ApproveAction) -> ApproveStep {
        if action == ApproveAction::Quit {
            return ApproveStep::Close(ApproveOutcome::Quit);
        }
        match (&self.step, action) {
            (Step::Confirm, ApproveAction::Cancel) => {
                return ApproveStep::Close(ApproveOutcome::Canceled);
            }
            (Step::Confirm, ApproveAction::Choose(decision)) => {
                self.step = Step::Reason(decision);
            }
            (Step::Reason(_), ApproveAction::Back) => {
                self.reason.clear();
                self.step = Step::Confirm;
            }
            (Step::Reason(_), ApproveAction::Submit) => return ApproveStep::Submit,
            (Step::Reason(_), ApproveAction::Edit(edit)) => self.reason.apply(edit),
            (Step::Result(result), ApproveAction::Dismiss) => {
                return ApproveStep::Close(match result {
                    StepResult::Ok(Decision::Approve) => ApproveOutcome::Approved,
                    StepResult::Ok(Decision::Deny) => ApproveOutcome::Denied,
                    StepResult::AlreadyHandled => ApproveOutcome::AlreadyHandled,
                    StepResult::Err(_) => ApproveOutcome::Failed,
                });
            }
            _ => {}
        }
        ApproveStep::Continue
    }

    /// Send the picked decision to PAM and move to the result step. Does
    /// nothing outside [`Step::Reason`].
    pub async fn submit(&mut self, backend: &DynBackend) {
        let Step::Reason(decision) = self.step else {
            return;
        };
        let Some(details) = &self.details else {
            self.step = Step::Result(StepResult::Err("no grant loaded".into()));
            return;
        };
        let reason = {
            let r = self.reason.as_str().trim();
            if r.is_empty() { None } else { Some(r) }
        };
        let result = match decision {
            Decision::Approve => backend.approve_grant(&details.name, reason).await,
            Decision::Deny => backend.deny_grant(&details.name, reason).await,
        };
        self.step = match result {
            Ok(()) => Step::Result(StepResult::Ok(decision)),
            Err(e) => {
                // Refetch to distinguish "someone else handled it" from a real
                // failure. Racing another approver is a legitimate outcome,
                // not a retry-worthy error.
                let already_handled = backend
                    .get_grant_details(&details.name)
                    .await
                    .map(|d| d.state.is_terminal())
                    .unwrap_or(false);
                if already_handled {
                    Step::Result(StepResult::AlreadyHandled)
                } else {
                    Step::Result(StepResult::Err(format!("{e:#}")))
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::anyhow;

    use super::*;
    use crate::backend::demo::DemoBackend;
    use crate::gcp::grants::GrantState;

    const GRANT: &str = "projects/fake-proj-1/locations/global/entitlements/ent-a/grants/g-0001";

    fn backend() -> DynBackend {
        Arc::new(DemoBackend::new())
    }

    #[tokio::test]
    async fn approve_with_reason_reaches_approved() {
        let backend = backend();
        let mut modal = ApproveModal::load(&backend, GRANT).await;
        assert_eq!(modal.step(), &Step::Confirm);
        assert_eq!(
            modal.apply(ApproveAction::Choose(Decision::Approve)),
            ApproveStep::Continue
        );
        for c in "ok".chars() {
            modal.apply(ApproveAction::Edit(EditAction::Insert(c)));
        }
        assert_eq!(modal.reason().as_str(), "ok");
        assert_eq!(modal.apply(ApproveAction::Submit), ApproveStep::Submit);
        modal.submit(&backend).await;
        assert_eq!(
            modal.step(),
            &Step::Result(StepResult::Ok(Decision::Approve))
        );
        assert_eq!(
            modal.apply(ApproveAction::Dismiss),
            ApproveStep::Close(ApproveOutcome::Approved)
        );
    }

    #[tokio::test]
    async fn back_clears_reason_and_cancel_closes() {
        let backend = backend();
        let mut modal = ApproveModal::load(&backend, GRANT).await;
        modal.apply(ApproveAction::Choose(Decision::Deny));
        modal.apply(ApproveAction::Edit(EditAction::Insert('x')));
        // Dismiss is not valid while collecting a reason.
        assert_eq!(modal.apply(ApproveAction::Dismiss), ApproveStep::Continue);
        modal.apply(ApproveAction::Back);
        assert_eq!(modal.step(), &Step::Confirm);
        assert!(modal.reason().is_empty());
        assert_eq!(
            modal.apply(ApproveAction::Cancel),
            ApproveStep::Close(ApproveOutcome::Canceled)
        );
    }

    #[tokio::test]
    async fn set_reason_only_applies_while_collecting_a_reason() {
        let backend = backend();
        let mut modal = ApproveModal::load(&backend, GRANT).await;
        modal.set_reason("too early");
        assert!(modal.reason().is_empty());
        modal.apply(ApproveAction::Choose(Decision::Deny));
        modal.set_reason("not on call");
        assert_eq!(modal.reason().as_str(), "not on call");
        modal.apply(ApproveAction::Edit(EditAction::Insert('!')));
        assert_eq!(modal.reason().as_str(), "not on call!");
    }

    #[test]
    fn result_and_detail_text() {
        assert_eq!(StepResult::Ok(Decision::Approve).tag(), "[approve]");
        assert_eq!(
            StepResult::Ok(Decision::Deny).message("ent-a"),
            "grant ent-a denied."
        );
        assert_eq!(StepResult::AlreadyHandled.tag(), "[already handled]");
        assert_eq!(
            StepResult::AlreadyHandled.message("ent-a"),
            "grant ent-a was already approved or denied by someone else."
        );
        assert_eq!(StepResult::Err("boom".into()).message("ent-a"), "boom");

        let details = GrantDetails {
            name: GRANT.into(),
            state: GrantState::ApprovalAwaited,
            requester: "user@example.test".into(),
            requested_duration_secs: 7200,
            justification: None,
            role_bindings: Vec::new(),
            scope_type: crate::gcp::Scope::Project,
            scope_id: "fake-proj-1".into(),
            entitlement_short_name: "ent-a".into(),
        };
        assert_eq!(
            detail_fields(&details),
            vec![
                ("requester", "user@example.test".to_string()),
                ("scope", "projects/fake-proj-1".to_string()),
                ("entitlement", "ent-a".to_string()),
                ("duration", "2h".to_string()),
                ("state", "ApprovalAwaited".to_string()),
            ]
        );
        let plain = RoleBindingView {
            role: "roles/viewer".into(),
            condition: None,
        };
        let conditional = RoleBindingView {
            role: "roles/editor".into(),
            condition: Some("request.time < x".into()),
        };
        assert_eq!(role_binding_label(&plain), "roles/viewer");
        assert_eq!(
            role_binding_label(&conditional),
            "roles/editor  (request.time < x)"
        );
    }

    #[tokio::test]
    async fn deny_reaches_denied() {
        let backend = backend();
        let mut modal = ApproveModal::load(&backend, GRANT).await;
        modal.apply(ApproveAction::Choose(Decision::Deny));
        modal.apply(ApproveAction::Submit);
        modal.submit(&backend).await;
        assert_eq!(
            modal.apply(ApproveAction::Dismiss),
            ApproveStep::Close(ApproveOutcome::Denied)
        );
    }

    #[test]
    fn fetch_failure_and_terminal_grant_open_in_result() {
        let mut failed = ApproveModal::from_fetch(Err(anyhow!("boom")));
        assert!(failed.details().is_none());
        assert_eq!(
            failed.apply(ApproveAction::Choose(Decision::Approve)),
            ApproveStep::Continue
        );
        assert_eq!(
            failed.apply(ApproveAction::Dismiss),
            ApproveStep::Close(ApproveOutcome::Failed)
        );

        let details = GrantDetails {
            name: GRANT.into(),
            state: GrantState::Denied,
            requester: "user@example.test".into(),
            requested_duration_secs: 3600,
            justification: None,
            role_bindings: Vec::new(),
            scope_type: crate::gcp::Scope::Project,
            scope_id: "fake-proj-1".into(),
            entitlement_short_name: "ent-a".into(),
        };
        let mut handled = ApproveModal::from_fetch(Ok(details));
        assert_eq!(
            handled.apply(ApproveAction::Dismiss),
            ApproveStep::Close(ApproveOutcome::AlreadyHandled)
        );
    }

    #[tokio::test]
    async fn quit_closes_from_any_step() {
        let backend = backend();
        let mut modal = ApproveModal::load(&backend, GRANT).await;
        modal.apply(ApproveAction::Choose(Decision::Approve));
        assert_eq!(
            modal.apply(ApproveAction::Quit),
            ApproveStep::Close(ApproveOutcome::Quit)
        );
    }
}
