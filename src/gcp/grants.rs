use std::str::FromStr;

use anyhow::{Context, Result};
use google_cloud_privilegedaccessmanager_v1::client::PrivilegedAccessManager;
use google_cloud_privilegedaccessmanager_v1::model::grant;
use google_cloud_privilegedaccessmanager_v1::model::{Grant, Justification};
use google_cloud_wkt::Duration;
use rusqlite::ToSql;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};

use crate::gcp::Scope;

/// PAM grant state as understood by gpam.
///
/// Mirrors the variants from PAM v1 (`grant::State`) plus a local `Requested`
/// placeholder for grants that have been submitted but not yet observed via a
/// poll. The `Unknown` arm captures any future SDK variant by name so an
/// upgraded server doesn't break us.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantState {
    /// Local placeholder. Never returned by GCP. Set when we have just created
    /// a grant and have not yet polled its state from the server.
    Requested,
    ApprovalAwaited,
    Scheduled,
    Activating,
    Active,
    Revoking,
    Withdrawing,
    Denied,
    ActivationFailed,
    Expired,
    Revoked,
    Ended,
    Withdrawn,
    /// A state the SDK either does not recognize or that we have not modeled
    /// yet. The wrapped string is the raw name from the wire.
    Unknown(String),
}

impl GrantState {
    pub fn is_active(&self) -> bool {
        matches!(self, GrantState::Active)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            GrantState::Denied
                | GrantState::ActivationFailed
                | GrantState::Expired
                | GrantState::Revoked
                | GrantState::Ended
                | GrantState::Withdrawn
        )
    }

    /// Map an SDK state variant to our local enum. Any variant we do not
    /// recognize lands in [`GrantState::Unknown`], carrying whatever name the
    /// SDK exposes for it.
    pub fn from_sdk(state: &grant::State) -> Self {
        match state {
            grant::State::ApprovalAwaited => Self::ApprovalAwaited,
            grant::State::Scheduled => Self::Scheduled,
            grant::State::Activating => Self::Activating,
            grant::State::Active => Self::Active,
            grant::State::Revoking => Self::Revoking,
            grant::State::Withdrawing => Self::Withdrawing,
            grant::State::Denied => Self::Denied,
            grant::State::ActivationFailed => Self::ActivationFailed,
            grant::State::Expired => Self::Expired,
            grant::State::Revoked => Self::Revoked,
            grant::State::Ended => Self::Ended,
            grant::State::Withdrawn => Self::Withdrawn,
            other => Self::Unknown(format!("{other:?}")),
        }
    }
}

impl std::fmt::Display for GrantState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrantState::Requested => f.write_str("Requested"),
            GrantState::ApprovalAwaited => f.write_str("ApprovalAwaited"),
            GrantState::Scheduled => f.write_str("Scheduled"),
            GrantState::Activating => f.write_str("Activating"),
            GrantState::Active => f.write_str("Active"),
            GrantState::Revoking => f.write_str("Revoking"),
            GrantState::Withdrawing => f.write_str("Withdrawing"),
            GrantState::Denied => f.write_str("Denied"),
            GrantState::ActivationFailed => f.write_str("ActivationFailed"),
            GrantState::Expired => f.write_str("Expired"),
            GrantState::Revoked => f.write_str("Revoked"),
            GrantState::Ended => f.write_str("Ended"),
            GrantState::Withdrawn => f.write_str("Withdrawn"),
            GrantState::Unknown(name) => write!(f, "Unknown:{name}"),
        }
    }
}

impl FromStr for GrantState {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "Requested" => Self::Requested,
            "ApprovalAwaited" => Self::ApprovalAwaited,
            "Scheduled" => Self::Scheduled,
            "Activating" => Self::Activating,
            "Active" => Self::Active,
            "Revoking" => Self::Revoking,
            "Withdrawing" => Self::Withdrawing,
            "Denied" => Self::Denied,
            "ActivationFailed" => Self::ActivationFailed,
            "Expired" => Self::Expired,
            "Revoked" => Self::Revoked,
            "Ended" => Self::Ended,
            "Withdrawn" => Self::Withdrawn,
            other => {
                let raw = other.strip_prefix("Unknown:").unwrap_or(other);
                Self::Unknown(raw.to_string())
            }
        })
    }
}

impl ToSql for GrantState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.to_string()))
    }
}

impl FromSql for GrantState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse()
            .map_err(|e: std::convert::Infallible| FromSqlError::Other(Box::new(e)))
    }
}

