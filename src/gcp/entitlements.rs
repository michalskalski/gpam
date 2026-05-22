use anyhow::Result;
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_privilegedaccessmanager_v1::client::PrivilegedAccessManager;
use google_cloud_privilegedaccessmanager_v1::model::Entitlement;
use google_cloud_privilegedaccessmanager_v1::model::search_entitlements_request::CallerAccessType;

use crate::backend::ScopeTarget;
use crate::cache::{EntitlementRow, now_unix};
use crate::gcp::Scope;

const LOCATION: &str = "global";

/// Build the PAM `parent` URI for a given scope. PAM uses
/// `{projects|folders|organizations}/{id}/locations/{location}` uniformly.
pub fn parent(scope: Scope, id: &str) -> String {
    format!("{}/{}/locations/{}", scope.plural(), id, LOCATION)
}

/// Search PAM for the entitlements the caller can request as `GRANT_REQUESTER`
/// under a single scope.
pub async fn search_one(
    client: &PrivilegedAccessManager,
    target: &ScopeTarget,
) -> Result<Vec<EntitlementRow>> {
    let mut stream = client
        .search_entitlements()
        .set_parent(parent(target.scope, &target.id))
        .set_caller_access_type(CallerAccessType::GrantRequester)
        .by_item();

    let fetched_at = now_unix();
    let mut rows = Vec::new();
    while let Some(ent) = stream.next().await {
        let ent = ent?;
        rows.push(to_row(ent, target, fetched_at));
    }
    Ok(rows)
}

fn to_row(ent: Entitlement, target: &ScopeTarget, fetched_at: i64) -> EntitlementRow {
    let short_name = ent.name.rsplit('/').next().unwrap_or("").to_string();

    // Justification is required unless `requester_justification_config.not_mandatory` is set.
    let justification_required = ent
        .requester_justification_config
        .as_ref()
        .and_then(|c| c.not_mandatory())
        .is_none();

    let max_request_duration_secs = ent.max_request_duration.as_ref().map(|d| d.seconds());

    let approvers: Vec<String> = ent
        .approval_workflow
        .as_ref()
        .and_then(|w| w.manual_approvals())
        .map(|m| {
            m.steps
                .iter()
                .flat_map(|s| s.approvers.iter())
                .flat_map(|a| a.principals.iter().cloned())
                .collect()
        })
        .unwrap_or_default();

    let roles: Vec<String> = ent
        .privileged_access
        .as_ref()
        .and_then(|p| p.gcp_iam_access())
        .map(|g| g.role_bindings.iter().map(|b| b.role.clone()).collect())
        .unwrap_or_default();

    let raw_json = serde_json::to_string(&ent).unwrap_or_else(|_| "{}".into());

    EntitlementRow {
        name: ent.name,
        scope_type: target.scope,
        scope_id: target.id.clone(),
        scope_display_name: target.display_name.clone(),
        short_name,
        max_request_duration_secs,
        justification_required,
        approvers,
        roles,
        raw_json,
        fetched_at,
        last_used_at: None,
    }
}
