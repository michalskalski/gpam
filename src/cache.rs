use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::gcp::Scope;

/// Schema version for the cache-only tables (projects, folders, organizations,
/// entitlements). Bumping this drops and recreates those tables.
/// The next refresh repopulates them.
const CACHE_SCHEMA_VERSION: i64 = 1;

/// Highest applied migration in [`STATE_MIGRATIONS`]. Bumping this is a real
/// migration.
const STATE_SCHEMA_VERSION: i64 = 1;

const META_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);";

const CACHE_SCHEMA: &str = include_str!("migrations/cache.sql");

/// Forward-only user-state (grants) migrations.
const STATE_MIGRATIONS: &[(i64, &str)] =
    &[(1, include_str!("migrations/state_001_grants_initial.sql"))];

fn ensure_schema(conn: &mut Connection, backup_target: Option<&Path>) -> Result<()> {
    conn.execute_batch(META_SCHEMA)?;
    ensure_cache_schema(conn)?;
    ensure_state_schema(conn, backup_target)?;
    Ok(())
}

fn read_version(conn: &Connection, key: &str) -> Result<Option<i64>> {
    let raw: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get::<_, String>("value")
        })
        .optional()?;
    Ok(raw.and_then(|s| s.parse().ok()))
}

fn write_version(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value.to_string()],
    )?;
    Ok(())
}

/// The cache tables are pure projections of GCP state, so any version mismatch
/// is handled by drop-and-recreate.
fn ensure_cache_schema(conn: &Connection) -> Result<()> {
    let stored = read_version(conn, meta_keys::CACHE_SCHEMA_VERSION)?;
    if stored.is_some() && stored != Some(CACHE_SCHEMA_VERSION) {
        conn.execute_batch(
            "DROP TABLE IF EXISTS entitlements;
             DROP TABLE IF EXISTS projects;
             DROP TABLE IF EXISTS folders;
             DROP TABLE IF EXISTS organizations;
             DELETE FROM meta WHERE key IN (
                 'projects_scanned_at',
                 'folders_scanned_at',
                 'organizations_scanned_at',
                 'entitlements_scanned_at'
             );",
        )?;
    }
    conn.execute_batch(CACHE_SCHEMA)?;
    write_version(conn, meta_keys::CACHE_SCHEMA_VERSION, CACHE_SCHEMA_VERSION)?;
    Ok(())
}

