-- Analytics mirror: daily rows only, so /v1/analytics keeps working when
-- MotherDuck and the homelab ClickHouse are both unreachable. Small table by
-- design; session/block rows stay in the warehouses only.
CREATE TABLE IF NOT EXISTS ccusage_events (
  dedup_key TEXT PRIMARY KEY,
  account_id TEXT NOT NULL DEFAULT '',
  api_key_id TEXT NOT NULL DEFAULT '',
  date TEXT NOT NULL,
  record_type TEXT NOT NULL DEFAULT 'daily',
  source TEXT NOT NULL DEFAULT '',
  machine_name TEXT NOT NULL DEFAULT '',
  model_name TEXT NOT NULL DEFAULT '',
  input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  reasoning_tokens INTEGER NOT NULL DEFAULT 0,
  total_tokens INTEGER NOT NULL DEFAULT 0,
  cost REAL NOT NULL DEFAULT 0,
  entries INTEGER NOT NULL DEFAULT 0,
  import_id TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT '',
  updated_at TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS ccusage_events_acct_date
  ON ccusage_events (account_id, date, record_type);
