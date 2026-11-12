use crate::cache::{EntitlementRow, GrantRow, now_unix};
use crate::gcp::grants::{GrantState, MIN_DURATION_SECS, parse_duration};
use crate::poller::{GrantUpdate, PollMode, Poller};

use super::format::format_age;
use super::text_input::{EditAction, TextInput};

/// Which input of the request modal has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Duration,
    Justification,
}

/// Request-modal intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAction {
    /// Stop the whole flow. Already-submitted grants stay submitted.
    Abort,
    /// Skip the current entitlement and move to the next one.
    Skip,
    /// Confirm the focused field.
    Confirm,
    /// Move focus to the other field.
    SwapField,
    /// Edit the focused field.
    Edit(EditAction),
}

/// What the frontend should do after [`RequestFlow::apply`].
// Returned once per input event, so the size of `Submit` does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum RequestStep {
    Continue,
    /// Pass this to [`submit`], then hand its result to
    /// [`RequestFlow::complete`].
    Submit(Submission),
    /// The user aborted the flow.
    Abort,
}

/// A validated request ready to send to PAM.
#[derive(Debug, Clone)]
pub struct Submission {
    pub entitlement: EntitlementRow,
    pub duration_secs: i64,
    pub justification: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RequestResult {
    pub short_name: String,
    pub state: ResultState,
}

#[derive(Debug, Clone)]
pub enum ResultState {
    /// Carries the created grant's resource name.
    Ok(String),
    Err(String),
    Skipped,
}

impl RequestResult {
    fn ok(short: String, grant: String) -> Self {
        Self {
            short_name: short,
            state: ResultState::Ok(grant),
        }
    }
    fn err(short: String, msg: String) -> Self {
        Self {
            short_name: short,
            state: ResultState::Err(msg),
        }
    }
    fn skipped(short: String) -> Self {
        Self {
            short_name: short,
            state: ResultState::Skipped,
        }
    }
}

/// Duration and justification prompts for one entitlement.
pub struct RequestModal {
    entitlement: EntitlementRow,
    duration: TextInput,
    justification: TextInput,
    field: Field,
    error: Option<String>,
}

enum ModalOutcome {
    Submit {
        duration_secs: i64,
        justification: Option<String>,
    },
    Continue,
}

impl RequestModal {
    fn new(
        entitlement: EntitlementRow,
        default_duration: &str,
        default_justification: &str,
    ) -> Self {
        Self {
            duration: TextInput::with_text(default_duration),
            justification: TextInput::with_text(default_justification),
            field: Field::Duration,
            error: None,
            entitlement,
        }
    }

    pub fn entitlement(&self) -> &EntitlementRow {
        &self.entitlement
    }

    pub fn duration(&self) -> &TextInput {
        &self.duration
    }

    pub fn justification(&self) -> &TextInput {
        &self.justification
    }

    pub fn field(&self) -> Field {
        self.field
    }

    /// Validation message from the last confirm, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// `scope: <plural>/<id>` with the display name appended when known.
    pub fn scope_line(&self) -> String {
        let ent = &self.entitlement;
        match &ent.scope_display_name {
            Some(name) if !name.is_empty() => format!(
                "scope: {}/{} ({})",
                ent.scope_type.plural(),
                ent.scope_id,
                name,
            ),
            _ => format!("scope: {}/{}", ent.scope_type.plural(), ent.scope_id),
        }
    }

    /// Allowed duration range, for example `min 30m | max 4h`.
    pub fn limits_line(&self) -> String {
        match self.entitlement.max_request_duration_secs {
            Some(s) => format!(
                "min {} | max {}",
                format_age(MIN_DURATION_SECS),
                format_age(s)
            ),
            None => format!("min {}", format_age(MIN_DURATION_SECS)),
        }
    }

    pub fn validate_duration(&self) -> Result<i64, String> {
        validate_duration(
            self.duration.as_str(),
            self.entitlement.max_request_duration_secs,
        )
    }