/// Submit a grant request. Returns the created grant's resource name on success.
pub async fn create(
    client: &PrivilegedAccessManager,
    entitlement_name: &str,
    duration_secs: i64,
    justification: Option<&str>,
) -> Result<String> {
    let duration = Duration::new(duration_secs, 0).context("invalid duration")?;

    let mut grant = Grant::new().set_requested_duration(duration);
    if let Some(text) = justification.filter(|t| !t.is_empty()) {
        grant = grant.set_justification(Justification::new().set_unstructured_justification(text));
    }

    let resp = client
        .create_grant()
        .set_parent(entitlement_name)
        .set_grant(grant)
        .send()
        .await
        .context("create_grant")?;

    Ok(resp.name)
}

/// Fetch the current state of a grant.
pub async fn get(client: &PrivilegedAccessManager, grant_name: &str) -> Result<Grant> {
    client
        .get_grant()
        .set_name(grant_name)
        .send()
        .await
        .context("get_grant")
}

/// Approve a grant in `APPROVAL_AWAITED` state. `reason` is sent only if
/// non-empty; some workflows require it, most accept it as optional metadata.
pub async fn approve(
    client: &PrivilegedAccessManager,
    grant_name: &str,
    reason: Option<&str>,
) -> Result<()> {
    let mut req = client.approve_grant().set_name(grant_name);
    if let Some(r) = reason.filter(|s| !s.is_empty()) {
        req = req.set_reason(r);
    }
    req.send().await.context("approve_grant")?;
    Ok(())
}

/// Deny a grant in `APPROVAL_AWAITED` state.
pub async fn deny(
    client: &PrivilegedAccessManager,
    grant_name: &str,
    reason: Option<&str>,
) -> Result<()> {
    let mut req = client.deny_grant().set_name(grant_name);
    if let Some(r) = reason.filter(|s| !s.is_empty()) {
        req = req.set_reason(r);
    }
    req.send().await.context("deny_grant")?;
    Ok(())
}

/// Subset of a PAM `Grant` we need to render an approval decision screen.
/// Decoupled from the SDK type so the modal doesn't grow a dependency on it
/// and the demo backend can construct one without round-tripping through the
/// real Grant model.
#[derive(Debug, Clone)]
pub struct GrantDetails {
    pub name: String,
    pub state: GrantState,
    pub requester: String,
    pub requested_duration_secs: i64,
    pub justification: Option<String>,
    pub role_bindings: Vec<RoleBindingView>,
    pub scope_type: Scope,
    pub scope_id: String,
    pub entitlement_short_name: String,
}

#[derive(Debug, Clone)]
pub struct RoleBindingView {
    pub role: String,
    pub condition: Option<String>,
}

