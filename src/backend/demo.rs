use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use crate::backend::{Backend, ScopeTarget};
use crate::cache::{EntitlementRow, FolderRow, OrganizationRow, ProjectRow, now_unix};
use crate::gcp::Scope;
use crate::gcp::entitlements::parent;
use crate::gcp::grants::GrantState;

/// Seeded data + in-memory grant state machine. Lets the binary be tried
/// without GCP credentials -- `--demo` swaps this in for [`GcpBackend`].
pub struct DemoBackend {
    projects: Vec<ProjectRow>,
    folders: Vec<FolderRow>,
    organizations: Vec<OrganizationRow>,
    /// Entitlement seeds, keyed by `(scope, scope_id)`. Each row is cloned and
    /// stamped with the caller's `display_name` + a fresh `fetched_at` on read.
    entitlements: HashMap<(Scope, String), Vec<EntitlementRow>>,
    grants: Mutex<DemoGrants>,
}

#[derive(Default)]
struct DemoGrants {
    next_id: u64,
    by_name: HashMap<String, DemoGrant>,
}

struct DemoGrant {
    state: GrantState,
    poll_count: u32,
    /// `true` when the source entitlement has no approvers. The demo state
    /// machine activates these on the first poll and manual ones take longer.
    auto: bool,
}

impl DemoBackend {
    pub fn new() -> Self {
        let now = now_unix();

        let projects = vec![
            ProjectRow {
                project_id: "acme-prod-platform".into(),
                display_name: Some("Acme Production Platform".into()),
                state: Some("Active".into()),
                fetched_at: now,
            },
            ProjectRow {
                project_id: "acme-staging".into(),
                display_name: Some("Acme Staging".into()),
                state: Some("Active".into()),
                fetched_at: now,
            },
            ProjectRow {
                project_id: "acme-data-warehouse".into(),
                display_name: Some("Data Warehouse".into()),
                state: Some("Active".into()),
                fetched_at: now,
            },
        ];

        let folders = vec![
            FolderRow {
                folder_id: "200000001".into(),
                display_name: Some("Engineering".into()),
                parent: Some("organizations/300000001".into()),
                fetched_at: now,
            },
            FolderRow {
                folder_id: "200000002".into(),
                display_name: Some("Platform Team".into()),
                parent: Some("organizations/300000001".into()),
                fetched_at: now,
            },
        ];

        let organizations = vec![OrganizationRow {
            org_id: "300000001".into(),
            display_name: Some("acme.example.com".into()),
            fetched_at: now,
        }];

        let mut entitlements: HashMap<(Scope, String), Vec<EntitlementRow>> = HashMap::new();

        let mut seed = |scope: Scope,
                        scope_id: &str,
                        short: &str,
                        justification_required: bool,
                        approvers: &[&str],
                        roles: &[&str],
                        max_secs: i64| {
            let name = format!("{}/entitlements/{short}", parent(scope, scope_id));
            let row = EntitlementRow {
                name,
                scope_type: scope,
                scope_id: scope_id.into(),
                scope_display_name: None,
                short_name: short.into(),
                max_request_duration_secs: Some(max_secs),
                justification_required,
                approvers: approvers.iter().map(|s| (*s).into()).collect(),
                roles: roles.iter().map(|s| (*s).into()).collect(),
                raw_json: "{}".into(),
                fetched_at: now,
                last_used_at: None,
            };
            entitlements
                .entry((scope, scope_id.into()))
                .or_default()
                .push(row);
        };

        // Project-scope.
        seed(
            Scope::Project,
            "acme-prod-platform",
            "sre-break-glass",
            true,
            &["user:on-call@acme.example.com"],
            &["roles/owner"],
            4 * 3600,
        );
        seed(
            Scope::Project,
            "acme-prod-platform",
            "logs-read",
            false,
            &[],
            &["roles/logging.viewer"],
            8 * 3600,
        );
        seed(
            Scope::Project,
            "acme-staging",
            "admin-staging",
            false,
            &[],
            &["roles/editor"],
            8 * 3600,
        );
        seed(
            Scope::Project,
            "acme-data-warehouse",
            "bigquery-read",
            true,
            &["group:data-stewards@acme.example.com"],
            &["roles/bigquery.dataViewer"],
            8 * 3600,
        );

        // Folder-scope.
        seed(
            Scope::Folder,
            "200000001",
            "eng-admin",
            true,
            &["group:eng-leads@acme.example.com"],
            &["roles/resourcemanager.folderAdmin"],
            4 * 3600,
        );
        seed(
            Scope::Folder,
            "200000002",
            "platform-deploy",
            false,
            &[],
            &["roles/cloudbuild.builds.editor"],
            8 * 3600,
        );

        // Org-scope.
        seed(
            Scope::Organization,
            "300000001",
            "org-billing-view",
            true,
            &["group:finance@acme.example.com"],
            &["roles/billing.viewer"],
            8 * 3600,
        );
        seed(
            Scope::Organization,
            "300000001",
            "org-audit-read",
            true,
            &["group:security@acme.example.com"],
            &["roles/iam.securityReviewer"],
            8 * 3600,
        );

        Self {
            projects,
            folders,
            organizations,
            entitlements,
            grants: Mutex::new(DemoGrants::default()),
        }
    }