    fn apply(&mut self, action: RequestAction) -> ModalOutcome {
        let need_justification = self.entitlement.justification_required;
        match (self.field, action) {
            (Field::Duration, RequestAction::Confirm) => match self.validate_duration() {
                Ok(_) => {
                    self.error = None;
                    if need_justification {
                        self.field = Field::Justification;
                    } else {
                        return self.submit();
                    }
                }
                Err(e) => self.error = Some(e),
            },
            (Field::Justification, RequestAction::Confirm) => {
                if need_justification && self.justification.as_str().trim().is_empty() {
                    self.error = Some("justification cannot be empty".into());
                } else {
                    return self.submit();
                }
            }
            (Field::Duration, RequestAction::SwapField) => self.field = Field::Justification,
            (Field::Justification, RequestAction::SwapField) => self.field = Field::Duration,
            (Field::Duration, RequestAction::Edit(edit)) => self.duration.apply(edit),
            (Field::Justification, RequestAction::Edit(edit)) => self.justification.apply(edit),
            (_, RequestAction::Abort | RequestAction::Skip) => {}
        }
        ModalOutcome::Continue
    }

    fn submit(&self) -> ModalOutcome {
        match self.validate_duration() {
            Ok(secs) => ModalOutcome::Submit {
                duration_secs: secs,
                justification: if self.entitlement.justification_required {
                    Some(self.justification.as_str().to_string())
                } else {
                    None
                },
            },
            Err(_) => ModalOutcome::Continue,
        }
    }
}

/// Parse and bound-check a typed duration. `max_secs` of `None` or `<= 0`
/// means the entitlement sets no upper bound.
pub fn validate_duration(input: &str, max_secs: Option<i64>) -> Result<i64, String> {
    let secs = parse_duration(input)
        .ok_or_else(|| format!("invalid duration '{input}'; try 1h, 30m, 5400s, 3600"))?;
    if secs < MIN_DURATION_SECS {
        return Err(format!("below minimum {}", format_age(MIN_DURATION_SECS)));
    }
    if let Some(max) = max_secs
        && max > 0
        && secs > max
    {
        return Err(format!("exceeds max {}", format_age(max)));
    }
    Ok(secs)
}

/// Reuse the previous duration input if it still passes this entitlement's
/// bounds. Carry the raw typed string so the prefill matches what the user
/// actually typed (e.g. "30m") rather than a reformatted "1800s".
pub fn pick_default_duration(ent: &EntitlementRow, last: Option<&(String, i64)>) -> String {
    if let Some((typed, secs)) = last
        && *secs >= MIN_DURATION_SECS
        && ent
            .max_request_duration_secs
            .is_none_or(|max| max <= 0 || *secs <= max)
    {
        return typed.clone();
    }
    "1h".into()
}

/// Walks the user through the request modal for each selected entitlement,
/// one at a time.
pub struct RequestFlow {
    entitlements: Vec<EntitlementRow>,
    index: usize,
    modal: Option<RequestModal>,
    results: Vec<RequestResult>,
    // Carry the previous submission across entitlements so a user requesting
    // many grants doesn't have to retype the same values. The duration
    // prefill is dropped if it would violate the next entitlement's bounds.
    last_duration: Option<(String, i64)>,
    last_justification: Option<String>,
}

impl RequestFlow {
    pub fn new(selected: Vec<EntitlementRow>) -> Self {
        let mut flow = Self {
            entitlements: selected,
            index: 0,
            modal: None,
            results: Vec::new(),
            last_duration: None,
            last_justification: None,
        };
        flow.open_current();
        flow
    }

    fn open_current(&mut self) {
        self.modal = self.entitlements.get(self.index).map(|ent| {
            let default_duration = pick_default_duration(ent, self.last_duration.as_ref());
            let default_justification = self.last_justification.clone().unwrap_or_default();
            RequestModal::new(ent.clone(), &default_duration, &default_justification)
        });
    }

    fn advance(&mut self, result: RequestResult) {
        self.results.push(result);
        self.index += 1;
        self.open_current();
    }

    /// The modal for the current entitlement, or `None` once every
    /// entitlement has been submitted or skipped.
    pub fn current(&self) -> Option<&RequestModal> {
        self.modal.as_ref()
    }

    /// Zero-based index of the current entitlement.
    pub fn position(&self) -> usize {
        self.index
    }

    pub fn total(&self) -> usize {
        self.entitlements.len()
    }

    pub fn results(&self) -> &[RequestResult] {
        &self.results
    }

    pub fn has_errors(&self) -> bool {
        self.results
            .iter()
            .any(|r| matches!(r.state, ResultState::Err(_)))
    }

