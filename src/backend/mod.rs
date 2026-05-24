pub mod demo;
pub mod gcp;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::cache::{EntitlementRow, FolderRow, OrganizationRow, ProjectRow};
use crate::gcp::Scope;
use crate::gcp::grants::{GrantDetails, GrantState};

pub type DynBackend = Arc<dyn Backend>;

/// A scope to search, paired with the human-readable name discovered alongside
/// it. The display name flows through to streamed entitlement rows so the UI
/// can show "fldr:My Team" instead of the bare numeric ID before the cache
/// JOIN takes over on the next read.
#[derive(Debug, Clone)]
pub struct ScopeTarget {
    pub scope: Scope,
    pub id: String,
    pub display_name: Option<String>,
}

/// Abstracts the data source behind the TUI. The production impl talks to GCP.
/// The demo impl returns seeded fixtures and runs an in-memory grant state
/// machine so the binary can be tried without credentials.
#[async_trait]
pub trait Backend: Send + Sync {
    async fn list_projects(&self) -> Result<Vec<ProjectRow>>;
    async fn list_folders(&self) -> Result<Vec<FolderRow>>;
    async fn list_organizations(&self) -> Result<Vec<OrganizationRow>>;

    /// Entitlements that the caller can request under one scope. Each row is
    /// expected to carry `target.display_name` in `scope_display_name`.
    async fn search_entitlements(&self, target: &ScopeTarget) -> Result<Vec<EntitlementRow>>;

    /// Submit a grant. Returns the new grant's resource name.
    async fn create_grant(
        &self,
        entitlement_name: &str,
        duration_secs: i64,
        justification: Option<&str>,
    ) -> Result<String>;

    /// Single grant state read
    async fn get_grant_state(&self, grant_name: &str) -> Result<GrantState>;

    /// Fetch enough of a grant to render an approval decision (requester,
    /// roles, justification, duration, scope).
    async fn get_grant_details(&self, grant_name: &str) -> Result<GrantDetails>;

    /// Approve an `APPROVAL_AWAITED` grant. Irreversible.
    async fn approve_grant(&self, grant_name: &str, reason: Option<&str>) -> Result<()>;

    /// Deny an `APPROVAL_AWAITED` grant. Irreversible.
    async fn deny_grant(&self, grant_name: &str, reason: Option<&str>) -> Result<()>;
}
