use crate::config::ClickHouseConfig;
use crate::model::{DataSink, EventRow, EventsSnapshotData, SinkResult};
use crate::util::escape_sql_literal;
use async_trait::async_trait;
use reqwest::Client;
use std::collections::HashMap;

const LIVE_TABLE: &str = "ccusage_events";
const SWAP_TABLE: &str = "ccusage_events__swap";
const INSERT_CHUNK: usize = 1000;

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

type ScopeKey = (String, String, String, String);

fn collect_scopes(rows: &[EventRow]) -> Vec<ScopeKey> {
    let mut scopes: Vec<ScopeKey> = Vec::new();
    let mut seen: HashMap<ScopeKey, ()> = HashMap::new();
    for row in rows {
        let key = (
            row.date.clone(),
            row.record_type.clone(),
            row.source.clone(),
            row.machine_name.clone(),
        );
        if seen.insert(key.clone(), ()).is_none() {
            scopes.push(key);
        }
    }
    scopes
}

fn collect_antigravity_machines(rows: &[EventRow]) -> Vec<String> {
    let mut machines = Vec::new();
    for row in rows {
        if row.source == "antigravity" && !machines.iter().any(|m| m == &row.machine_name) {
            machines.push(row.machine_name.clone());
        }
    }
    machines
}

fn sql_string(value: &str) -> String {
    format!("'{}'", escape_sql_literal(value))
}

