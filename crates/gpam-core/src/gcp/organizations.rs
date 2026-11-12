use anyhow::Result;
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_resourcemanager_v3::client::Organizations;
use google_cloud_resourcemanager_v3::model::organization::State;

use crate::cache::{OrganizationRow, now_unix};

/// List every organization visible to the caller via
/// `cloudresourcemanager.organizations.search`. Skips non-ACTIVE orgs.
/// `search_organizations` requires a filter; an empty string asks the server
/// to return everything the caller can see.
pub async fn list_visible(client: &Organizations) -> Result<Vec<OrganizationRow>> {
    let mut stream = client.search_organizations().by_item();
    let fetched_at = now_unix();
    let mut out = Vec::new();

    while let Some(o) = stream.next().await {
        let o = o?;
        if !matches!(o.state, State::Active) {
            continue;
        }
        let org_id = match o.name.strip_prefix("organizations/") {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };
        out.push(OrganizationRow {
            org_id,
            display_name: if o.display_name.is_empty() {
                None
            } else {
                Some(o.display_name)
            },
            fetched_at,
        });
    }

    Ok(out)
}