    fn entitlement_auto(&self, entitlement_name: &str) -> bool {
        self.entitlements
            .values()
            .flatten()
            .find(|e| e.name == entitlement_name)
            .map(|e| e.approvers.is_empty())
            .unwrap_or(true)
    }
}

impl Default for DemoBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for DemoBackend {
    async fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        let now = now_unix();
        Ok(self
            .projects
            .iter()
            .map(|p| ProjectRow {
                fetched_at: now,
                ..p.clone()
            })
            .collect())
    }

    async fn list_folders(&self) -> Result<Vec<FolderRow>> {
        let now = now_unix();
        Ok(self
            .folders
            .iter()
            .map(|f| FolderRow {
                fetched_at: now,
                ..f.clone()
            })
            .collect())
    }

    async fn list_organizations(&self) -> Result<Vec<OrganizationRow>> {
        let now = now_unix();
        Ok(self
            .organizations
            .iter()
            .map(|o| OrganizationRow {
                fetched_at: now,
                ..o.clone()
            })
            .collect())
    }

    async fn search_entitlements(&self, target: &ScopeTarget) -> Result<Vec<EntitlementRow>> {
        let now = now_unix();
        Ok(self
            .entitlements
            .get(&(target.scope, target.id.clone()))
            .into_iter()
            .flatten()
            .map(|e| EntitlementRow {
                scope_display_name: target.display_name.clone(),
                fetched_at: now,
                ..e.clone()
            })
            .collect())
    }

    async fn create_grant(
        &self,
        entitlement_name: &str,
        _duration_secs: i64,
        _justification: Option<&str>,
    ) -> Result<String> {
        let auto = self.entitlement_auto(entitlement_name);
        let mut g = self.grants.lock().unwrap();
        g.next_id += 1;
        let id = format!("demo-{:04}", g.next_id);
        let name = format!("{entitlement_name}/grants/{id}");
        g.by_name.insert(
            name.clone(),
            DemoGrant {
                state: GrantState::Requested,
                poll_count: 0,
                auto,
            },
        );
        Ok(name)
    }

    async fn get_grant_state(&self, grant_name: &str) -> Result<GrantState> {
        let mut g = self.grants.lock().unwrap();
        // Unknown grants (e.g. persisted from a previous demo session) resolve
        // as Ended so the poller stops cleanly without growing a backlog.
        let Some(grant) = g.by_name.get_mut(grant_name) else {
            return Ok(GrantState::Ended);
        };
        grant.poll_count += 1;
        grant.state = if grant.auto || grant.poll_count >= 2 {
            GrantState::Active
        } else {
            GrantState::ApprovalAwaited
        };
        Ok(grant.state.clone())
    }
}