/// Fetch a grant and project it into [`GrantDetails`] suitable for rendering.
pub async fn get_details(
    client: &PrivilegedAccessManager,
    grant_name: &str,
) -> Result<GrantDetails> {
    let g = get(client, grant_name).await?;
    let (scope_type, scope_id, entitlement_short_name) = parse_scope_from_name(&g.name)?;

    let requested_duration_secs = g
        .requested_duration
        .as_ref()
        .map(|d| d.seconds())
        .unwrap_or(0);

    let justification = g
        .justification
        .as_ref()
        .and_then(|j| j.unstructured_justification().cloned());

    let role_bindings = g
        .privileged_access
        .as_ref()
        .and_then(|p| p.gcp_iam_access())
        .map(|a| {
            a.role_bindings
                .iter()
                .map(|rb| RoleBindingView {
                    role: rb.role.clone(),
                    condition: if rb.condition_expression.is_empty() {
                        None
                    } else {
                        Some(rb.condition_expression.clone())
                    },
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(GrantDetails {
        state: GrantState::from_sdk(&g.state),
        name: g.name,
        requester: g.requester,
        requested_duration_secs,
        justification,
        role_bindings,
        scope_type,
        scope_id,
        entitlement_short_name,
    })
}

/// Parse the scope segments and entitlement short name out of a fully-qualified
/// grant resource name: `<plural>/<id>/locations/<loc>/entitlements/<ent>/grants/<id>`.
fn parse_scope_from_name(name: &str) -> Result<(Scope, String, String)> {
    let parts = parse_grant_name(name)
        .ok_or_else(|| anyhow::anyhow!("grant name does not match expected resource shape: '{name}'"))?;
    Ok((parts.scope, parts.scope_id.to_string(), parts.entitlement.to_string()))
}

/// Fields parsed out of a fully-qualified PAM grant resource name:
/// `<scope-root>/<id>/locations/<loc>/entitlements/<ent>/grants/<id>`.
#[derive(Debug, Clone, Copy)]
pub struct GrantNameParts<'a> {
    pub scope: Scope,
    pub scope_id: &'a str,
    pub entitlement: &'a str,
}

/// Parse the grant resource name into its useful pieces, or return `None`
/// when the shape doesn't match what PAM produces.
pub fn parse_grant_name(s: &str) -> Option<GrantNameParts<'_>> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 8
        || parts[2] != "locations"
        || parts[4] != "entitlements"
        || parts[6] != "grants"
        || parts[1].is_empty()
        || parts[3].is_empty()
        || parts[5].is_empty()
        || parts[7].is_empty()
    {
        return None;
    }
    let scope = match parts[0] {
        "organizations" => Scope::Organization,
        "folders" => Scope::Folder,
        "projects" => Scope::Project,
        _ => return None,
    };
    Some(GrantNameParts {
        scope,
        scope_id: parts[1],
        entitlement: parts[5],
    })
}

/// Shape check for a fully-qualified PAM grant resource name.
pub fn is_valid_grant_name(s: &str) -> bool {
    parse_grant_name(s).is_some()
}

/// Parse a duration like `1h`, `30m`, `5400s`, or bare integer seconds.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, suffix) = match s.chars().last()? {
        'h' | 'H' => (&s[..s.len() - 1], 3600i64),
        'm' | 'M' => (&s[..s.len() - 1], 60),
        's' | 'S' => (&s[..s.len() - 1], 1),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    n.checked_mul(suffix)
}

pub const MIN_DURATION_SECS: i64 = 30 * 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duration_formats() {
        assert_eq!(parse_duration("1h"), Some(3600));
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration("5400s"), Some(5400));
        assert_eq!(parse_duration("3600"), Some(3600));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("0h"), None);
    }

    #[test]
    fn known_state_round_trips() {
        for s in [
            GrantState::Requested,
            GrantState::ApprovalAwaited,
            GrantState::Activating,
            GrantState::Active,
            GrantState::ActivationFailed,
            GrantState::Ended,
        ] {
            let serialized = s.to_string();
            let parsed: GrantState = serialized.parse().unwrap();
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn unknown_round_trips() {
        let s = GrantState::Unknown("FooBar".into());
        let serialized = s.to_string();
        assert_eq!(serialized, "Unknown:FooBar");
        let parsed: GrantState = serialized.parse().unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn activating_is_not_active() {
        assert!(!GrantState::Activating.is_active());
        assert!(!GrantState::ActivationFailed.is_active());
        assert!(GrantState::Active.is_active());
    }

    #[test]
    fn parses_scope_from_grant_name() {
        let (s, id, ent) = parse_scope_from_name(
            "organizations/123/locations/global/entitlements/foo/grants/abc",
        )
        .unwrap();
        assert_eq!(s, Scope::Organization);
        assert_eq!(id, "123");
        assert_eq!(ent, "foo");

        let (s, id, ent) =
            parse_scope_from_name("projects/p/locations/global/entitlements/bar/grants/g")
                .unwrap();
        assert_eq!(s, Scope::Project);
        assert_eq!(id, "p");
        assert_eq!(ent, "bar");

        assert!(parse_scope_from_name("garbage").is_err());
        assert!(
            parse_scope_from_name("users/1/locations/global/entitlements/e/grants/g").is_err()
        );
    }

    #[test]
    fn validator_accepts_org_folder_project() {
        assert!(is_valid_grant_name(
            "organizations/0/locations/global/entitlements/e/grants/g"
        ));
        assert!(is_valid_grant_name(
            "folders/1/locations/global/entitlements/e/grants/g"
        ));
        assert!(is_valid_grant_name(
            "projects/p/locations/global/entitlements/e/grants/g"
        ));
    }

    #[test]
    fn validator_rejects_bad_shapes() {
        // wrong scope root
        assert!(!is_valid_grant_name(
            "users/1/locations/global/entitlements/e/grants/g"
        ));
        // missing segments
        assert!(!is_valid_grant_name("organizations/1/locations/global"));
        // empty id slot
        assert!(!is_valid_grant_name(
            "organizations//locations/global/entitlements/e/grants/g"
        ));
        // a URL
        assert!(!is_valid_grant_name(
            "https://console.cloud.google.com/iam-admin/pam/grants"
        ));
    }

    #[test]
    fn parse_grant_name_exposes_parts() {
        let p = parse_grant_name(
            "projects/p123/locations/global/entitlements/my-ent/grants/uuid-abc",
        )
        .unwrap();
        assert_eq!(p.scope, Scope::Project);
        assert_eq!(p.scope_id, "p123");
        assert_eq!(p.entitlement, "my-ent");
    }

    #[test]
    fn terminal_set() {
        assert!(GrantState::Denied.is_terminal());
        assert!(GrantState::ActivationFailed.is_terminal());
        assert!(GrantState::Ended.is_terminal());
        assert!(!GrantState::Active.is_terminal());
        assert!(!GrantState::Activating.is_terminal());
        assert!(!GrantState::ApprovalAwaited.is_terminal());
    }
}
