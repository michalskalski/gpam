//! The resource hierarchy (organizations, folders, projects) that holds
//! entitlements, for a navigator that narrows the entitlement list to one
//! scope. Scopes without entitlements at or below them are left out.

use std::collections::{BTreeSet, HashMap};

use crate::cache::{EntitlementRow, ScopeRow};
use crate::gcp::Scope;

/// Identifies one organization, folder or project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeKey {
    pub scope: Scope,
    pub id: String,
}

impl ScopeKey {
    pub fn new(scope: Scope, id: impl Into<String>) -> Self {
        Self {
            scope,
            id: id.into(),
        }
    }

    /// Parse a resource name such as `folders/123` or `organizations/9`.
    pub fn parse(resource: &str) -> Option<Self> {
        let (kind, id) = resource.split_once('/')?;
        let scope = match kind {
            "projects" => Scope::Project,
            "folders" => Scope::Folder,
            "organizations" => Scope::Organization,
            _ => return None,
        };
        (!id.is_empty() && !id.contains('/')).then(|| Self::new(scope, id))
    }
}

#[derive(Debug, Clone)]
struct Node {
    key: ScopeKey,
    label: String,
    parent: Option<usize>,
    children: Vec<usize>,
    /// Entitlements defined directly on this scope.
    own: usize,
    /// Entitlements defined on scopes below this one.
    below: usize,
}

/// One visible line of the navigator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub key: ScopeKey,
    /// Display name, or the scope id when no name is known.
    pub label: String,
    /// Nesting level, starting at zero for top-level scopes.
    pub depth: usize,
    /// Entitlements defined directly on this scope.
    pub own: usize,
    /// Entitlements defined on scopes below this one.
    pub below: usize,
    pub has_children: bool,
    pub expanded: bool,
}

/// Navigator intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAction {
    Up,
    Down,
    Top,
    Bottom,
    /// Open the scope under the cursor.
    Expand,
    /// Close the scope under the cursor, or move to its parent when it is
    /// already closed.
    Collapse,
    /// Narrow the entitlement list to the scope under the cursor only.
    Select,
    /// Add the scope under the cursor to the active scopes, or take it out.
    Toggle,
}

/// Scope hierarchy plus which branches are closed. Branches start open.
#[derive(Debug, Default)]
pub struct ScopeTree {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    collapsed: BTreeSet<ScopeKey>,
}

impl ScopeTree {
    /// Build the hierarchy from the cached scopes and the entitlements to
    /// count. A scope whose parent is not in `scopes` becomes a top-level
    /// row, since the caller cannot see where it hangs.
    pub fn build(scopes: &[ScopeRow], entitlements: &[EntitlementRow]) -> Self {
        let mut tree = Self::default();
        tree.rebuild(scopes, entitlements);
        tree
    }

    /// Recompute the hierarchy and counts, keeping closed branches closed.
    pub fn rebuild(&mut self, scopes: &[ScopeRow], entitlements: &[EntitlementRow]) {
        let mut nodes: Vec<Node> = Vec::new();
        let mut index: HashMap<ScopeKey, usize> = HashMap::new();
        let mut add = |nodes: &mut Vec<Node>, key: ScopeKey, label: Option<&str>| -> usize {
            if let Some(&i) = index.get(&key) {
                if let Some(label) = label.filter(|l| !l.is_empty()) {
                    nodes[i].label = label.to_string();
                }
                return i;
            }
            let label = label
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| key.id.clone());
            nodes.push(Node {
                key: key.clone(),
                label,
                parent: None,
                children: Vec::new(),
                own: 0,
                below: 0,
            });
            index.insert(key, nodes.len() - 1);
            nodes.len() - 1
        };

        for row in scopes {
            add(
                &mut nodes,
                ScopeKey::new(row.scope, &row.id),
                row.display_name.as_deref(),
            );
        }
        for ent in entitlements {
            let i = add(
                &mut nodes,
                ScopeKey::new(ent.scope_type, &ent.scope_id),
                ent.scope_display_name.as_deref(),
            );
            nodes[i].own += 1;
        }

        // Link each scope to its parent when that parent is known.
        for row in scopes {
            let Some(parent) = row.parent.as_deref().and_then(ScopeKey::parse) else {
                continue;
            };
            let (Some(&child), Some(&parent)) = (
                index.get(&ScopeKey::new(row.scope, &row.id)),
                index.get(&parent),
            ) else {
                continue;
            };
            if child != parent && !is_ancestor(&nodes, child, parent) {
                nodes[child].parent = Some(parent);
            }
        }

