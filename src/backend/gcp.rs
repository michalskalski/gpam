use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use google_cloud_auth::credentials::Credentials;
use google_cloud_privilegedaccessmanager_v1::client::PrivilegedAccessManager;
use google_cloud_resourcemanager_v3::client::{Folders, Organizations, Projects};

use crate::backend::{Backend, ScopeTarget};
use crate::cache::{EntitlementRow, FolderRow, OrganizationRow, ProjectRow};
use crate::gcp;

pub struct GcpBackend {
    pam: Arc<PrivilegedAccessManager>,
    projects: Projects,
    folders: Folders,
    organizations: Organizations,
}

impl GcpBackend {
    pub async fn new(credentials: Credentials) -> Result<Self> {
        let pam = gcp::pam_client(credentials.clone()).await?;
        let projects = gcp::projects_client(credentials.clone()).await?;
        let folders = gcp::folders_client(credentials.clone()).await?;
        let organizations = gcp::organizations_client(credentials).await?;
        Ok(Self {
            pam: Arc::new(pam),
            projects,
            folders,
            organizations,
        })
    }
}

#[async_trait]
impl Backend for GcpBackend {
    async fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        gcp::projects::list_visible(&self.projects).await
    }

    async fn list_folders(&self) -> Result<Vec<FolderRow>> {
        gcp::folders::list_visible(&self.folders).await
    }

    async fn list_organizations(&self) -> Result<Vec<OrganizationRow>> {
        gcp::organizations::list_visible(&self.organizations).await
    }

    async fn search_entitlements(&self, target: &ScopeTarget) -> Result<Vec<EntitlementRow>> {
        gcp::entitlements::search_one(&self.pam, target).await
    }

    async fn create_grant(
        &self,
        entitlement_name: &str,
        duration_secs: i64,
        justification: Option<&str>,
    ) -> Result<String> {
        gcp::grants::create(&self.pam, entitlement_name, duration_secs, justification).await
    }

    async fn get_grant_state(&self, grant_name: &str) -> Result<String> {
        let g = gcp::grants::get(&self.pam, grant_name).await?;
        Ok(format!("{:?}", g.state))
    }
}