    /// `(ok, error, skipped)` counts.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for r in &self.results {
            match r.state {
                ResultState::Ok(_) => counts.0 += 1,
                ResultState::Err(_) => counts.1 += 1,
                ResultState::Skipped => counts.2 += 1,
            }
        }
        counts
    }

    /// Resource names of the grants created so far.
    pub fn grant_names(&self) -> Vec<String> {
        self.results
            .iter()
            .filter_map(|r| match &r.state {
                ResultState::Ok(name) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn apply(&mut self, action: RequestAction) -> RequestStep {
        let Some(modal) = self.modal.as_mut() else {
            return RequestStep::Continue;
        };
        match action {
            RequestAction::Abort => return RequestStep::Abort,
            RequestAction::Skip => {
                let short = modal.entitlement.short_name.clone();
                self.advance(RequestResult::skipped(short));
                return RequestStep::Continue;
            }
            _ => {}
        }
        match modal.apply(action) {
            ModalOutcome::Continue => RequestStep::Continue,
            ModalOutcome::Submit {
                duration_secs,
                justification,
            } => {
                self.last_duration = Some((modal.duration.as_str().to_string(), duration_secs));
                if let Some(j) = &justification {
                    self.last_justification = Some(j.clone());
                }
                RequestStep::Submit(Submission {
                    entitlement: modal.entitlement.clone(),
                    duration_secs,
                    justification,
                })
            }
        }
    }

    /// Record the outcome of [`submit`] for the current entitlement and move
    /// to the next one.
    pub fn complete(&mut self, result: RequestResult) {
        if self.modal.is_some() {
            self.advance(result);
        }
    }
}

/// Create the grant. On success the grant is persisted to the cache and a
/// background poller is spawned, with state changes forwarded through the
/// poller's channel so the browse view can show them.
pub async fn submit(poller: &Poller, sub: Submission) -> RequestResult {
    let ent = &sub.entitlement;
    match poller
        .backend
        .create_grant(&ent.name, sub.duration_secs, sub.justification.as_deref())
        .await
    {
        Ok(name) => {
            let now = now_unix();
            let short_id = name.rsplit('/').next().unwrap_or("").to_string();
            let grant_row = GrantRow {
                name: name.clone(),
                entitlement_name: ent.name.clone(),
                entitlement_short_name: ent.short_name.clone(),
                scope_type: ent.scope_type,
                scope_id: ent.scope_id.clone(),
                scope_display_name: ent.scope_display_name.clone(),
                short_id,
                state: GrantState::Requested,
                requested_duration_secs: sub.duration_secs,
                justification: sub.justification.clone(),
                created_at: now,
                activated_at: None,
                expires_at: None,
                last_polled_at: None,
                raw_json: None,
            };
            {
                let cache = poller.cache.lock().await;
                let _ = cache.bump_used(&ent.name, now);
                let _ = cache.upsert_grant(&grant_row);
            }
            // Surface the new row to the strip immediately, then spawn a
            // poller. The first poll observation overwrites this provisional
            // state via the same channel.
            let _ = poller.tx.send(GrantUpdate).await;
            tokio::spawn(poller.clone().poll_grant(
                name.clone(),
                sub.duration_secs,
                grant_row.state.clone(),
                PollMode::Track,
            ));
            RequestResult::ok(ent.short_name.clone(), name)
        }
        Err(e) => RequestResult::err(ent.short_name.clone(), format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::Scope;

    fn ent(short: &str, max: Option<i64>, justification_required: bool) -> EntitlementRow {
        EntitlementRow {
            name: format!("projects/fake-proj-1/locations/global/entitlements/{short}"),
            scope_type: Scope::Project,
            scope_id: "fake-proj-1".into(),
            scope_display_name: None,
            short_name: short.into(),
            max_request_duration_secs: max,
            justification_required,
            approvers: Vec::new(),
            roles: Vec::new(),
            raw_json: "{}".into(),
            fetched_at: 0,
            last_used_at: None,
        }
    }

    fn type_into(flow: &mut RequestFlow, s: &str) {
        for c in s.chars() {
            flow.apply(RequestAction::Edit(EditAction::Insert(c)));
        }
    }

    fn clear_field(flow: &mut RequestFlow) {
        for _ in 0..16 {
            flow.apply(RequestAction::Edit(EditAction::Backspace));
        }
    }

    #[test]
    fn validate_duration_bounds() {
        assert_eq!(validate_duration("1h", Some(4 * 3600)), Ok(3600));
        assert_eq!(validate_duration("5400", None), Ok(5400));
        assert_eq!(
            validate_duration("10m", None),
            Err("below minimum 30m".into())
        );
        assert_eq!(
            validate_duration("5h", Some(4 * 3600)),
            Err("exceeds max 4h".into())
        );
        // A non-positive max means no upper bound.
        assert_eq!(validate_duration("48h", Some(0)), Ok(48 * 3600));
        assert!(
            validate_duration("soon", None)
                .unwrap_err()
                .contains("invalid duration")
        );
    }

    #[test]
    fn pick_default_duration_reuses_last_when_in_bounds() {
        let tight = ent("tight", Some(3600), false);
        let open = ent("open", None, false);
        assert_eq!(pick_default_duration(&open, None), "1h");
        let last = ("2h".to_string(), 2 * 3600);
        assert_eq!(pick_default_duration(&open, Some(&last)), "2h");
        assert_eq!(pick_default_duration(&tight, Some(&last)), "1h");
        let short = ("10m".to_string(), 600);
        assert_eq!(pick_default_duration(&open, Some(&short)), "1h");
    }

    #[test]
    fn flow_submits_skips_and_carries_values() {
        let mut flow = RequestFlow::new(vec![
            ent("first", None, true),
            ent("second", Some(4 * 3600), true),
            ent("third", Some(3600), false),
        ]);
        assert_eq!(flow.current().unwrap().duration().as_str(), "1h");

        // Duration first, then justification is required.
        clear_field(&mut flow);
        type_into(&mut flow, "2h");
        assert!(matches!(
            flow.apply(RequestAction::Confirm),
            RequestStep::Continue
        ));
        assert_eq!(flow.current().unwrap().field(), Field::Justification);
        assert!(matches!(
            flow.apply(RequestAction::Confirm),
            RequestStep::Continue
        ));
        assert_eq!(
            flow.current().unwrap().error(),
            Some("justification cannot be empty")
        );
        type_into(&mut flow, "debugging");
        let RequestStep::Submit(sub) = flow.apply(RequestAction::Confirm) else {
            panic!("expected a submission");
        };
        assert_eq!(sub.duration_secs, 2 * 3600);
        assert_eq!(sub.justification.as_deref(), Some("debugging"));
        flow.complete(RequestResult::ok("first".into(), "g-1".into()));

        // Second entitlement inherits both values.
        assert_eq!(flow.position(), 1);
        let modal = flow.current().unwrap();
        assert_eq!(modal.duration().as_str(), "2h");
        assert_eq!(modal.justification().as_str(), "debugging");
        assert!(matches!(
            flow.apply(RequestAction::Skip),
            RequestStep::Continue
        ));

        // Third caps at 1h, so the carried 2h is dropped. No justification
        // is needed, so confirming the duration submits right away.
        assert_eq!(flow.current().unwrap().duration().as_str(), "1h");
        let RequestStep::Submit(sub) = flow.apply(RequestAction::Confirm) else {
            panic!("expected a submission");
        };
        assert_eq!(sub.justification, None);
        flow.complete(RequestResult::err("third".into(), "boom".into()));

        assert!(flow.current().is_none());
        assert_eq!(flow.counts(), (1, 1, 1));
        assert!(flow.has_errors());
        assert_eq!(flow.grant_names(), vec!["g-1".to_string()]);
    }

    #[test]
    fn invalid_duration_sets_error_and_abort_stops() {
        let mut flow = RequestFlow::new(vec![ent("only", Some(3600), false)]);
        clear_field(&mut flow);
        type_into(&mut flow, "2h");
        assert!(matches!(
            flow.apply(RequestAction::Confirm),
            RequestStep::Continue
        ));
        assert!(
            flow.current()
                .unwrap()
                .error()
                .unwrap()
                .starts_with("exceeds max")
        );
        assert!(matches!(
            flow.apply(RequestAction::Abort),
            RequestStep::Abort
        ));
        assert!(flow.results().is_empty());
    }
}
