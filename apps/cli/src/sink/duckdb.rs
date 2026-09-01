use crate::model::{DataSink, EventRow, EventsSnapshotData, SinkResult};
use crate::sink::csv::{csv_line, csv_value};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// DuckDB sink: writes flat event rows to `ccusage_events` using COPY FROM.
/// Supports local files and MotherDuck (`md:…` + `MOTHERDUCK_TOKEN`).
pub struct DuckDbSink {
    db_path: String,
    tables_ensured: bool,
    is_motherduck: bool,
}

fn append_motherduck_token(db_path: &str, token: &str) -> String {
    if token.is_empty() {
        return db_path.to_string();
    }
    let sep = if db_path.contains('?') { '&' } else { '?' };
    format!("{db_path}{sep}motherduck_token={token}")
}

impl DuckDbSink {
    pub fn new(db_path: impl Into<String>) -> Self {
        let db_path = db_path.into();
        let is_motherduck = db_path.starts_with("md:");
        Self {
            db_path,
            tables_ensured: false,
            is_motherduck,
        }
    }

    fn db_path(&self) -> &str {
        &self.db_path
    }

    /// Connection string including MotherDuck token when applicable.
    fn connection_string(&self) -> String {
        if !self.is_motherduck {
            return self.db_path.clone();
        }
        let token = std::env::var("MOTHERDUCK_TOKEN").unwrap_or_default();
        append_motherduck_token(&self.db_path, &token)
    }

