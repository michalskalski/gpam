use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use futures::stream;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use crate::backend::{DynBackend, ScopeTarget};
use crate::cache::{Cache, EntitlementRow, meta_keys, now_unix};
use crate::gcp::Scope;

/// Cache freshness policy. The hard threshold blocks the UI on a refresh
/// because the data would be too old to trust; the soft threshold lets the UI
/// open instantly but kicks off a background re-scan.
pub const SOFT_TTL_SECS: i64 = 30 * 60;
pub const HARD_TTL_SECS: i64 = 24 * 60 * 60;

/// Max number of in-flight per-scope entitlement searches.
const FANOUT: usize = 20;

/// Which resource scopes a refresh should crawl. Defaults to all three.
#[derive(Debug, Clone, Copy)]
pub struct RefreshOptions {
    pub projects: bool,
    pub folders: bool,
    pub organizations: bool,
}

impl Default for RefreshOptions {
    fn default() -> Self {
        Self {
            projects: true,
            folders: true,
            organizations: true,
        }
    }
}

impl RefreshOptions {
    /// True when entitlements at this scope should be visible. Used both to
    /// gate the discovery call and to filter the UI view of cached rows, so
    /// `--no-folders` is honored even when a previous run populated the
    /// folder rows.
    pub fn includes(&self, scope: Scope) -> bool {
        match scope {
            Scope::Project => self.projects,
            Scope::Folder => self.folders,
            Scope::Organization => self.organizations,
        }
    }
}

#[derive(Debug, Clone)]
pub enum RefreshEvent {
    Started,
    EntitlementUpserted(EntitlementRow),
    Finished { total: usize, error: Option<String> },
}

pub enum Decision {
    /// Cached data is fresh enough; no refresh needed.
    Fresh,
    /// Show cached data, refresh in the background.
    Soft,
    /// Cache is too stale (or missing); block on a refresh before showing the UI.
    Hard,
}

pub fn decide(cache: &Cache) -> Decision {
    match cache
        .meta_timestamp(meta_keys::ENTITLEMENTS_SCANNED_AT)
        .ok()
        .flatten()
    {
        None => Decision::Hard,
        Some(ts) => {
            let age = now_unix() - ts;
            if age >= HARD_TTL_SECS {
                Decision::Hard
            } else if age >= SOFT_TTL_SECS {
                Decision::Soft
            } else {
                Decision::Fresh
            }
        }
    }
}

/// Run a full re-scan: list visible scopes, fan out entitlement search across
/// them, persist into the cache, and emit one event per upserted row.
pub fn spawn(
    cache: Arc<Mutex<Cache>>,
    backend: DynBackend,
    options: RefreshOptions,
    events: mpsc::Sender<RefreshEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = events.send(RefreshEvent::Started).await;
        let outcome = run(&cache, backend, options, &events).await;
        let event = match outcome {
            Ok(total) => RefreshEvent::Finished { total, error: None },
            Err(e) => RefreshEvent::Finished {
                total: 0,
                error: Some(e.to_string()),
            },
        };
        let _ = events.send(event).await;
    })
}

