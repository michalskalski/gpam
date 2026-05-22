use anyhow::Result;
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_resourcemanager_v3::client::Projects;

use crate::cache::{ProjectRow, now_unix};

/// List every project visible to the caller via `cloudresourcemanager.projects.search`.
/// Skips projects that aren't ACTIVE.
pub async fn list_visible(client: &Projects) -> Result<Vec<ProjectRow>> {
    let mut stream = client.search_projects().by_item();
    let fetched_at = now_unix();
    let mut out = Vec::new();

    while let Some(p) = stream.next().await {
        let p = p?;
        if p.project_id.is_empty() {
            continue;
        }
        let state = format!("{:?}", p.state);
        if !state.contains("Active") {
            continue;
        }
        out.push(ProjectRow {
            project_id: p.project_id,
            display_name: if p.display_name.is_empty() {
                None
            } else {
                Some(p.display_name)
            },
            state: Some(state),
            fetched_at,
        });
    }

    Ok(out)
}