/// Run every pending entry in [`STATE_MIGRATIONS`] inside a single transaction.
/// A DB whose stored version is *higher* than this binary knows about is a
/// downgrade attempt. We bail rather than silently wiping anything.
fn ensure_state_schema(conn: &mut Connection, backup_target: Option<&Path>) -> Result<()> {
    let stored = read_version(conn, meta_keys::STATE_SCHEMA_VERSION)?.unwrap_or(0);
    if stored > STATE_SCHEMA_VERSION {
        bail!(
            "cache state schema is v{stored}, newer than this gpam (v{}). \
             Refusing to downgrade. Use a newer binary or delete the cache file.",
            STATE_SCHEMA_VERSION,
        );
    }
    let pending: Vec<&(i64, &str)> = STATE_MIGRATIONS
        .iter()
        .filter(|(v, _)| *v > stored)
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    // First-ever creation (stored == 0) has nothing to back up. Real upgrades
    // copy the DB aside so a broken migration leaves a recovery point.
    if stored > 0
        && let Some(path) = backup_target
    {
        let bak = path.with_extension(format!("db.v{stored}.bak"));
        if bak.exists() {
            warn!(path = %bak.display(), "overwriting existing pre-migration backup");
        }
        if let Err(e) = std::fs::copy(path, &bak) {
            warn!(error = %e, "could not back up cache before state migration");
        }
    }
    let tx = conn.transaction()?;
    for (v, sql) in pending {
        tx.execute_batch(sql)
            .with_context(|| format!("applying state migration v{v}"))?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES(?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![meta_keys::STATE_SCHEMA_VERSION, v.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitlementRow {
    pub name: String,
    pub scope_type: Scope,
    pub scope_id: String,
    /// Human-readable name for the scope, joined from the projects/folders/
    /// organizations tables at read time (or stamped from discovery during a
    /// streaming refresh). Not persisted directly on the entitlement.
    pub scope_display_name: Option<String>,
    pub short_name: String,
    pub max_request_duration_secs: Option<i64>,
    pub justification_required: bool,
    pub approvers: Vec<String>,
    pub roles: Vec<String>,
    pub raw_json: String,
    pub fetched_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRow {
    pub project_id: String,
    pub display_name: Option<String>,
    pub state: Option<String>,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderRow {
    pub folder_id: String,
    pub display_name: Option<String>,
    pub parent: Option<String>,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationRow {
    pub org_id: String,
    pub display_name: Option<String>,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRow {
    pub name: String,
    pub entitlement_name: String,
    pub entitlement_short_name: String,
    pub scope_type: Scope,
    pub scope_id: String,
    /// Joined from projects/folders/organizations at read time. Not persisted.
    pub scope_display_name: Option<String>,
    pub short_id: String,
    pub state: String,
    pub requested_duration_secs: i64,
    pub justification: Option<String>,
    pub created_at: i64,
    pub activated_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub last_polled_at: Option<i64>,
    pub raw_json: Option<String>,
}

// wrap in Arc<Mutex<>>, one connection, should be ok for tui
pub struct Cache {
    conn: Connection,
}

impl Cache {
    pub fn open(account: &str) -> Result<Self> {
        let path = db_path(account)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        ensure_schema(&mut conn, Some(&path))?;
        Ok(Self { conn })
    }

    #[allow(dead_code)] // used by integration tests
    pub fn open_at(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        ensure_schema(&mut conn, Some(path))?;
        Ok(Self { conn })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES (?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(value)
    }

    pub fn meta_timestamp(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.get_meta(key)?.and_then(|s| s.parse().ok()))
    }

    pub fn set_meta_timestamp(&self, key: &str, ts: i64) -> Result<()> {
        self.set_meta(key, &ts.to_string())
    }

    /// Insert or update an entitlement. `last_used_at` is preserved across
    /// refreshes; only [`Cache::bump_used`] touches it.
    pub fn upsert_entitlement(&self, row: &EntitlementRow) -> Result<()> {
        let approvers = serde_json::to_string(&row.approvers).unwrap_or_else(|_| "[]".into());
        let roles = serde_json::to_string(&row.roles).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO entitlements(
                name, scope_type, scope_id, short_name, max_request_duration_secs,
                justification_required, approvers_json, roles_json, raw_json, fetched_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(name) DO UPDATE SET
                scope_type=excluded.scope_type,
                scope_id=excluded.scope_id,
                short_name=excluded.short_name,
                max_request_duration_secs=excluded.max_request_duration_secs,
                justification_required=excluded.justification_required,
                approvers_json=excluded.approvers_json,
                roles_json=excluded.roles_json,
                raw_json=excluded.raw_json,
                fetched_at=excluded.fetched_at",
            params![
                row.name,
                row.scope_type.as_str(),
                row.scope_id,
                row.short_name,
                row.max_request_duration_secs,
                row.justification_required as i64,
                approvers,
                roles,
                row.raw_json,
                row.fetched_at,
            ],
        )?;
        Ok(())
    }

    pub fn bump_used(&self, name: &str, ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE entitlements SET last_used_at=?2 WHERE name=?1",
            params![name, ts],
        )?;
        Ok(())
    }

    pub fn upsert_project(&self, row: &ProjectRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO projects(project_id, display_name, state, fetched_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(project_id) DO UPDATE SET
                display_name=excluded.display_name,
                state=excluded.state,
                fetched_at=excluded.fetched_at",
            params![row.project_id, row.display_name, row.state, row.fetched_at],
        )?;
        Ok(())
    }

    pub fn upsert_folder(&self, row: &FolderRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO folders(folder_id, display_name, parent, fetched_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(folder_id) DO UPDATE SET
                display_name=excluded.display_name,
                parent=excluded.parent,
                fetched_at=excluded.fetched_at",
            params![row.folder_id, row.display_name, row.parent, row.fetched_at],
        )?;
        Ok(())
    }

    pub fn upsert_organization(&self, row: &OrganizationRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO organizations(org_id, display_name, fetched_at)
             VALUES (?1,?2,?3)
             ON CONFLICT(org_id) DO UPDATE SET
                display_name=excluded.display_name,
                fetched_at=excluded.fetched_at",
            params![row.org_id, row.display_name, row.fetched_at],
        )?;
        Ok(())
    }

    /// Most-recently-used first. Never-used entitlements fall to the bottom,
    /// ordered alphabetically by scope then short_name. Joins the scope tables
    /// so each row carries the resolved `scope_display_name` if one is known.
    pub fn list_entitlements(&self) -> Result<Vec<EntitlementRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.name, e.scope_type, e.scope_id, e.short_name,
                    e.max_request_duration_secs, e.justification_required,
                    e.approvers_json, e.roles_json, e.raw_json,
                    e.fetched_at, e.last_used_at,
                    COALESCE(p.display_name, f.display_name, o.display_name) AS scope_display_name
             FROM entitlements e
             LEFT JOIN projects      p ON e.scope_type='project'      AND e.scope_id = p.project_id
             LEFT JOIN folders       f ON e.scope_type='folder'       AND e.scope_id = f.folder_id
             LEFT JOIN organizations o ON e.scope_type='organization' AND e.scope_id = o.org_id
             ORDER BY e.last_used_at IS NULL, e.last_used_at DESC, e.scope_type, e.scope_id, e.short_name",
        )?;
        let rows = stmt.query_map([], |r| {
            let approvers_json: String = r.get("approvers_json")?;
            let approvers = serde_json::from_str(&approvers_json).unwrap_or_default();
            let roles_json: String = r.get("roles_json")?;
            let roles = serde_json::from_str(&roles_json).unwrap_or_default();
            let scope_str: String = r.get("scope_type")?;
            let scope_type = Scope::from_str(&scope_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            Ok(EntitlementRow {
                name: r.get("name")?,
                scope_type,
                scope_id: r.get("scope_id")?,
                scope_display_name: r.get("scope_display_name")?,
                short_name: r.get("short_name")?,
                max_request_duration_secs: r.get("max_request_duration_secs")?,
                justification_required: r.get::<_, i64>("justification_required")? != 0,
                approvers,
                roles,
                raw_json: r.get("raw_json")?,
                fetched_at: r.get("fetched_at")?,
                last_used_at: r.get("last_used_at")?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn upsert_grant(&self, row: &GrantRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO grants(
                name, entitlement_name, entitlement_short_name, scope_type, scope_id, short_id,
                state, requested_duration_secs, justification, created_at,
                activated_at, expires_at, last_polled_at, raw_json
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(name) DO UPDATE SET
                entitlement_name=excluded.entitlement_name,
                entitlement_short_name=excluded.entitlement_short_name,
                scope_type=excluded.scope_type,
                scope_id=excluded.scope_id,
                short_id=excluded.short_id,
                state=excluded.state,
                requested_duration_secs=excluded.requested_duration_secs,
                justification=excluded.justification,
                created_at=excluded.created_at,
                activated_at=excluded.activated_at,
                expires_at=excluded.expires_at,
                last_polled_at=excluded.last_polled_at,
                raw_json=excluded.raw_json",
            params![
                row.name,
                row.entitlement_name,
                row.entitlement_short_name,
                row.scope_type.as_str(),
                row.scope_id,
                row.short_id,
                row.state,
                row.requested_duration_secs,
                row.justification,
                row.created_at,
                row.activated_at,
                row.expires_at,
                row.last_polled_at,
                row.raw_json,
            ],
        )?;
        Ok(())
    }

    /// Patch the dynamic fields of a tracked grant after a poll.
    /// `activated_at` and `expires_at` are only written when their argument is
    /// `Some`, that way the first observation of `Active` can stamp them and
    /// later polls leave them alone.
    pub fn update_grant_state(
        &self,
        name: &str,
        state: &str,
        activated_at: Option<i64>,
        expires_at: Option<i64>,
        last_polled_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE grants
             SET state=?2,
                 activated_at=COALESCE(?3, activated_at),
                 expires_at=COALESCE(?4, expires_at),
                 last_polled_at=?5
             WHERE name=?1",
            params![name, state, activated_at, expires_at, last_polled_at],
        )?;
        Ok(())
    }

    /// Rows that should appear in the live strip:
    /// every non-terminal grant, plus terminal rows whose `expires_at` is
    /// within the last hour so the user can see the outcome briefly.
    pub fn list_tracked_grants(&self, now: i64) -> Result<Vec<GrantRow>> {
        const FRESH_TERMINAL_WINDOW_SECS: i64 = 3600;
        let cutoff = now - FRESH_TERMINAL_WINDOW_SECS;
        let mut stmt = self.conn.prepare(
            "SELECT g.name, g.entitlement_name, g.entitlement_short_name,
                    g.scope_type, g.scope_id, g.short_id,
                    g.state, g.requested_duration_secs, g.justification, g.created_at,
                    g.activated_at, g.expires_at, g.last_polled_at, g.raw_json,
                    COALESCE(p.display_name, f.display_name, o.display_name) AS scope_display_name
             FROM grants g
             LEFT JOIN projects      p ON g.scope_type='project'      AND g.scope_id = p.project_id
             LEFT JOIN folders       f ON g.scope_type='folder'       AND g.scope_id = f.folder_id
             LEFT JOIN organizations o ON g.scope_type='organization' AND g.scope_id = o.org_id
             WHERE g.state NOT IN ('Denied','Expired','Revoked','Ended','Withdrawn','ActivationFailed',
                                   'DENIED','EXPIRED','REVOKED','ENDED','WITHDRAWN','ACTIVATION_FAILED')
                OR COALESCE(g.expires_at, g.last_polled_at, g.created_at) >= ?1
             ORDER BY g.created_at DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], grant_row_from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn grant_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<GrantRow> {
    let scope_str: String = r.get("scope_type")?;
    let scope_type = Scope::from_str(&scope_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    // scope_display_name is added by list_tracked_grants' JOIN. When called
    // from a SELECT that lacks the column, treat its absence as None.
    let scope_display_name = r
        .get::<_, Option<String>>("scope_display_name")
        .ok()
        .flatten();
    Ok(GrantRow {
        name: r.get("name")?,
        entitlement_name: r.get("entitlement_name")?,
        entitlement_short_name: r.get("entitlement_short_name")?,
        scope_type,
        scope_id: r.get("scope_id")?,
        scope_display_name,
        short_id: r.get("short_id")?,
        state: r.get("state")?,
        requested_duration_secs: r.get("requested_duration_secs")?,
        justification: r.get("justification")?,
        created_at: r.get("created_at")?,
        activated_at: r.get("activated_at")?,
        expires_at: r.get("expires_at")?,
        last_polled_at: r.get("last_polled_at")?,
        raw_json: r.get("raw_json")?,
    })
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn db_path(account: &str) -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "gpam")
        .context("locating cache dir (set XDG_CACHE_HOME or HOME)")?;
    let safe = account.replace(
        |c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '@',
        "_",
    );
    Ok(dirs.cache_dir().join(format!("{safe}.db")))
}

pub mod meta_keys {
    pub const PROJECTS_SCANNED_AT: &str = "projects_scanned_at";
    pub const FOLDERS_SCANNED_AT: &str = "folders_scanned_at";
    pub const ORGANIZATIONS_SCANNED_AT: &str = "organizations_scanned_at";
    pub const ENTITLEMENTS_SCANNED_AT: &str = "entitlements_scanned_at";
    pub const CACHE_SCHEMA_VERSION: &str = "cache_schema_version";
    pub const STATE_SCHEMA_VERSION: &str = "state_schema_version";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache() -> Cache {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let cache = Cache::open_at(&path).unwrap();
        std::mem::forget(dir);
        cache
    }

    fn ent(scope: Scope, scope_id: &str, name: &str, short: &str) -> EntitlementRow {
        EntitlementRow {
            name: name.into(),
            scope_type: scope,
            scope_id: scope_id.into(),
            scope_display_name: None,
            short_name: short.into(),
            max_request_duration_secs: Some(3600),
            justification_required: true,
            approvers: vec!["user:alice@example.com".into()],
            roles: vec!["roles/viewer".into()],
            raw_json: r#"{"name":"x"}"#.into(),
            fetched_at: 1700000000,
            last_used_at: None,
        }
    }

    #[test]
    fn round_trip_entitlement() {
        let cache = temp_cache();
        let row = ent(
            Scope::Project,
            "p",
            "projects/p/locations/global/entitlements/e1",
            "e1",
        );
        cache.upsert_entitlement(&row).unwrap();
        let rows = cache.list_entitlements().unwrap();
        assert_eq!(rows.len(), 1);
        let got = &rows[0];
        assert_eq!(got.name, row.name);
        assert_eq!(got.scope_type, Scope::Project);
        assert_eq!(got.scope_id, "p");
        assert_eq!(got.approvers, row.approvers);
        assert!(got.justification_required);
        assert_eq!(got.last_used_at, None);
    }

    #[test]
    fn list_entitlements_joins_scope_display_name() {
        let cache = temp_cache();
        cache
            .upsert_folder(&FolderRow {
                folder_id: "12345".into(),
                display_name: Some("Platform Team".into()),
                parent: Some("organizations/9".into()),
                fetched_at: 1700000000,
            })
            .unwrap();
        cache
            .upsert_organization(&OrganizationRow {
                org_id: "9".into(),
                display_name: Some("example.com".into()),
                fetched_at: 1700000000,
            })
            .unwrap();
        cache
            .upsert_entitlement(&ent(
                Scope::Folder,
                "12345",
                "folders/12345/locations/global/entitlements/f1",
                "f1",
            ))
            .unwrap();
        cache
            .upsert_entitlement(&ent(
                Scope::Organization,
                "9",
                "organizations/9/locations/global/entitlements/o1",
                "o1",
            ))
            .unwrap();
        // Folder without a stored display_name still reads back as None.
        cache
            .upsert_entitlement(&ent(
                Scope::Folder,
                "99999",
                "folders/99999/locations/global/entitlements/f2",
                "f2",
            ))
            .unwrap();

        let rows = cache.list_entitlements().unwrap();
        let by_name = |short: &str| {
            rows.iter()
                .find(|r| r.short_name == short)
                .cloned()
                .unwrap()
        };
        assert_eq!(
            by_name("f1").scope_display_name.as_deref(),
            Some("Platform Team")
        );
        assert_eq!(
            by_name("o1").scope_display_name.as_deref(),
            Some("example.com")
        );
        assert_eq!(by_name("f2").scope_display_name, None);
    }

    #[test]
    fn bump_used_orders_recent_first() {
        let cache = temp_cache();
        for (n, short) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
            let row = EntitlementRow {
                name: format!("projects/p/locations/global/entitlements/{n}"),
                scope_type: Scope::Project,
                scope_id: "p".into(),
                scope_display_name: None,
                short_name: short.into(),
                max_request_duration_secs: None,
                justification_required: false,
                approvers: vec![],
                roles: vec![],
                raw_json: "{}".into(),
                fetched_at: 1700000000,
                last_used_at: None,
            };
            cache.upsert_entitlement(&row).unwrap();
        }
        cache
            .bump_used("projects/p/locations/global/entitlements/b", 1700001000)
            .unwrap();
        cache
            .bump_used("projects/p/locations/global/entitlements/c", 1700002000)
            .unwrap();

        let rows = cache.list_entitlements().unwrap();
        assert_eq!(rows[0].short_name, "gamma");
        assert_eq!(rows[1].short_name, "beta");
        assert_eq!(rows[2].short_name, "alpha");
    }

    #[test]
    fn grant_round_trip_and_state_patch() {
        let cache = temp_cache();
        let row = GrantRow {
            name: "projects/p/locations/global/entitlements/e/grants/g1".into(),
            entitlement_name: "projects/p/locations/global/entitlements/e".into(),
            entitlement_short_name: "e".into(),
            scope_type: Scope::Project,
            scope_id: "p".into(),
            scope_display_name: None,
            short_id: "g1".into(),
            state: "Requested".into(),
            requested_duration_secs: 3600,
            justification: Some("on-call paging investigation".into()),
            created_at: 1700000000,
            activated_at: None,
            expires_at: None,
            last_polled_at: None,
            raw_json: None,
        };
        cache.upsert_grant(&row).unwrap();

        let tracked = cache.list_tracked_grants(1700000010).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].state, "Requested");
        assert_eq!(tracked[0].scope_type, Scope::Project);
        assert_eq!(
            tracked[0].justification.as_deref(),
            Some("on-call paging investigation")
        );

        cache
            .update_grant_state(
                &row.name,
                "Active",
                Some(1700000050),
                Some(1700003650),
                1700000050,
            )
            .unwrap();
        let tracked = cache.list_tracked_grants(1700000060).unwrap();
        assert_eq!(tracked[0].state, "Active");
        assert_eq!(tracked[0].activated_at, Some(1700000050));
        assert_eq!(tracked[0].expires_at, Some(1700003650));

        // A second patch without timestamps should leave them intact.
        cache
            .update_grant_state(&row.name, "Active", None, None, 1700000100)
            .unwrap();
        let tracked = cache.list_tracked_grants(1700000110).unwrap();
        assert_eq!(tracked[0].activated_at, Some(1700000050));
        assert_eq!(tracked[0].expires_at, Some(1700003650));
        assert_eq!(tracked[0].last_polled_at, Some(1700000100));
    }

    #[test]
    fn list_tracked_drops_old_terminal_rows() {
        let cache = temp_cache();
        for (id, state, expires) in [
            ("g1", "Active", Some(2_000_000_000_i64)),
            ("g2", "Ended", Some(1_700_000_000_i64)),
            ("g3", "Ended", Some(1_700_003_500_i64)),
        ] {
            let row = GrantRow {
                name: format!("ent/grants/{id}"),
                entitlement_name: "ent".into(),
                entitlement_short_name: "ent-short".into(),
                scope_type: Scope::Project,
                scope_id: "proj".into(),
                scope_display_name: None,
                short_id: id.into(),
                state: state.into(),
                requested_duration_secs: 3600,
                justification: None,
                created_at: 1_700_000_000,
                activated_at: None,
                expires_at: expires,
                last_polled_at: None,
                raw_json: None,
            };
            cache.upsert_grant(&row).unwrap();
        }
        // Now = 1_700_004_000. Window = 1 hour back -> 1_700_000_400.
        // g1 (Active) always shown; g2 (Ended, expired far in the past) dropped;
        // g3 (Ended, expired 8 minutes ago) shown.
        let tracked = cache.list_tracked_grants(1_700_004_000).unwrap();
        let ids: Vec<&str> = tracked.iter().map(|g| g.short_id.as_str()).collect();
        assert!(ids.contains(&"g1"));
        assert!(ids.contains(&"g3"));
        assert!(!ids.contains(&"g2"));
    }

    #[test]
    fn fresh_open_records_both_schema_versions() {
        let cache = temp_cache();
        let cache_v = read_version(&cache.conn, meta_keys::CACHE_SCHEMA_VERSION).unwrap();
        let state_v = read_version(&cache.conn, meta_keys::STATE_SCHEMA_VERSION).unwrap();
        assert_eq!(cache_v, Some(CACHE_SCHEMA_VERSION));
        assert_eq!(state_v, Some(STATE_SCHEMA_VERSION));
    }

    #[test]
    fn second_open_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repeat.db");
        {
            let cache = Cache::open_at(&path).unwrap();
            cache
                .upsert_grant(&GrantRow {
                    name: "ent/grants/g1".into(),
                    entitlement_name: "ent".into(),
                    entitlement_short_name: "ent".into(),
                    scope_type: Scope::Project,
                    scope_id: "p".into(),
                    scope_display_name: None,
                    short_id: "g1".into(),
                    state: "Active".into(),
                    requested_duration_secs: 3600,
                    justification: None,
                    created_at: 1_700_000_000,
                    activated_at: Some(1_700_000_000),
                    expires_at: Some(1_700_003_600),
                    last_polled_at: None,
                    raw_json: None,
                })
                .unwrap();
        }
        let cache = Cache::open_at(&path).unwrap();
        let tracked = cache.list_tracked_grants(1_700_000_500).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].short_id, "g1");
    }

    #[test]
    fn rejects_future_state_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let cache = Cache::open_at(&path).unwrap();
            write_version(
                &cache.conn,
                meta_keys::STATE_SCHEMA_VERSION,
                STATE_SCHEMA_VERSION + 1,
            )
            .unwrap();
        }
        let err = match Cache::open_at(&path) {
            Ok(_) => panic!("expected open to refuse future schema"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("Refusing to downgrade"),
            "got: {err}"
        );
    }
}