/// Rows kept from the live table: everything outside this snapshot's replace scopes.
/// Antigravity is a full machine snapshot (same as DuckDB).
fn keep_predicate(scopes: &[ScopeKey], antigravity_machines: &[String]) -> String {
    let mut parts = Vec::new();
    if !scopes.is_empty() {
        let tuples = scopes
            .iter()
            .map(|(date, record_type, source, machine_name)| {
                format!(
                    "({},{},{},{})",
                    sql_string(date),
                    sql_string(record_type),
                    sql_string(source),
                    sql_string(machine_name),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!(
            "(date, record_type, source, machine_name) NOT IN ({tuples})"
        ));
    }
    if !antigravity_machines.is_empty() {
        let list = antigravity_machines
            .iter()
            .map(|m| sql_string(m))
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!(
            "NOT (source = 'antigravity' AND machine_name IN ({list}))"
        ));
    }
    if parts.is_empty() {
        "1".to_string()
    } else {
        parts.join(" AND ")
    }
}

fn drop_swap_sql() -> String {
    format!("DROP TABLE IF EXISTS {SWAP_TABLE}")
}

fn create_swap_sql() -> String {
    format!("CREATE TABLE {SWAP_TABLE} AS {LIVE_TABLE}")
}

fn copy_kept_sql(scopes: &[ScopeKey], antigravity_machines: &[String]) -> String {
    format!(
        "INSERT INTO {SWAP_TABLE} SELECT * FROM {LIVE_TABLE} FINAL WHERE {}",
        keep_predicate(scopes, antigravity_machines)
    )
}

fn exchange_sql() -> String {
    format!("EXCHANGE TABLES {LIVE_TABLE} AND {SWAP_TABLE}")
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

    #[allow(dead_code)]
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

    pub async fn delete_by_dedup_keys(&self, keys: &[String]) -> anyhow::Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        const KEY_BATCH: usize = 200;
        for chunk in keys.chunks(KEY_BATCH) {
            let list = chunk
                .iter()
                .filter(|k| !k.is_empty())
                .map(|k| sql_string(k))
                .collect::<Vec<_>>();
            if list.is_empty() {
                continue;
            }
            let query = format!(
                "ALTER TABLE {LIVE_TABLE} DELETE WHERE dedup_key IN ({})",
                list.join(",")
            );
            self.run_query(&query).await?;
        }
        Ok(())
    }

    pub async fn insert_events(&self, rows: &[EventRow]) -> anyhow::Result<()> {
        for chunk in rows.chunks(INSERT_CHUNK) {
            self.insert_rows(LIVE_TABLE, chunk).await?;
        }
        Ok(())
    }

    async fn insert_rows(&self, table: &str, rows: &[EventRow]) -> anyhow::Result<()> {
        let client = self
            .client
            .as_ref()
            .expect("ClickHouseSink not connected");
        let url = format!(
            "{}/?query=INSERT+INTO+{table}+FORMAT+JSONEachRow",
            self.base_url()
        );

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

    pub async fn connect_with(&mut self, cfg: ClickHouseConfig) -> anyhow::Result<()> { // pragma: allowlist secret
        self.config = Some(cfg);
        self.client = Some(Client::new());
        self.run_query(click_house_create_sql()).await?;
        for stmt in click_house_alter_statements() {
            let _ = self.run_query(stmt).await;
        }
        Ok(())
    }

    /// Build replacement in `ccusage_events__swap`, then atomically swap.
    /// Live `ccusage_events` is unchanged until EXCHANGE TABLES succeeds.
    async fn write_via_swap(&self, rows: &[EventRow]) -> anyhow::Result<usize> {
        let scopes = collect_scopes(rows);
        let antigravity_machines = collect_antigravity_machines(rows);

        self.run_query(&drop_swap_sql()).await?;
        self.run_query(&create_swap_sql()).await?;
        self.run_query(&copy_kept_sql(&scopes, &antigravity_machines))
            .await?;

        let mut inserted = 0;
        for chunk in rows.chunks(INSERT_CHUNK) {
            self.insert_rows(SWAP_TABLE, chunk).await?;
            inserted += chunk.len();
        }

        self.run_query(&exchange_sql()).await?;
        Ok(inserted)
    }
}

#[async_trait]
impl DataSink for ClickHouseSink {
    fn name(&self) -> &'static str {
        "clickhouse"
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        self.connect_with(ClickHouseConfig::from_env()).await // pragma: allowlist secret
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

        let inserted = self.write_via_swap(&rows).await?;

        result.tables_written.push(LIVE_TABLE.to_string());
        result
            .rows_written
            .insert(LIVE_TABLE.to_string(), inserted as u64);
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
    use crate::model::EventsSnapshotData;
    use axum::body::Bytes;
    use axum::extract::{Query, RawQuery, State};
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::Router;
    use std::collections::HashMap as StdHashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn sample_row(date: &str, source: &str, machine: &str, model: &str, cost: f64) -> EventRow {
        EventRow {
            date: date.into(),
            record_type: "daily".into(),
            record_key: date.into(),
            source: source.into(),
            machine_name: machine.into(),
            model_name: model.into(),
            cost,
            total_tokens: (cost * 1000.0) as u64,
            dedup_key: format!("{date}-{source}-{machine}-{model}"),
            ..EventRow::default()
        }
    }

    #[test]
    fn write_plan_never_deletes_live_rows() {
        let rows = vec![sample_row("2026-09-01", "ccusage", "host", "sonnet", 1.0)];
        let scopes = collect_scopes(&rows);
        let ag = collect_antigravity_machines(&rows);
        let stmts = [
            drop_swap_sql(),
            create_swap_sql(),
            copy_kept_sql(&scopes, &ag),
            exchange_sql(),
        ];
        for sql in &stmts {
            let lower = sql.to_ascii_lowercase();
            assert!(
                !lower.contains("alter table ccusage_events delete"),
                "live delete leaked into plan: {sql}"
            );
            assert!(
                !lower.contains(&format!("delete from {LIVE_TABLE}")),
                "live delete leaked into plan: {sql}"
            );
        }
        assert_eq!(stmts[3], "EXCHANGE TABLES ccusage_events AND ccusage_events__swap");
        assert!(copy_kept_sql(&scopes, &ag).contains("INSERT INTO ccusage_events__swap"));
        assert!(copy_kept_sql(&scopes, &ag).contains("FROM ccusage_events FINAL"));
    }

    #[test]
    fn keep_predicate_excludes_scopes_and_escapes_quotes() {
        let rows = vec![sample_row(
            "2026-09-01",
            "ccusage",
            "host'1",
            "sonnet",
            1.0,
        )];
        let pred = keep_predicate(&collect_scopes(&rows), &[]);
        assert!(pred.contains("NOT IN"));
        assert!(pred.contains("'host''1'"));
        assert!(!pred.contains("antigravity"));
    }

    #[test]
    fn keep_predicate_drops_all_antigravity_for_machine() {
        let rows = vec![sample_row(
            "2026-09-01",
            "antigravity",
            "laptop",
            "gemini",
            0.5,
        )];
        let pred = keep_predicate(
            &collect_scopes(&rows),
            &collect_antigravity_machines(&rows),
        );
        assert!(pred.contains("NOT (source = 'antigravity' AND machine_name IN ('laptop'))"));
    }

    fn apply_snapshot(live: &[EventRow], incoming: &[EventRow]) -> Vec<EventRow> {
        let scopes = collect_scopes(incoming);
        let ag = collect_antigravity_machines(incoming);
        let mut out: Vec<EventRow> = live
            .iter()
            .filter(|row| !row_replaced_by_snapshot(row, &scopes, &ag))
            .cloned()
            .collect();
        out.extend(incoming.iter().cloned());
        out
    }

    fn row_replaced_by_snapshot(
        row: &EventRow,
        scopes: &[ScopeKey],
        antigravity_machines: &[String],
    ) -> bool {
        if row.source == "antigravity"
            && antigravity_machines.iter().any(|m| m == &row.machine_name)
        {
            return true;
        }
        scopes.iter().any(|(date, record_type, source, machine_name)| {
            date == &row.date
                && record_type == &row.record_type
                && source == &row.source
                && machine_name == &row.machine_name
        })
    }

    fn cost_of(rows: &[EventRow], date: &str, model: &str) -> f64 {
        rows.iter()
            .filter(|r| r.date == date && r.model_name == model)
            .map(|r| r.cost)
            .sum()
    }

    #[test]
    fn swap_commit_replaces_scope_and_keeps_other_days() {
        let prior = vec![
            sample_row("2026-09-01", "ccusage", "host", "sonnet", 1.25),
            sample_row("2026-09-02", "ccusage", "host", "opus", 2.50),
        ];
        let incoming = vec![sample_row(
            "2026-09-01",
            "ccusage",
            "host",
            "sonnet",
            9.00,
        )];
        let committed = apply_snapshot(&prior, &incoming);
        assert!((cost_of(&committed, "2026-09-01", "sonnet") - 9.00).abs() < 1e-9);
        assert!((cost_of(&committed, "2026-09-02", "opus") - 2.50).abs() < 1e-9);
        assert_eq!(committed.len(), 2);
    }

    #[test]
    fn swap_commit_drops_stale_model_in_replaced_scope() {
        let prior = vec![sample_row(
            "2026-09-01",
            "ccusage",
            "host",
            "sonnet",
            1.25,
        )];
        let incoming = vec![sample_row("2026-09-01", "ccusage", "host", "opus", 3.00)];
        let committed = apply_snapshot(&prior, &incoming);
        assert_eq!(cost_of(&committed, "2026-09-01", "sonnet"), 0.0);
        assert!((cost_of(&committed, "2026-09-01", "opus") - 3.00).abs() < 1e-9);
        assert_eq!(committed.len(), 1);
    }

    /// Live table is only replaced at EXCHANGE. Any earlier failure keeps prior rows.
    fn simulate_write(
        live: Vec<EventRow>,
        incoming: &[EventRow],
        fail_before_exchange: bool,
        fail_after_inserts: usize,
    ) -> (Vec<EventRow>, bool) {
        let scopes = collect_scopes(incoming);
        let ag = collect_antigravity_machines(incoming);
        let mut swap: Vec<EventRow> = live
            .iter()
            .filter(|row| !row_replaced_by_snapshot(row, &scopes, &ag))
            .cloned()
            .collect();
        for (i, row) in incoming.iter().enumerate() {
            if i >= fail_after_inserts {
                return (live, false);
            }
            swap.push(row.clone());
        }
        if fail_before_exchange {
            return (live, false);
        }
        (swap, true)
    }

    #[test]
    fn crash_before_exchange_keeps_prior_live_rows() {
        let prior = vec![
            sample_row("2026-09-01", "ccusage", "host", "sonnet", 1.25),
            sample_row("2026-09-02", "cursor", "account", "grok", 4.00),
        ];
        let incoming = vec![sample_row(
            "2026-09-01",
            "ccusage",
            "host",
            "sonnet",
            99.00,
        )];
        let (live, ok) = simulate_write(prior.clone(), &incoming, true, usize::MAX);
        assert!(!ok);
        assert_eq!(live, prior);
        assert!((cost_of(&live, "2026-09-01", "sonnet") - 1.25).abs() < 1e-9);
        assert!((cost_of(&live, "2026-09-02", "grok") - 4.00).abs() < 1e-9);
    }

    #[test]
    fn crash_mid_insert_into_swap_keeps_prior_live_rows() {
        let prior = vec![sample_row(
            "2026-09-01",
            "ccusage",
            "host",
            "sonnet",
            1.25,
        )];
        let incoming = vec![
            sample_row("2026-09-01", "ccusage", "host", "sonnet", 2.00),
            sample_row("2026-09-01", "ccusage", "host", "opus", 3.00),
        ];
        let (live, ok) = simulate_write(prior.clone(), &incoming, false, 1);
        assert!(!ok);
        assert_eq!(live, prior);
        assert!((cost_of(&live, "2026-09-01", "sonnet") - 1.25).abs() < 1e-9);
        assert_eq!(cost_of(&live, "2026-09-01", "opus"), 0.0);
    }

    #[derive(Clone)]
    struct FakeCh {
        tables: Arc<Mutex<StdHashMap<String, Vec<EventRow>>>>,
        queries: Arc<Mutex<Vec<String>>>,
        /// Fail the Nth JSONEachRow insert (1-based). 0 = never fail.
        fail_on_insert: Arc<AtomicUsize>,
        insert_count: Arc<AtomicUsize>,
    }

    impl FakeCh {
        fn new() -> Self {
            Self {
                tables: Arc::new(Mutex::new(StdHashMap::new())),
                queries: Arc::new(Mutex::new(Vec::new())),
                fail_on_insert: Arc::new(AtomicUsize::new(0)),
                insert_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn live_rows(&self) -> Vec<EventRow> {
            self.tables
                .lock()
                .unwrap()
                .get(LIVE_TABLE)
                .cloned()
                .unwrap_or_default()
        }

        fn recorded(&self) -> Vec<String> {
            self.queries.lock().unwrap().clone()
        }
    }

    fn table_token<'a>(sql: &'a str, after: &str) -> Option<&'a str> {
        let idx = sql.find(after)?;
        let rest = sql[idx + after.len()..].trim_start();
        rest.split(|c: char| c.is_whitespace() || c == '(' || c == ';')
            .next()
            .filter(|s| !s.is_empty())
    }

    async fn fake_ch_handler(
        State(state): State<FakeCh>,
        RawQuery(raw): RawQuery,
        Query(params): Query<StdHashMap<String, String>>,
        body: Bytes,
    ) -> StatusCode {
        let body_s = String::from_utf8_lossy(&body).into_owned();
        let sql = params
            .get("query")
            .cloned()
            .or_else(|| {
                raw.as_deref().and_then(|q| {
                    q.strip_prefix("query=")
                        .map(|v| v.replace('+', " "))
                })
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| body_s.clone());
        state.queries.lock().unwrap().push(sql.clone());

        let trimmed = sql.trim();
        let upper = trimmed.to_ascii_uppercase();

        if upper.starts_with("SELECT 1") {
            return StatusCode::OK;
        }
        if upper.starts_with("ALTER TABLE") {
            return StatusCode::OK;
        }
        if upper.starts_with("DROP TABLE") {
            if let Some(name) = table_token(trimmed, "EXISTS ").or_else(|| {
                table_token(trimmed, "TABLE ")
            }) {
                state.tables.lock().unwrap().remove(name);
            }
            return StatusCode::OK;
        }
        if upper.contains("CREATE TABLE") && upper.contains(" AS ") {
            if let Some(name) = table_token(trimmed, "TABLE ") {
                state
                    .tables
                    .lock()
                    .unwrap()
                    .entry(name.to_string())
                    .or_default();
            }
            return StatusCode::OK;
        }
        if upper.starts_with("CREATE TABLE") {
            if let Some(name) = table_token(trimmed, "EXISTS ")
                .or_else(|| table_token(trimmed, "TABLE "))
            {
                state
                    .tables
                    .lock()
                    .unwrap()
                    .entry(name.to_string())
                    .or_default();
            }
            return StatusCode::OK;
        }
        if upper.starts_with("EXCHANGE TABLES") {
            let mut tables = state.tables.lock().unwrap();
            let a = tables.remove(LIVE_TABLE).unwrap_or_default();
            let b = tables.remove(SWAP_TABLE).unwrap_or_default();
            tables.insert(LIVE_TABLE.to_string(), b);
            tables.insert(SWAP_TABLE.to_string(), a);
            return StatusCode::OK;
        }
        if upper.contains("FORMAT JSONEACHROW") {
            let n = state.insert_count.fetch_add(1, Ordering::SeqCst) + 1;
            let fail = state.fail_on_insert.load(Ordering::SeqCst);
            if fail > 0 && n == fail {
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
            let table = table_token(trimmed, "INTO ").unwrap_or(SWAP_TABLE);
            let mut rows = Vec::new();
            for line in body_s.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(row) = serde_json::from_str::<EventRow>(line) {
                    rows.push(row);
                }
            }
            state
                .tables
                .lock()
                .unwrap()
                .entry(table.to_string())
                .or_default()
                .extend(rows);
            return StatusCode::OK;
        }
        if upper.starts_with("INSERT INTO") && upper.contains(" SELECT ") {
            let dest = table_token(trimmed, "INTO ").unwrap_or(SWAP_TABLE);
            let src = table_token(trimmed, "FROM ").unwrap_or(LIVE_TABLE);
            let tables = state.tables.lock().unwrap();
            let src_rows = tables.get(src).cloned().unwrap_or_default();
            drop(tables);
            // Fake CH does not parse WHERE; production still sends the keep predicate.
            // Crash-safety tests only need live to stay put until EXCHANGE.
            state
                .tables
                .lock()
                .unwrap()
                .entry(dest.to_string())
                .or_default()
                .extend(src_rows);
            return StatusCode::OK;
        }
        StatusCode::OK
    }

    async fn spawn_fake_ch(state: FakeCh) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/", post(fake_ch_handler))
            .with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (port, handle)
    }

    fn test_cfg(port: u16) -> ClickHouseConfig { // pragma: allowlist secret
        ClickHouseConfig { // pragma: allowlist secret
            host: "127.0.0.1".into(),
            port,
            user: "default".into(),
            password: String::new(),
            database: String::new(),
            protocol: "http".into(),
        }
    }

    #[tokio::test]
    async fn http_write_exchanges_and_does_not_delete_live() {
        let fake = FakeCh::new();
        let (port, server) = spawn_fake_ch(fake.clone()).await;
        let mut sink = ClickHouseSink::new(); // pragma: allowlist secret
        sink.connect_with(test_cfg(port)).await.unwrap();
        sink.connect_with(test_cfg(port)).await.unwrap();
        let row = sample_row("2026-09-01", "ccusage", "host", "sonnet", 1.25);
        sink.write(EventsSnapshotData {
            events: vec![row.clone()],
        })
        .await
        .unwrap();

        let live = fake.live_rows();
        assert_eq!(live.len(), 1);
        assert!((live[0].cost - 1.25).abs() < 1e-9);
        let log = fake.recorded().join("\n").to_ascii_lowercase();
        assert!(log.contains("exchange tables ccusage_events and ccusage_events__swap"));
        assert!(!log.contains("alter table ccusage_events delete"));
        server.abort();
    }

    #[tokio::test]
    async fn http_mid_write_insert_failure_preserves_prior_live_rows() {
        let fake = FakeCh::new();
        let (port, server) = spawn_fake_ch(fake.clone()).await;
        let mut sink = ClickHouseSink::new(); // pragma: allowlist secret
        sink.connect_with(test_cfg(port)).await.unwrap();

        let prior = sample_row("2026-09-01", "ccusage", "host", "sonnet", 1.25);
        sink.write(EventsSnapshotData {
            events: vec![prior.clone()],
        })
        .await
        .unwrap();
        assert_eq!(fake.live_rows().len(), 1);
        assert!((fake.live_rows()[0].cost - 1.25).abs() < 1e-9);

        fake.fail_on_insert.store(1, Ordering::SeqCst);
        fake.insert_count.store(0, Ordering::SeqCst);
        let replacement = sample_row("2026-09-01", "ccusage", "host", "sonnet", 9.99);
        let err = sink
            .write(EventsSnapshotData {
                events: vec![replacement],
            })
            .await;
        assert!(err.is_err(), "second write should fail mid-insert");

        let live = fake.live_rows();
        assert_eq!(live.len(), 1, "live must still hold the first successful write");
        assert!((live[0].cost - 1.25).abs() < 1e-9);
        let exchanges = fake
            .recorded()
            .iter()
            .filter(|q| q.to_ascii_uppercase().contains("EXCHANGE TABLES"))
            .count();
        assert_eq!(exchanges, 1, "failed rewrite must not EXCHANGE live");
        server.abort();
    }
}