        // Add each scope's own count to every ancestor.
        for i in 0..nodes.len() {
            let own = nodes[i].own;
            let mut at = nodes[i].parent;
            while let Some(p) = at {
                nodes[p].below += own;
                at = nodes[p].parent;
            }
        }

        // Keep scopes that hold entitlements at or below them.
        let keep: Vec<bool> = nodes.iter().map(|n| n.own + n.below > 0).collect();
        let mut remap = vec![usize::MAX; nodes.len()];
        let mut kept: Vec<Node> = Vec::new();
        for (i, node) in nodes.into_iter().enumerate() {
            if keep[i] {
                remap[i] = kept.len();
                kept.push(node);
            }
        }
        for node in &mut kept {
            node.parent = node.parent.map(|p| remap[p]);
        }
        let mut roots = Vec::new();
        for i in 0..kept.len() {
            match kept[i].parent {
                Some(p) => kept[p].children.push(i),
                None => roots.push(i),
            }
        }
        let order = |nodes: &[Node], list: &mut Vec<usize>| {
            list.sort_by_cached_key(|&i| {
                let n = &nodes[i];
                (
                    scope_rank(n.key.scope),
                    n.label.to_lowercase(),
                    n.key.id.clone(),
                )
            });
        };
        order(&kept, &mut roots);
        for i in 0..kept.len() {
            let mut children = std::mem::take(&mut kept[i].children);
            order(&kept, &mut children);
            kept[i].children = children;
        }

        self.collapsed
            .retain(|key| kept.iter().any(|n| &n.key == key));
        self.nodes = kept;
        self.roots = roots;
    }

    /// Visible lines in display order, skipping the contents of closed
    /// branches.
    pub fn rows(&self) -> Vec<TreeRow> {
        let mut out = Vec::new();
        let mut stack: Vec<(usize, usize)> = self.roots.iter().rev().map(|&i| (i, 0)).collect();
        while let Some((i, depth)) = stack.pop() {
            let node = &self.nodes[i];
            let expanded = !self.collapsed.contains(&node.key);
            out.push(TreeRow {
                key: node.key.clone(),
                label: node.label.clone(),
                depth,
                own: node.own,
                below: node.below,
                has_children: !node.children.is_empty(),
                expanded,
            });
            if expanded {
                stack.extend(node.children.iter().rev().map(|&c| (c, depth + 1)));
            }
        }
        out
    }

    pub fn contains(&self, key: &ScopeKey) -> bool {
        self.nodes.iter().any(|n| &n.key == key)
    }

    /// Display name of a scope in the tree.
    pub fn label(&self, key: &ScopeKey) -> Option<&str> {
        self.nodes
            .iter()
            .find(|n| &n.key == key)
            .map(|n| n.label.as_str())
    }

    /// The parent of a scope, when it has one in the tree.
    pub fn parent(&self, key: &ScopeKey) -> Option<&ScopeKey> {
        let node = self.nodes.iter().find(|n| &n.key == key)?;
        node.parent.map(|p| &self.nodes[p].key)
    }

    /// The scopes a typed name could refer to, best first: an exact id or
    /// display name, then fuzzy matches over names and ids.
    pub fn candidates(&self, text: &str) -> Vec<ScopeKey> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let exact = self.nodes.iter().position(|n| {
            n.key.id.eq_ignore_ascii_case(text) || n.label.eq_ignore_ascii_case(text)
        });
        let names: Vec<String> = self
            .nodes
            .iter()
            .map(|n| format!("{} {}", n.label, n.key.id))
            .collect();
        let ranked = crate::fuzzy::rank(&names, text);
        exact
            .into_iter()
            .chain(ranked.into_iter().filter(|i| Some(*i) != exact))
            .map(|i| self.nodes[i].key.clone())
            .collect()
    }

    /// The best match of [`ScopeTree::candidates`].
    pub fn resolve(&self, text: &str) -> Option<ScopeKey> {
        self.candidates(text).into_iter().next()
    }

    pub fn set_expanded(&mut self, key: &ScopeKey, expanded: bool) {
        if expanded {
            self.collapsed.remove(key);
        } else if self.contains(key) {
            self.collapsed.insert(key.clone());
        }
    }

    pub fn is_expanded(&self, key: &ScopeKey) -> bool {
        !self.collapsed.contains(key)
    }
}

/// A search text split into the plain query and one `prefix:value` token,
/// such as an `in:` scope or a `role:` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeQuery {
    /// The text to search entitlements with, without the token.
    pub rest: String,
    /// What follows the prefix in the first such word, if there is one.
    pub token: Option<String>,
    /// More text or whitespace follows the token, so it is finished.
    pub complete: bool,
}

