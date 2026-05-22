CREATE TABLE IF NOT EXISTS projects (
  project_id   TEXT PRIMARY KEY,
  display_name TEXT,
  state        TEXT,
  fetched_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS folders (
  folder_id    TEXT PRIMARY KEY,
  display_name TEXT,
  parent       TEXT,
  fetched_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS organizations (
  org_id       TEXT PRIMARY KEY,
  display_name TEXT,
  fetched_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS entitlements (
  name                       TEXT PRIMARY KEY,
  scope_type                 TEXT NOT NULL,
  scope_id                   TEXT NOT NULL,
  short_name                 TEXT NOT NULL,
  max_request_duration_secs  INTEGER,
  justification_required     INTEGER NOT NULL,
  approvers_json             TEXT NOT NULL DEFAULT '[]',
  roles_json                 TEXT NOT NULL DEFAULT '[]',
  raw_json                   TEXT NOT NULL,
  fetched_at                 INTEGER NOT NULL,
  last_used_at               INTEGER
);
CREATE INDEX IF NOT EXISTS entitlements_scope      ON entitlements(scope_type, scope_id);
CREATE INDEX IF NOT EXISTS entitlements_short_name ON entitlements(short_name);
CREATE INDEX IF NOT EXISTS entitlements_last_used  ON entitlements(last_used_at);