    fn duckdb_create_sql() -> &'static str {
        "CREATE TABLE IF NOT EXISTS ccusage_events (\n\
         date DATE NOT NULL,\n\
         record_type VARCHAR NOT NULL,\n\
         record_key VARCHAR NOT NULL,\n\
         source VARCHAR NOT NULL DEFAULT 'ccusage',\n\
         machine_name VARCHAR NOT NULL,\n\
         account_id VARCHAR DEFAULT '',\n\
         api_key_id VARCHAR DEFAULT '',\n\
         model_name VARCHAR DEFAULT '',\n\
         session_id VARCHAR DEFAULT '',\n\
         project_path VARCHAR DEFAULT '',\n\
         input_tokens BIGINT DEFAULT 0,\n\
         output_tokens BIGINT DEFAULT 0,\n\
         cache_creation_tokens BIGINT DEFAULT 0,\n\
         cache_read_tokens BIGINT DEFAULT 0,\n\
         reasoning_tokens BIGINT DEFAULT 0,\n\
         total_tokens BIGINT DEFAULT 0,\n\
         cost DOUBLE DEFAULT 0,\n\
         dedup_key VARCHAR DEFAULT '',\n\
         import_id VARCHAR DEFAULT '',\n\
         block_id VARCHAR DEFAULT '',\n\
         start_time TIMESTAMP,\n\
         end_time TIMESTAMP,\n\
         actual_end_time TIMESTAMP,\n\
         is_active SMALLINT DEFAULT 0,\n\
         is_gap SMALLINT DEFAULT 0,\n\
         entries INTEGER DEFAULT 0,\n\
         burn_rate DOUBLE DEFAULT 0,\n\
         projection DOUBLE DEFAULT 0,\n\
         usage_limit_reset_time TIMESTAMP,\n\
         created_at TIMESTAMP DEFAULT current_timestamp,\n\
         updated_at TIMESTAMP DEFAULT current_timestamp\n\
         )"
    }

    fn ensure_tables(&mut self, conn: &duckdb::Connection) -> anyhow::Result<()> {
        if self.tables_ensured {
            return Ok(());
        }
        conn.execute(Self::duckdb_create_sql(), [])?;
        conn.execute(
            "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS reasoning_tokens BIGINT DEFAULT 0",
            [],
        )?;
        conn.execute(
            "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS account_id VARCHAR DEFAULT ''",
            [],
        )?;
        conn.execute(
            "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS api_key_id VARCHAR DEFAULT ''",
            [],
        )?;
        self.tables_ensured = true;
        Ok(())
    }

    /// Open a read/write connection (local file or MotherDuck).
    pub fn open_for_query(&self) -> anyhow::Result<duckdb::Connection> {
        self.open_connection()
    }

    /// Delete every row for `source` (full snapshot reset).
    pub fn purge_source(&mut self, source: &str) -> anyhow::Result<usize> {
        let conn = self.open_connection()?;
        self.ensure_tables(&conn)?;
        let sql = format!(
            "DELETE FROM ccusage_events WHERE source = '{}'",
            csv_value(source),
        );
        Ok(conn.execute(&sql, [])?)
    }

    fn open_connection(&self) -> anyhow::Result<duckdb::Connection> {
        if !self.is_motherduck {
            if let Some(parent) = Path::new(&self.db_path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            return Ok(duckdb::Connection::open(self.connection_string())?);
        }
        Self::open_motherduck(&self.connection_string())
    }

    /// MotherDuck needs its extension loaded. Prefer HTTPS (plain HTTP to
    /// extensions.duckdb.org fails with HTTP/0.9 on some networks), then open
    /// the `md:` path. Falls back to auto-install if the local cache is empty.
    fn open_motherduck(conn_str: &str) -> anyhow::Result<duckdb::Connection> {
        // duckdb also reads motherduck_token from env (lowercase).
        if let Ok(token) = std::env::var("MOTHERDUCK_TOKEN") {
            if !token.is_empty() {
                std::env::set_var("motherduck_token", &token);
            }
        }

        let bootstrap = duckdb::Connection::open_in_memory().map_err(|e| {
            anyhow::anyhow!("motherduck bootstrap open failed: {e}")
        })?;

        // Force HTTPS extension repo before INSTALL (HTTP is broken here).
        let _ = bootstrap.execute_batch(
            "SET custom_extension_repository = 'https://extensions.duckdb.org';",
        );

        // LOAD from local cache if present; otherwise INSTALL then LOAD.
        let load_result = bootstrap.execute_batch("LOAD motherduck;");
        if load_result.is_err() {
            bootstrap
                .execute_batch(
                    "SET custom_extension_repository = 'https://extensions.duckdb.org';
                     INSTALL motherduck;
                     LOAD motherduck;",
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to INSTALL/LOAD motherduck extension via HTTPS: {e}"
                    )
                })?;
        }
        drop(bootstrap);

        duckdb::Connection::open(conn_str).map_err(|e| {
            anyhow::anyhow!("motherduck open `{conn_str}` failed after LOAD: {e}")
        })
    }

    fn write_events_sync(&mut self, rows: &[EventRow]) -> anyhow::Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }

        let conn = self.open_connection()?;
        self.ensure_tables(&conn)?;

        // Dedup: delete by scoped (date, record_type, source, machine_name).
        let mut scopes: Vec<(String, String, String, String)> = Vec::new();
        let mut seen: HashMap<(String, String, String, String), usize> = HashMap::new();
        for row in rows {
            let key = (
                row.date.clone(),
                row.record_type.clone(),
                row.source.clone(),
                row.machine_name.clone(),
            );
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key.clone()) {
                e.insert(scopes.len());
                scopes.push(key);
            }
        }

        // Antigravity is a full local snapshot. Drop every prior row for that
        // machine so leftover .pb/implicit estimates do not survive a re-import
        // that only emits decoded token records (or none).
        let mut antigravity_machines: Vec<String> = Vec::new();
        for row in rows {
            if row.source == "antigravity" && !antigravity_machines.iter().any(|m| m == &row.machine_name) {
                antigravity_machines.push(row.machine_name.clone());
            }
        }
        for machine_name in &antigravity_machines {
            let sql = format!(
                "DELETE FROM ccusage_events WHERE source = 'antigravity' AND machine_name = '{}'",
                csv_value(machine_name),
            );
            conn.execute(&sql, [])?;
        }

        for (date, record_type, source, machine_name) in &scopes {
            if source == "antigravity" {
                continue;
            }
            let sql = format!(
                "DELETE FROM ccusage_events WHERE date = '{}' AND record_type = '{}' AND source = '{}' AND machine_name = '{}'",
                csv_value(date),
                csv_value(record_type),
                csv_value(source),
                csv_value(machine_name),
            );
            conn.execute(&sql, [])?;
        }

        copy_rows_from_csv(&conn, rows)?;
        Ok(rows.len())
    }

    /// Telemetry ingest: replace by `dedup_key` only (do not wipe a whole day).
    pub fn write_events_by_dedup_key(&mut self, rows: &[EventRow]) -> anyhow::Result<usize> {
        let rows: Vec<EventRow> = rows
            .iter()
            .filter(|r| !r.dedup_key.is_empty())
            .cloned()
            .collect();
        if rows.is_empty() {
            return Ok(0);
        }
        let conn = self.open_connection()?;
        self.ensure_tables(&conn)?;

        let mut keys: Vec<String> = Vec::new();
        for row in &rows {
            if !keys.iter().any(|k| k == &row.dedup_key) {
                keys.push(row.dedup_key.clone());
            }
        }
        const KEY_BATCH: usize = 200;
        for chunk in keys.chunks(KEY_BATCH) {
            let list = chunk
                .iter()
                .map(|k| format!("'{}'", csv_value(k)))
                .collect::<Vec<_>>()
                .join(",");
            conn.execute(
                &format!("DELETE FROM ccusage_events WHERE dedup_key IN ({list})"),
                [],
            )?;
        }
        copy_rows_from_csv(&conn, &rows)?;
        Ok(rows.len())
    }
}