/// Find an `in:name` word in a search text. Without one the text comes back
/// unchanged as `rest`.
pub fn split_scope_token(text: &str) -> ScopeQuery {
    split_token(text, "in:")
}

/// Find a `role:name` word in a search text. Without one the text comes
/// back unchanged as `rest`.
pub fn split_role_token(text: &str) -> ScopeQuery {
    split_token(text, "role:")
}

fn split_token(text: &str, prefix: &str) -> ScopeQuery {
    let n = prefix.len();
    let mut start = 0;
    for word in text.split_whitespace() {
        let offset = text[start..].find(word).map_or(start, |i| start + i);
        start = offset + word.len();
        let is_token = word
            .get(..n)
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix));
        if !is_token {
            continue;
        }
        let before = text[..offset].trim_end();
        let after = text[start..].trim_start();
        let rest = match (before.is_empty(), after.is_empty()) {
            (true, _) => after.to_string(),
            (false, true) => before.to_string(),
            (false, false) => format!("{before} {after}"),
        };
        return ScopeQuery {
            rest,
            token: Some(word[n..].to_string()),
            complete: start < text.len(),
        };
    }
    ScopeQuery {
        rest: text.to_string(),
        token: None,
        complete: false,
    }
}

/// True when `candidate` is `node` or one of its ancestors. Used to refuse
/// a parent link that would close a loop.
fn is_ancestor(nodes: &[Node], candidate: usize, node: usize) -> bool {
    let mut at = Some(node);
    let mut steps = 0;
    while let Some(i) = at {
        if i == candidate {
            return true;
        }
        steps += 1;
        if steps > nodes.len() {
            return true;
        }
        at = nodes[i].parent;
    }
    false
}