async fn run(
    cache: &Arc<Mutex<Cache>>,
    backend: DynBackend,
    options: RefreshOptions,
    events: &mpsc::Sender<RefreshEvent>,
) -> Result<usize> {
    let mut scopes: Vec<ScopeTarget> = Vec::new();

    if options.projects {
        match discover_projects(&backend, cache).await {
            Ok(items) => scopes.extend(items.into_iter().map(|(id, name)| ScopeTarget {
                scope: Scope::Project,
                id,
                display_name: name,
            })),
            Err(e) => warn!(error = %e, "project discovery failed"),
        }
    }
    if options.folders {
        match discover_folders(&backend, cache).await {
            Ok(items) => scopes.extend(items.into_iter().map(|(id, name)| ScopeTarget {
                scope: Scope::Folder,
                id,
                display_name: name,
            })),
            Err(e) => warn!(error = %e, "folder discovery failed"),
        }
    }
    if options.organizations {
        match discover_organizations(&backend, cache).await {
            Ok(items) => scopes.extend(items.into_iter().map(|(id, name)| ScopeTarget {
                scope: Scope::Organization,
                id,
                display_name: name,
            })),
            Err(e) => warn!(error = %e, "organization discovery failed"),
        }
    }

    let count = stream::iter(scopes)
        .map(|target| {
            let backend = backend.clone();
            async move {
                match backend.search_entitlements(&target).await {
                    Ok(rows) => Some(rows),
                    Err(e) => {
                        // PERMISSION_DENIED on scopes that haven't enabled the PAM API
                        let msg = e.to_string();
                        if msg.contains("PERMISSION_DENIED") {
                            debug!(scope = %target.scope, id = %target.id, error = %msg, "entitlement search failed");
                        } else {
                            warn!(scope = %target.scope, id = %target.id, error = %msg, "entitlement search failed");
                        }
                        None
                    }
                }
            }
        })
        .buffer_unordered(FANOUT)
        .fold(0usize, |acc, batch| async move {
            let Some(rows) = batch else { return acc };
            let mut n = acc;
            for row in rows {
                {
                    let cache = cache.lock().await;
                    if let Err(e) = cache.upsert_entitlement(&row) {
                        warn!(entitlement = %row.name, error = %e, "cache upsert_entitlement failed");
                    }
                }
                n += 1;
                let _ = events.send(RefreshEvent::EntitlementUpserted(row)).await;
            }
            n
        })
        .await;

    {
        let cache = cache.lock().await;
        let _ = cache.set_meta_timestamp(meta_keys::ENTITLEMENTS_SCANNED_AT, now_unix());
    }

    Ok(count)
}

async fn discover_projects(
    backend: &DynBackend,
    cache: &Arc<Mutex<Cache>>,
) -> Result<Vec<(String, Option<String>)>> {
    let projects = backend.list_projects().await?;
    let items = {
        let cache = cache.lock().await;
        for p in &projects {
            if let Err(e) = cache.upsert_project(p) {
                warn!(project = %p.project_id, error = %e, "cache upsert_project failed");
            }
        }
        let _ = cache.set_meta_timestamp(meta_keys::PROJECTS_SCANNED_AT, now_unix());
        projects
            .into_iter()
            .map(|p| (p.project_id, p.display_name))
            .collect()
    };
    Ok(items)
}

async fn discover_folders(
    backend: &DynBackend,
    cache: &Arc<Mutex<Cache>>,
) -> Result<Vec<(String, Option<String>)>> {
    let folders = backend.list_folders().await?;
    let items = {
        let cache = cache.lock().await;
        for f in &folders {
            if let Err(e) = cache.upsert_folder(f) {
                warn!(folder = %f.folder_id, error = %e, "cache upsert_folder failed");
            }
        }
        let _ = cache.set_meta_timestamp(meta_keys::FOLDERS_SCANNED_AT, now_unix());
        folders
            .into_iter()
            .map(|f| (f.folder_id, f.display_name))
            .collect()
    };
    Ok(items)
}

async fn discover_organizations(
    backend: &DynBackend,
    cache: &Arc<Mutex<Cache>>,
) -> Result<Vec<(String, Option<String>)>> {
    let orgs = backend.list_organizations().await?;
    let items = {
        let cache = cache.lock().await;
        for o in &orgs {
            if let Err(e) = cache.upsert_organization(o) {
                warn!(org = %o.org_id, error = %e, "cache upsert_organization failed");
            }
        }
        let _ = cache.set_meta_timestamp(meta_keys::ORGANIZATIONS_SCANNED_AT, now_unix());
        orgs.into_iter()
            .map(|o| (o.org_id, o.display_name))
            .collect()
    };
    Ok(items)
}