fn copy_rows_from_csv(conn: &duckdb::Connection, rows: &[EventRow]) -> anyhow::Result<()> {
    let mut csv_lines: Vec<String> = Vec::with_capacity(rows.len() + 1);
    csv_lines.push(csv_line(&vec![
        "date".into(),
        "record_type".into(),
        "record_key".into(),
        "source".into(),
        "machine_name".into(),
        "account_id".into(),
        "api_key_id".into(),
        "model_name".into(),
        "session_id".into(),
        "project_path".into(),
        "input_tokens".into(),
        "output_tokens".into(),
        "cache_creation_tokens".into(),
        "cache_read_tokens".into(),
        "reasoning_tokens".into(),
        "total_tokens".into(),
        "cost".into(),
        "dedup_key".into(),
        "import_id".into(),
        "block_id".into(),
        "start_time".into(),
        "end_time".into(),
        "actual_end_time".into(),
        "is_active".into(),
        "is_gap".into(),
        "entries".into(),
        "burn_rate".into(),
        "projection".into(),
        "usage_limit_reset_time".into(),
        "created_at".into(),
        "updated_at".into(),
    ]));

    for row in rows {
        let values: Vec<String> = vec![
            csv_value(&row.date),
            csv_value(&row.record_type),
            csv_value(&row.record_key),
            csv_value(&row.source),
            csv_value(&row.machine_name),
            csv_value(&row.account_id),
            csv_value(&row.api_key_id),
            csv_value(&row.model_name),
            csv_value(&row.session_id),
            csv_value(&row.project_path),
            row.input_tokens.to_string(),
            row.output_tokens.to_string(),
            row.cache_creation_tokens.to_string(),
            row.cache_read_tokens.to_string(),
            row.reasoning_tokens.to_string(),
            row.total_tokens.to_string(),
            if row.cost.is_finite() {
                row.cost.to_string()
            } else {
                "0".into()
            },
            csv_value(&row.dedup_key),
            csv_value(&row.import_id),
            csv_value(&row.block_id),
            csv_opt_ts(&row.start_time),
            csv_opt_ts(&row.end_time),
            csv_opt_ts(&row.actual_end_time),
            row.is_active.to_string(),
            row.is_gap.to_string(),
            row.entries.to_string(),
            if row.burn_rate.is_finite() {
                row.burn_rate.to_string()
            } else {
                "0".into()
            },
            if row.projection.is_finite() {
                row.projection.to_string()
            } else {
                "0".into()
            },
            csv_opt_ts(&row.usage_limit_reset_time),
            csv_value(&row.created_at),
            csv_value(&row.updated_at),
        ];
        csv_lines.push(csv_line(&values));
    }

    let csv_data = csv_lines.join("\n");
    let tmp_path = write_temp_csv(&csv_data)?;
    let tmp_path_str = tmp_path.to_string_lossy().replace('\\', "/");
    const COLS: &str = "date, record_type, record_key, source, machine_name, \
        account_id, api_key_id, \
        model_name, session_id, project_path, input_tokens, output_tokens, \
        cache_creation_tokens, cache_read_tokens, reasoning_tokens, total_tokens, \
        cost, dedup_key, import_id, block_id, start_time, end_time, actual_end_time, \
        is_active, is_gap, entries, burn_rate, projection, usage_limit_reset_time, \
        created_at, updated_at";
    let sql = format!(
        "COPY ccusage_events ({cols}) FROM '{path}' (HEADER, DELIMITER ',', FORMAT csv, NULL '')",
        cols = COLS,
        path = tmp_path_str
    );
    let result = conn.execute(&sql, []);
    let _ = std::fs::remove_file(&tmp_path);
    result?;
    Ok(())
}

fn write_temp_csv(csv_data: &str) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join("summa");
    std::fs::create_dir_all(&dir)?;
    let name = format!("import-{}.csv", uuid::Uuid::new_v4());
    let path = dir.join(name);
    std::fs::write(&path, csv_data)?;
    Ok(path)
}

fn csv_opt_ts(opt: &Option<String>) -> String {
    match opt {
        Some(v) if !v.is_empty() => v.replace('T', " ").replace('Z', ""),
        _ => String::new(),
    }
}

