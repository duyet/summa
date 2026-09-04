use crate::config::ClickHouseConfig;
use crate::model::{DataSink, EventRow, EventsSnapshotData, SinkResult};
use async_trait::async_trait;
use reqwest::Client;
use std::collections::HashMap;

const CH_DELETE_BATCH: usize = 50;

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Escape a single-quoted SQL string literal by doubling embedded quotes.
fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// Build the base CREATE TABLE statement (without deferred columns).
fn click_house_create_sql() -> &'static str {
    "CREATE TABLE IF NOT EXISTS ccusage_events (\n\
     date Date,\n\
     record_type String,\n\
     record_key String,\n\
     source String DEFAULT 'ccusage',\n\
     machine_name String,\n\
     account_id String DEFAULT '',\n\
     api_key_id String DEFAULT '',\n\
     model_name String DEFAULT '',\n\
     session_id String DEFAULT '',\n\
     project_path String DEFAULT '',\n\
     input_tokens UInt64 DEFAULT 0,\n\
     output_tokens UInt64 DEFAULT 0,\n\
     cache_creation_tokens UInt64 DEFAULT 0,\n\
     cache_read_tokens UInt64 DEFAULT 0,\n\
     reasoning_tokens UInt64 DEFAULT 0,\n\
     total_tokens UInt64 DEFAULT 0,\n\
     cost Float64 DEFAULT 0,\n\
     dedup_key String DEFAULT '',\n\
     import_id String DEFAULT '',\n\
     block_id String DEFAULT '',\n\
     start_time Nullable(DateTime),\n\
     end_time Nullable(DateTime),\n\
     actual_end_time Nullable(DateTime),\n\
     is_active UInt8 DEFAULT 0,\n\
     is_gap UInt8 DEFAULT 0,\n\
     entries UInt32 DEFAULT 0,\n\
     burn_rate Nullable(Float64),\n\
     created_at DateTime DEFAULT now(),\n\
     updated_at DateTime DEFAULT now()\n\
     )\n\
     ENGINE = ReplacingMergeTree(updated_at)\n\
     PARTITION BY toYYYYMM(date)\n\
     ORDER BY (account_id, source, machine_name, record_type, date, model_name, record_key)"
}

/// ALTER ADD COLUMN statements for deferred columns (idempotent).
fn click_house_alter_statements() -> Vec<&'static str> {
    vec![
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS account_id String DEFAULT '' AFTER machine_name",
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS api_key_id String DEFAULT '' AFTER account_id",
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS projection Nullable(Float64) AFTER burn_rate",
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS usage_limit_reset_time Nullable(DateTime) AFTER projection",
    ]
}

/// ClickHouse sink: writes flat event rows to `ccusage_events` via HTTP.
pub struct ClickHouseSink {
    config: Option<ClickHouseConfig>,
    client: Option<Client>,
}

impl ClickHouseSink {
    pub fn new() -> Self {
        Self {
            config: None,
            client: None,
        }
    }

    fn base_url(&self) -> String {
        let cfg = self
            .config
            .as_ref()
            .expect("ClickHouseSink not connected");
        format!("{}://{}:{}", cfg.protocol, cfg.host, cfg.port)
    }

    async fn run_query(&self, query: &str) -> anyhow::Result<()> {
        let client = self
            .client
            .as_ref()
            .expect("ClickHouseSink not connected");
        // POST the SQL as the body so reqwest sends Content-Length. A query-string
        // POST with no body is rejected by ClickHouse HTTP as 411 Length Required.
        client
            .post(self.base_url())
            .basic_auth(
                self.config
                    .as_ref()
                    .map(|c| c.user.as_str())
                    .unwrap_or("default"),
                self.config
                    .as_ref()
                    .and_then(|c| if c.password.is_empty() { None } else { Some(c.password.as_str()) }),
            )
            .header("Content-Type", "text/plain; charset=UTF-8")
            .body(query.to_string())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn run_command(&self, query: &str) -> anyhow::Result<()> {
        let client = self
            .client
            .as_ref()
            .expect("ClickHouseSink not connected");
        let url = format!("{}/?query={}", self.base_url(), percent_encode(query));
        client
            .post(&url)
            .basic_auth(
                self.config
                    .as_ref()
                    .map(|c| c.user.as_str())
                    .unwrap_or("default"),
                self.config
                    .as_ref()
                    .and_then(|c| if c.password.is_empty() { None } else { Some(c.password.as_str()) }),
            )
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    fn select_url(&self) -> String {
        let cfg = self
            .config
            .as_ref()
            .expect("ClickHouseSink not connected");
        if cfg.database.is_empty() {
            format!("{}/", self.base_url())
        } else {
            format!(
                "{}/?database={}",
                self.base_url(),
                percent_encode(&cfg.database)
            )
        }
    }

    pub async fn query_text(&self, query: &str) -> anyhow::Result<String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ClickHouseSink not connected"))?;
        let cfg = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ClickHouseSink not connected"))?;
        let resp = client
            .post(self.select_url())
            .basic_auth(
                if cfg.user.is_empty() {
                    "default"
                } else {
                    cfg.user.as_str()
                },
                if cfg.password.is_empty() {
                    None
                } else {
                    Some(cfg.password.as_str())
                },
            )
            .header("Content-Type", "text/plain; charset=UTF-8")
            .body(query.to_string())
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.text().await?)
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let text = self.query_text("SELECT 1").await?;
        if text.trim().starts_with('1') {
            Ok(())
        } else {
            anyhow::bail!("clickhouse ping: {}", text.trim())
        }
    }

