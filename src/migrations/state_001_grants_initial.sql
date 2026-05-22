CREATE TABLE IF NOT EXISTS grants (
  name                     TEXT PRIMARY KEY,
  entitlement_name         TEXT NOT NULL,
  entitlement_short_name   TEXT NOT NULL,
  scope_type               TEXT NOT NULL,
  scope_id                 TEXT NOT NULL,
  short_id                 TEXT NOT NULL,
  state                    TEXT NOT NULL,
  requested_duration_secs  INTEGER NOT NULL,
  justification            TEXT,
  created_at               INTEGER NOT NULL,
  activated_at             INTEGER,
  expires_at               INTEGER,
  last_polled_at           INTEGER,
  raw_json                 TEXT
);
CREATE INDEX IF NOT EXISTS grants_state      ON grants(state);
CREATE INDEX IF NOT EXISTS grants_expires_at ON grants(expires_at);
