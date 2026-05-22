use anyhow::Result;
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_resourcemanager_v3::client::Folders;
use google_cloud_resourcemanager_v3::model::folder::State;

use crate::cache::{FolderRow, now_unix};

/// List every folder visible to the caller via `cloudresourcemanager.folders.search`.
/// Skips folders that aren't ACTIVE.
pub async fn list_visible(client: &Folders) -> Result<Vec<FolderRow>> {
    let mut stream = client.search_folders().by_item();
    let fetched_at = now_unix();
    let mut out = Vec::new();

    while let Some(f) = stream.next().await {
        let f = f?;
        if !matches!(f.state, State::Active) {
            continue;
        }
        let folder_id = match f.name.strip_prefix("folders/") {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };
        out.push(FolderRow {
            folder_id,
            display_name: if f.display_name.is_empty() {
                None
            } else {
                Some(f.display_name)
            },
            parent: if f.parent.is_empty() {
                None
            } else {
                Some(f.parent)
            },
            fetched_at,
        });
    }

    Ok(out)
}