    /// Distinct `(date, record_type, source, machine_name)` scopes present in `rows`.
    fn scopes_of(rows: &[EventRow]) -> Vec<(String, String, String, String)> {
        let mut scopes = Vec::new();
        let mut seen: HashMap<(String, String, String, String), ()> = HashMap::new();
        for row in rows {
            let key = (
                row.date.clone(),
                row.record_type.clone(),
                row.source.clone(),
                row.machine_name.clone(),
            );
            if seen.insert(key, ()).is_none() {
                scopes.push(key);
            }
        }
        scopes
    }

    /// Build the retire-previous-scope mutation: delete rows in the given scopes
    /// that were written by a *different* import run. Keeping every row whose
    /// `import_id` is in `keep_import_ids` (the ids present in this write batch)
    /// is what makes the insert-then-retire order crash-safe (issue #101). Using
    /// the whole batch's id set — not a single id — guarantees the retire never
    /// deletes a row this write just inserted, even when a batch mixes ids (e.g.
    /// `publish` forwarding rows from many historical runs in one call).
    fn retire_scopes_sql(
        scopes: &[(String, String, String, String)],
        keep_import_ids: &[String],
    ) -> Option<String> {
        if scopes.is_empty() || keep_import_ids.is_empty() {
            return None;
        }
        let mut parts = Vec::with_capacity(scopes.len());
        for (date, record_type, source, machine_name) in scopes {
            parts.push(format!(
                "(date = '{}' AND record_type = '{}' AND source = '{}' AND machine_name = '{}')",
                escape_sql_literal(date),
                escape_sql_literal(record_type),
                escape_sql_literal(source),
                escape_sql_literal(machine_name),
            ));
        }
        let keep_list = keep_import_ids
            .iter()
            .map(|id| format!("'{}'", escape_sql_literal(id)))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "ALTER TABLE ccusage_events DELETE WHERE ({}) AND import_id NOT IN ({})",
            parts.join(" OR "),
            keep_list,
        ))
    }

    pub async fn delete_by_dedup_keys(&self, keys: &[String]) -> anyhow::Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        const KEY_BATCH: usize = 200;
        for chunk in keys.chunks(KEY_BATCH) {
            let list = chunk
                .iter()
                .filter(|k| !k.is_empty())
                .map(|k| format!("'{}'", escape_sql_literal(k)))
                .collect::<Vec<_>>();
            if list.is_empty() {
                continue;
            }
            let query = format!(
                "ALTER TABLE ccusage_events DELETE WHERE dedup_key IN ({})",
                list.join(",")
            );
            self.run_query(&query).await?;
        }
        Ok(())
    }

    pub async fn insert_events(&self, rows: &[EventRow]) -> anyhow::Result<()> {
        const CHUNK_SIZE: usize = 1000;
        for chunk in rows.chunks(CHUNK_SIZE) {
            self.insert_rows(chunk).await?;
        }
        Ok(())
    }

    async fn insert_rows(&self, rows: &[EventRow]) -> anyhow::Result<()> {
        let client = self
            .client
            .as_ref()
            .expect("ClickHouseSink not connected");
        let url = format!(
            "{}/?query=INSERT+INTO+ccusage_events+FORMAT+JSONEachRow",
            self.base_url()
        );

        // Serialize each row as a JSON object on its own line.
        let mut body = String::with_capacity(rows.len() * 512);
        for row in rows {
            body.push_str(&serde_json::to_string(row)?);
            body.push('\n');
        }

        client
            .post(&url)
            .basic_auth(
                self.config
                    .as_ref()
                    .map(|c| c.user.as_str())
                    .unwrap_or("default"),
                self.config
                    .as_ref()
                    .and_then(|c| if c.password.is_empty() { None } else { Some(c.password.as_str()) }),
            )
            .header("Content-Type", "application/x-ndjson")
            .body(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[async_trait]
impl DataSink for ClickHouseSink {
    fn name(&self) -> &'static str {
        "clickhouse"
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        let cfg = ClickHouseConfig::from_env();
        self.config = Some(cfg);
        self.client = Some(Client::new());

        // Ensure table exists.
        self.run_query(click_house_create_sql()).await?;
        for stmt in click_house_alter_statements() {
            // Idempotent: ignore "column already exists" errors.
            let _ = self.run_query(stmt).await;
        }
        Ok(())
    }

    async fn write(&mut self, data: EventsSnapshotData) -> anyhow::Result<SinkResult> {
        let start = std::time::Instant::now();
        let mut result = SinkResult {
            sink_name: self.name().to_string(),
            tables_written: Vec::new(),
            rows_written: HashMap::new(),
            duration_ms: 0,
            error: None,
        };

        let rows = data.events;
        if rows.is_empty() {
            result.duration_ms = start.elapsed().as_millis() as u64;
            return Ok(result);
        }

        // Stamp any rows that arrived without an import_id (older sources / manual
        // callers) with a fresh run id so the retire step keeps them, then collect
        // the full set of ids present in this batch. The retire excludes every one
        // of them, so it can never delete a row this write just inserted — even
        // when a batch mixes ids (publish forwards rows from many runs at once).
        let mut rows = rows;
        let mut keep_import_ids: Vec<String> = Vec::new();
        for r in &mut rows {
            if r.import_id.is_empty() {
                r.import_id = uuid::Uuid::new_v4().to_string();
            }
            if !keep_import_ids.contains(&r.import_id) {
                keep_import_ids.push(r.import_id.clone());
            }
        }

        // Insert first, then retire rows from prior runs in the touched scopes.
        // Crash after the insert (before or during the retire) leaves duplicates,
        // not holes — ReplacingMergeTree collapses them by (ORDER BY, updated_at)
        // and the next successful run's retire re-removes the stale set. The
        // retire always excludes this batch's import_ids, so it never deletes the
        // rows this write just inserted. (Issue #101.)
        const CHUNK_SIZE: usize = 1000;
        let mut inserted = 0;
        for chunk in rows.chunks(CHUNK_SIZE) {
            self.insert_rows(chunk).await?;
            inserted += chunk.len();
        }

        let scopes = Self::scopes_of(&rows);
        for chunk in scopes.chunks(CH_DELETE_BATCH) {
            if let Some(query) = Self::retire_scopes_sql(chunk, &keep_import_ids) {
                self.run_query(&query).await?;
            }
        }

        result.tables_written.push("ccusage_events".to_string());
        result.rows_written.insert("ccusage_events".to_string(), inserted as u64);
        result.duration_ms = start.elapsed().as_millis() as u64;
        Ok(result)
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.client = None;
        self.config = None;
        Ok(())
    }
}

impl Default for ClickHouseSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(date: &str, rt: &str, src: &str, machine: &str, import_id: &str) -> EventRow {
        EventRow {
            date: date.into(),
            record_type: rt.into(),
            source: src.into(),
            machine_name: machine.into(),
            import_id: import_id.into(),
            ..EventRow::default()
        }
    }

    #[test]
    fn scopes_of_dedups_scope_tuples() {
        let rows = vec![
            row("2026-08-20", "daily", "ccusage", "box", "run-1"),
            row("2026-08-20", "daily", "ccusage", "box", "run-1"),
            row("2026-08-21", "daily", "ccusage", "box", "run-1"),
            row("2026-08-20", "daily", "cursor", "account", "run-1"),
        ];
        let scopes = ClickHouseSink::scopes_of(&rows);
        assert_eq!(scopes.len(), 3);
    }

    #[test]
    fn retire_sql_excludes_current_run() {
        let scopes = vec![
            ("2026-08-20".into(), "daily".into(), "ccusage".into(), "box".into()),
        ];
        let sql = ClickHouseSink::retire_scopes_sql(&scopes, &["run-9".into()]).unwrap();
        assert!(
            sql.contains("import_id NOT IN ('run-9')"),
            "retire must keep the current run's rows: {sql}"
        );
        assert!(
            sql.contains("machine_name = 'box'") && sql.contains("source = 'ccusage'"),
            "retire must scope to the touched scope: {sql}"
        );
    }

    #[test]
    fn retire_sql_keeps_every_batch_id() {
        // publish() forwards rows from many historical runs in one batch; the
        // retire must keep all of them, not just the first.
        let scopes = vec![
            ("2026-08-20".into(), "daily".into(), "ccusage".into(), "box".into()),
        ];
        let sql =
            ClickHouseSink::retire_scopes_sql(&scopes, &["run-1".into(), "run-2".into()]).unwrap();
        assert!(
            sql.contains("import_id NOT IN ('run-1', 'run-2')"),
            "retire must keep every batch import_id: {sql}"
        );
    }

    #[test]
    fn retire_sql_escaped_quote_cannot_inject() {
        let scopes = vec![
            ("2026-08-20".into(), "daily".into(), "ccusage".into(), "a'b".into()),
        ];
        let sql = ClickHouseSink::retire_scopes_sql(&scopes, &["x'y".into()]).unwrap();
        assert!(sql.contains("machine_name = 'a''b'"));
        assert!(sql.contains("import_id NOT IN ('x''y')"));
    }

    #[test]
    fn retire_sql_none_for_empty_scopes_or_ids() {
        assert!(ClickHouseSink::retire_scopes_sql(&[], &["run-1".into()]).is_none());
        assert!(ClickHouseSink::retire_scopes_sql(
            &[("2026-08-20".into(), "daily".into(), "ccusage".into(), "box".into())],
            &[]
        )
        .is_none());
    }
}