/// Sort order among siblings: organizations, then folders, then projects.
fn scope_rank(scope: Scope) -> u8 {
    match scope {
        Scope::Organization => 0,
        Scope::Folder => 1,
        Scope::Project => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(scope: Scope, id: &str, name: &str, parent: Option<&str>) -> ScopeRow {
        ScopeRow {
            scope,
            id: id.into(),
            display_name: Some(name.into()),
            parent: parent.map(Into::into),
        }
    }

    fn ent(scope: Scope, scope_id: &str, short: &str) -> EntitlementRow {
        EntitlementRow {
            name: format!(
                "{}/{scope_id}/locations/global/entitlements/{short}",
                scope.plural()
            ),
            scope_type: scope,
            scope_id: scope_id.into(),
            scope_display_name: None,
            short_name: short.into(),
            max_request_duration_secs: None,
            justification_required: false,
            approvers: Vec::new(),
            roles: Vec::new(),
            raw_json: "{}".into(),
            fetched_at: 0,
            last_used_at: None,
        }
    }

    fn fixture() -> ScopeTree {
        let scopes = [
            scope(Scope::Organization, "900", "example.test", None),
            scope(Scope::Folder, "810", "Platform", Some("organizations/900")),
            scope(Scope::Folder, "820", "Empty", Some("organizations/900")),
            scope(Scope::Folder, "811", "Data", Some("folders/810")),
            scope(
                Scope::Project,
                "fake-warehouse",
                "Warehouse",
                Some("folders/811"),
            ),
            scope(Scope::Project, "fake-api", "Api", Some("folders/810")),
            scope(Scope::Project, "fake-orphan", "Orphan", Some("folders/777")),
            scope(Scope::Project, "fake-unused", "Unused", Some("folders/810")),
        ];
        let entitlements = [
            ent(Scope::Organization, "900", "org-audit"),
            ent(Scope::Folder, "810", "platform-admin"),
            ent(Scope::Project, "fake-warehouse", "bq-read"),
            ent(Scope::Project, "fake-warehouse", "bq-write"),
            ent(Scope::Project, "fake-api", "deploy"),
            ent(Scope::Project, "fake-orphan", "debug"),
            ent(Scope::Project, "fake-unlisted", "viewer"),
        ];
        ScopeTree::build(&scopes, &entitlements)
    }

    fn lines(tree: &ScopeTree) -> Vec<String> {
        tree.rows()
            .iter()
            .map(|r| format!("{}{} {}+{}", "  ".repeat(r.depth), r.label, r.own, r.below))
            .collect()
    }

    #[test]
    fn parses_resource_names() {
        assert_eq!(
            ScopeKey::parse("folders/123"),
            Some(ScopeKey::new(Scope::Folder, "123"))
        );
        assert_eq!(ScopeKey::parse("folders/"), None);
        assert_eq!(ScopeKey::parse("widgets/1"), None);
        assert_eq!(ScopeKey::parse("folders/1/extra"), None);
    }

    #[test]
    fn builds_hierarchy_with_counts_and_drops_empty_scopes() {
        let tree = fixture();
        assert_eq!(
            lines(&tree),
            [
                "example.test 1+4",
                "  Platform 1+3",
                "    Data 0+2",
                "      Warehouse 2+0",
                "    Api 1+0",
                "fake-unlisted 1+0",
                "Orphan 1+0",
            ]
        );
        assert!(!tree.contains(&ScopeKey::new(Scope::Folder, "820")));
        assert!(!tree.contains(&ScopeKey::new(Scope::Project, "fake-unused")));
        assert_eq!(
            tree.parent(&ScopeKey::new(Scope::Project, "fake-api")),
            Some(&ScopeKey::new(Scope::Folder, "810"))
        );
    }

    #[test]
    fn closed_branches_hide_their_contents_and_survive_a_rebuild() {
        let mut tree = fixture();
        let platform = ScopeKey::new(Scope::Folder, "810");
        tree.set_expanded(&platform, false);
        assert_eq!(
            lines(&tree),
            [
                "example.test 1+4",
                "  Platform 1+3",
                "fake-unlisted 1+0",
                "Orphan 1+0"
            ]
        );
        let row = &tree.rows()[1];
        assert!(row.has_children && !row.expanded);

        let scopes = [
            scope(Scope::Organization, "900", "example.test", None),
            scope(Scope::Folder, "810", "Platform", Some("organizations/900")),
        ];
        tree.rebuild(&scopes, &[ent(Scope::Folder, "810", "platform-admin")]);
        assert!(!tree.is_expanded(&platform));
        assert_eq!(lines(&tree), ["example.test 0+1", "  Platform 1+0"]);
    }

    #[test]
    fn resolves_typed_scope_names() {
        let tree = fixture();
        let key = |scope, id: &str| Some(ScopeKey::new(scope, id));
        assert_eq!(tree.resolve("fake-api"), key(Scope::Project, "fake-api"));
        assert_eq!(tree.resolve("PLATFORM"), key(Scope::Folder, "810"));
        assert_eq!(tree.resolve("wareh"), key(Scope::Project, "fake-warehouse"));
        assert_eq!(tree.resolve("zzzz"), None);

        // Every scope with "fake" in its id is a candidate, and an exact
        // name comes before the fuzzy matches.
        assert_eq!(tree.candidates("fake").len(), 4);
        assert_eq!(
            tree.candidates("data")[0],
            ScopeKey::new(Scope::Folder, "811")
        );
        assert!(tree.candidates("").is_empty());
        assert_eq!(tree.resolve("  "), None);
    }

    #[test]
    fn splits_role_tokens_out_of_a_search() {
        let typing = split_role_token("in:ware role:big");
        assert_eq!(typing.rest, "in:ware");
        assert_eq!(typing.token.as_deref(), Some("big"));
        assert!(!typing.complete);
        let done = split_role_token("ROLE:storage.admin secret");
        assert_eq!(done.rest, "secret");
        assert_eq!(done.token.as_deref(), Some("storage.admin"));
        assert!(done.complete);
        // A multi-byte word shorter than the prefix is not a token.
        assert_eq!(split_role_token("\u{e9}\u{e9}").token, None);
    }

    #[test]
    fn splits_scope_tokens_out_of_a_search() {
        let split = |text| split_scope_token(text);
        assert_eq!(
            split("viewer"),
            ScopeQuery {
                rest: "viewer".into(),
                token: None,
                complete: false
            }
        );
        // Text without a token keeps its exact spacing.
        assert_eq!(split("a  b ").rest, "a  b ");

        let typing = split("read in:ware");
        assert_eq!(typing.rest, "read");
        assert_eq!(typing.token.as_deref(), Some("ware"));
        assert!(!typing.complete);

        let done = split("in:ware read");
        assert_eq!(done.rest, "read");
        assert!(done.complete);
        assert!(split("IN:api ").complete);

        let middle = split("bq in:ware write");
        assert_eq!(middle.rest, "bq write");
        assert_eq!(split("in:").token.as_deref(), Some(""));
    }

    #[test]
    fn a_parent_loop_does_not_hang() {
        let scopes = [
            scope(Scope::Folder, "1", "one", Some("folders/2")),
            scope(Scope::Folder, "2", "two", Some("folders/1")),
        ];
        let tree = ScopeTree::build(&scopes, &[ent(Scope::Folder, "1", "x")]);
        assert_eq!(tree.rows().len(), 2);
    }
}