#[async_trait]
impl DataSink for DuckDbSink {
    fn name(&self) -> &'static str {
        if self.is_motherduck {
            "motherduck"
        } else {
            "duckdb"
        }
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        let conn_str = self.connection_string();
        let is_md = self.is_motherduck;
        let local_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = if is_md {
                DuckDbSink::open_motherduck(&conn_str)?
            } else {
                if let Some(parent) = Path::new(&local_path).parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)?;
                    }
                }
                duckdb::Connection::open(&conn_str)?
            };
            // Touch connection so MotherDuck auth fails early if broken.
            conn.execute("SELECT 1", [])?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    async fn write(&mut self, data: EventsSnapshotData) -> anyhow::Result<SinkResult> {
        let start = std::time::Instant::now();
        let rows = data.events;
        let mut result = SinkResult {
            sink_name: self.name().to_string(),
            tables_written: Vec::new(),
            rows_written: HashMap::new(),
            duration_ms: 0,
            error: None,
        };

        if rows.is_empty() {
            result.duration_ms = start.elapsed().as_millis() as u64;
            return Ok(result);
        }

        let count = tokio::task::spawn_blocking({
            let db_path = self.db_path().to_string();
            move || {
                let mut sink = DuckDbSink::new(db_path);
                sink.write_events_sync(&rows)
            }
        })
        .await??;

        result.tables_written.push("ccusage_events".to_string());
        result
            .rows_written
            .insert("ccusage_events".to_string(), count as u64);
        result.duration_ms = start.elapsed().as_millis() as u64;
        Ok(result)
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl Default for DuckDbSink {
    fn default() -> Self {
        Self::new("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motherduck_connection_string_appends_token() {
        assert_eq!(
            append_motherduck_token("md:ccusage", "test-token-xyz"),
            "md:ccusage?motherduck_token=test-token-xyz"
        );
        assert_eq!(
            append_motherduck_token("md:ccusage?x=1", "tok"),
            "md:ccusage?x=1&motherduck_token=tok"
        );
        assert_eq!(append_motherduck_token("md:ccusage", ""), "md:ccusage");
    }

    #[test]
    fn antigravity_write_drops_prior_dates_for_machine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.duckdb");
        let mut sink = DuckDbSink::new(path.to_string_lossy().to_string());
        let stale = EventRow {
            date: "2026-08-01".into(),
            record_type: "daily".into(),
            record_key: "2026-08-01".into(),
            source: "antigravity".into(),
            machine_name: "host".into(),
            model_name: "est".into(),
            total_tokens: 14_303_983,
            cost: 3.42,
            ..EventRow::default()
        };
        sink.write_events_sync(&[stale]).unwrap();
        let fresh = EventRow {
            date: "2026-06-15".into(),
            record_type: "daily".into(),
            record_key: "2026-06-15".into(),
            source: "antigravity".into(),
            machine_name: "host".into(),
            model_name: "gemini-test".into(),
            total_tokens: 1250,
            ..EventRow::default()
        };
        sink.write_events_sync(&[fresh]).unwrap();
        let conn = duckdb::Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM ccusage_events WHERE source = 'antigravity'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let tokens: i64 = conn
            .query_row(
                "SELECT total_tokens FROM ccusage_events WHERE source = 'antigravity'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tokens, 1250);
    }

    #[test]
    fn local_connection_string_unchanged() {
        let sink = DuckDbSink::new("/tmp/ccusage-test.duckdb");
        assert_eq!(sink.connection_string(), "/tmp/ccusage-test.duckdb");
        assert_eq!(sink.name(), "duckdb");
    }

    #[test]
    fn motherduck_name() {
        let sink = DuckDbSink::new("md:ccusage");
        assert_eq!(sink.name(), "motherduck");
    }

    #[test]
    fn local_open_creates_parent_dirs_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested").join("data").join("summa.duckdb");
        assert!(!nested.exists());
        let sink = DuckDbSink::new(nested.to_string_lossy().to_string());
        let conn = sink.open_connection().expect("open local duckdb");
        drop(conn);
        assert!(
            nested.exists(),
            "DuckDB should create the local file at {}",
            nested.display()
        );
        assert!(nested.parent().unwrap().is_dir());
    }

    #[test]
    fn dedup_key_write_replaces_one_row_not_the_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.duckdb");
        let mut sink = DuckDbSink::new(path.to_string_lossy().to_string());
        let a = EventRow {
            date: "2026-08-20".into(),
            record_type: "daily".into(),
            record_key: "a".into(),
            source: "cursor".into(),
            machine_name: "account".into(),
            model_name: "grok".into(),
            cost: 1.0,
            total_tokens: 10,
            dedup_key: "key-a".into(),
            ..EventRow::default()
        };
        let b = EventRow {
            date: "2026-08-20".into(),
            record_type: "daily".into(),
            record_key: "b".into(),
            source: "cursor".into(),
            machine_name: "account".into(),
            model_name: "opus".into(),
            cost: 2.0,
            total_tokens: 20,
            dedup_key: "key-b".into(),
            ..EventRow::default()
        };
        sink.write_events_by_dedup_key(&[a.clone(), b]).unwrap();
        let mut a2 = a;
        a2.cost = 9.0;
        sink.write_events_by_dedup_key(&[a2]).unwrap();
        let conn = duckdb::Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM ccusage_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let cost: f64 = conn
            .query_row(
                "SELECT cost FROM ccusage_events WHERE dedup_key = 'key-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!((cost - 9.0).abs() < 1e-9);
    }
}
