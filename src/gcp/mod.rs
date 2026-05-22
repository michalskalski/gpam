pub mod entitlements;
pub mod folders;
pub mod grants;
pub mod organizations;
pub mod projects;

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use google_cloud_auth::credentials::Credentials;
use google_cloud_privilegedaccessmanager_v1::client::PrivilegedAccessManager;
use google_cloud_resourcemanager_v3::client::{Folders, Organizations, Projects};
use serde::{Deserialize, Serialize};

/// Which level of the GCP resource hierarchy a PAM entitlement is attached to.
/// PAM accepts the same parent shape for all three: `{plural}/{id}/locations/global`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Project,
    Folder,
    Organization,
}

impl Scope {
    pub fn plural(self) -> &'static str {
        match self {
            Scope::Project => "projects",
            Scope::Folder => "folders",
            Scope::Organization => "organizations",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Scope::Project => "proj",
            Scope::Folder => "fldr",
            Scope::Organization => "org",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Folder => "folder",
            Scope::Organization => "organization",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "project" | "projects" => Ok(Scope::Project),
            "folder" | "folders" => Ok(Scope::Folder),
            "organization" | "organizations" | "org" => Ok(Scope::Organization),
            other => bail!("unknown scope '{other}'"),
        }
    }
}

pub async fn pam_client(creds: Credentials) -> Result<PrivilegedAccessManager> {
    PrivilegedAccessManager::builder()
        .with_credentials(creds)
        .build()
        .await
        .context("building PAM client")
}

pub async fn projects_client(creds: Credentials) -> Result<Projects> {
    Projects::builder()
        .with_credentials(creds)
        .build()
        .await
        .context("building Projects client")
}

pub async fn folders_client(creds: Credentials) -> Result<Folders> {
    Folders::builder()
        .with_credentials(creds)
        .build()
        .await
        .context("building Folders client")
}

pub async fn organizations_client(creds: Credentials) -> Result<Organizations> {
    Organizations::builder()
        .with_credentials(creds)
        .build()
        .await
        .context("building Organizations client")
}
