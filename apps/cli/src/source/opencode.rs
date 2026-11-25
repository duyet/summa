/**
 * OpenCode Source
 *
 * Reads OpenCode's SQLite state from `$OPENCODE_DATA_DIR` or `$XDG_DATA_HOME/opencode`
 * (default `~/.local/share/opencode`). Two storage generations coexist and are
 * both read, because OpenCode migrates old history into the new table and the
 * two then overlap:
 * - legacy `message` table: `data` JSON with `role`, `modelID`, `path.cwd`, `tokens`, `cost`.
 * - current `session_message` table (`type = 'assistant'`), joined to `session_v2`
 *   for the working directory: `data` JSON with `model.id`, `tokens`, `cost`.
 * The same message id is counted once. Databases are read from private copies
 * (WAL included) so a running OpenCode is never disturbed. `opencode-tui-v2.db`
 * is read too when present, with whichever of those tables it has.
 *
 * Token mapping (OpenCode input is already non-cached):
 *   input = tokens.input, cache_read = tokens.cache.read,
 *   cache_creation = tokens.cache.write, output = tokens.output,
 *   reasoning = tokens.reasoning (reported, not added to the total)
 *   total = input + output + cache_read + cache_write
 *
 * Cost: the reported `cost` when sane, else public rates via `util::pricing`.
 */

use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::model::{DataSource, EventRow, EventsSnapshotData, SourceResult};
use crate::util::date::ch_now;
use crate::util::hash::{hash_project_name_sync, make_dedup_key};
use crate::util::pricing::resolve_reported_cost;
use crate::util::sqlite::SqliteSnapshot;
use crate::util::tokens::total_tokens;

const DB_FILES: [&str; 2] = ["opencode.db", "opencode-tui-v2.db"];

#[derive(Debug, Clone)]
pub struct OpenCodeSourceOptions {
    pub machine_name: String,
    pub hash_projects: bool,
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Override the OpenCode data dir (tests). When None, resolves
    /// `OPENCODE_DATA_DIR`, then `XDG_DATA_HOME/opencode`, then `~/.local/share/opencode`.
    pub base_dir: Option<PathBuf>,
    /// Override where the per-run DB copies go (tests).
    pub temp_dir: Option<PathBuf>,
}

#[derive(Debug)]
struct Message {
    id: String,
    session_id: String,
    model: String,
    cwd: String,
    created_ms: i64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    cost: f64,
}

#[derive(Default)]
struct Agg {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    cost: f64,
    entries: u32,
    cwd: String,
    first_ms: i64,
    last_ms: i64,
}

impl Agg {
    fn add(&mut self, m: &Message, cost: f64) {
        self.input += m.input;
        self.output += m.output;
        self.cache_read += m.cache_read;
        self.cache_write += m.cache_write;
        self.reasoning += m.reasoning;
        self.cost += cost;
        if self.entries == 0 || m.created_ms < self.first_ms {
            self.first_ms = m.created_ms;
        }
        self.last_ms = self.last_ms.max(m.created_ms);
        self.entries += 1;
        if self.cwd.is_empty() {
            self.cwd = m.cwd.clone();
        }
    }

    fn total(&self) -> u64 {
        total_tokens(self.input, self.output, self.cache_write, self.cache_read)
    }
}

pub struct OpenCodeSource {
    opts: OpenCodeSourceOptions,
}

impl OpenCodeSource {
    pub fn new(opts: OpenCodeSourceOptions) -> Self {
        Self { opts }
    }
}

#[async_trait]
impl DataSource for OpenCodeSource {
    fn name(&self) -> &'static str {
        "opencode"
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let events = fetch_opencode_events(&self.opts)?;
        if self.opts.verbose {
            eprintln!("OpenCode Source parsed {} rows.", events.len());
        }
        Ok(SourceResult {
            source_name: self.name().to_string(),
            data: EventsSnapshotData { events },
            fetched_at: chrono::Utc::now().to_rfc3339(),
            error: None,
        })
    }
}

/// Pure entry used by fetch and unit tests.
pub fn fetch_opencode_events(opts: &OpenCodeSourceOptions) -> anyhow::Result<Vec<EventRow>> {
    let effective_since = if let Some(s) = &opts.since {
        Some(s.clone())
    } else {
        opts.days_back.filter(|d| *d > 0).map(|days| {
            (chrono::Utc::now() - chrono::Duration::days(days))
                .format("%Y-%m-%d")
                .to_string()
        })
    };

    let data_dir = resolve_data_dir(opts);
    let mut seen: HashSet<String> = HashSet::new();
    let mut messages: Vec<Message> = Vec::new();
    for file in DB_FILES {
        let db_path = data_dir.join(file);
        if !db_path.exists() {
            if opts.verbose {
                eprintln!("OpenCode database not found: {}", db_path.display());
            }
            continue;
        }
        let snapshot = SqliteSnapshot::open(&db_path, opts.temp_dir.as_deref())?;
        // Current storage first: it is the live one, the legacy table only holds
        // the same history from before the migration.
        for m in read_messages(&snapshot.conn)? {
            if seen.insert(m.id.clone()) {
                messages.push(m);
            }
        }
    }

    let mut sessions: BTreeMap<(String, String, String), Agg> = BTreeMap::new();
    let mut daily: BTreeMap<(String, String), Agg> = BTreeMap::new();
    for m in &messages {
        if total_tokens(m.input, m.output, m.cache_write, m.cache_read) == 0 {
            continue;
        }
        let Some(date) = chrono::DateTime::from_timestamp_millis(m.created_ms)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
        else {
            continue;
        };
        if effective_since.as_ref().is_some_and(|s| &date < s)
            || opts.end_date.as_ref().is_some_and(|e| &date > e)
        {
            continue;
        }
        let cost = resolve_reported_cost(
            &m.model,
            m.cost,
            m.input,
            m.cache_read,
            m.cache_write,
            m.output,
        );
        sessions
            .entry((m.session_id.clone(), date.clone(), m.model.clone()))
            .or_default()
            .add(m, cost);
        daily.entry((date, m.model.clone())).or_default().add(m, cost);
    }

    let now = ch_now();
    let mut events = Vec::with_capacity(sessions.len() + daily.len());

    for ((session_id, date, model), agg) in &sessions {
        let hashed_session_id = hash_project_name_sync(session_id, opts.hash_projects);
        let project = hash_project_name_sync(
            if agg.cwd.is_empty() { session_id } else { &agg.cwd },
            opts.hash_projects,
        );
        let raw_key = format!(
            "opencode|{}|session|{}|{}|{}",
            opts.machine_name, date, model, hashed_session_id
        );
        events.push(event_row(
            opts,
            &now,
            "session",
            date,
            &hashed_session_id,
            model,
            &hashed_session_id,
            project,
            agg,
            &raw_key,
            true,
        ));
    }

    for ((date, model), agg) in &daily {
        let project = hash_project_name_sync(
            if agg.cwd.is_empty() { "unknown" } else { &agg.cwd },
            opts.hash_projects,
        );
        let raw_key = format!(
            "opencode|{}|daily|{}|{}|{}",
            opts.machine_name, date, model, date
        );
        events.push(event_row(
            opts, &now, "daily", date, date, model, "", project, agg, &raw_key, false,
        ));
    }

    Ok(events)
}

#[allow(clippy::too_many_arguments)]
fn event_row(
    opts: &OpenCodeSourceOptions,
    now: &str,
    record_type: &str,
    date: &str,
    record_key: &str,
    model: &str,
    session_id: &str,
    project_path: String,
    agg: &Agg,
    raw_key: &str,
    with_times: bool,
) -> EventRow {
    let time = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
    };
    EventRow {
        date: date.to_string(),
        record_type: record_type.to_string(),
        record_key: record_key.to_string(),
        source: "opencode".to_string(),
        machine_name: opts.machine_name.clone(),
        account_id: String::new(),
        api_key_id: String::new(),
        model_name: model.to_string(),
        session_id: session_id.to_string(),
        project_path,
        input_tokens: agg.input,
        output_tokens: agg.output,
        cache_creation_tokens: agg.cache_write,
        cache_read_tokens: agg.cache_read,
        reasoning_tokens: agg.reasoning,
        total_tokens: agg.total(),
        cost: (agg.cost * 100.0).round() / 100.0,
        dedup_key: make_dedup_key(raw_key),
        import_id: opts.import_id.clone(),
        start_time: if with_times { time(agg.first_ms) } else { None },
        end_time: if with_times { time(agg.last_ms) } else { None },
        actual_end_time: None,
        is_active: 0,
        is_gap: 0,
        entries: agg.entries,
        burn_rate: 0.0,
        projection: 0.0,
        usage_limit_reset_time: None,
        block_id: String::new(),
        created_at: now.to_string(),
        updated_at: now.to_string(),
    }
}

/// `OPENCODE_DATA_DIR` → `XDG_DATA_HOME/opencode` → `~/.local/share/opencode`.
fn resolve_data_dir(opts: &OpenCodeSourceOptions) -> PathBuf {
    if let Some(d) = &opts.base_dir {
        return d.clone();
    }
    if let Ok(d) = env::var("OPENCODE_DATA_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(x) = env::var("XDG_DATA_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("opencode");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("share")
        .join("opencode")
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Assistant messages from the current table, then the legacy one.
fn read_messages(conn: &Connection) -> anyhow::Result<Vec<Message>> {
    let mut out = Vec::new();
    if has_table(conn, "session_message") {
        let sql = if has_table(conn, "session_v2") {
            "SELECT m.id, m.session_id, m.time_created, m.data, s.directory
             FROM session_message m LEFT JOIN session_v2 s ON s.id = m.session_id
             WHERE m.type = 'assistant'"
        } else {
            "SELECT m.id, m.session_id, m.time_created, m.data, NULL
             FROM session_message m WHERE m.type = 'assistant'"
        };
        collect(conn, sql, &mut out)?;
    }
    if has_table(conn, "message") {
        collect(
            conn,
            "SELECT id, session_id, time_created, data, NULL FROM message",
            &mut out,
        )?;
    }
    Ok(out)
}

fn collect(conn: &Connection, sql: &str, out: &mut Vec<Message>) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1).unwrap_or_default(),
            row.get::<_, Option<i64>>(2).unwrap_or(None),
            row.get::<_, String>(3).unwrap_or_default(),
            row.get::<_, Option<String>>(4).unwrap_or(None),
        ))
    })?;
    for row in rows.flatten() {
        let (id, session_id, column_ms, data, directory) = row;
        if let Some(m) = parse_message(id, session_id, column_ms, &data, directory) {
            out.push(m);
        }
    }
    Ok(())
}

fn parse_message(
    id: String,
    session_id: String,
    column_ms: Option<i64>,
    data: &str,
    directory: Option<String>,
) -> Option<Message> {
    let v: Value = serde_json::from_str(data).ok()?;
    // Legacy rows carry a role; session_message rows are already filtered by type.
    if let Some(role) = v.get("role").and_then(Value::as_str) {
        if role != "assistant" {
            return None;
        }
    }
    let tokens = v.get("tokens")?;
    let model = v
        .pointer("/model/id")
        .or_else(|| v.get("modelID"))
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let cwd = directory
        .filter(|d| !d.is_empty())
        .or_else(|| v.pointer("/path/cwd").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    let created_ms = v
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .or(column_ms)?;
    Some(Message {
        id,
        session_id,
        model,
        cwd,
        created_ms,
        input: count(tokens.get("input")),
        output: count(tokens.get("output")),
        cache_read: count(tokens.pointer("/cache/read")),
        cache_write: count(tokens.pointer("/cache/write")),
        reasoning: count(tokens.get("reasoning")),
        cost: v.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

fn count(v: Option<&Value>) -> u64 {
    v.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;

    // 2026-10-08T12:00:00Z and 2026-10-20T09:30:00Z.
    const OCT_8: i64 = 1791460800000;
    const OCT_20: i64 = 1792488600000;

    fn opts(dir: &Path) -> OpenCodeSourceOptions {
        OpenCodeSourceOptions {
            machine_name: "m1".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "imp".into(),
            base_dir: Some(dir.to_path_buf()),
            temp_dir: None,
        }
    }

    fn db(dir: &Path) -> Connection {
        let conn = Connection::open(dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE session_v2 (id TEXT, directory TEXT, version TEXT);
             CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        conn
    }

    fn legacy(conn: &Connection, id: &str, session: &str, role: &str, ms: i64, input: u64) {
        let data = json!({
            "role": role, "modelID": "big-model", "providerID": "opencode",
            "path": {"cwd": "/work/legacy"}, "cost": 0,
            "tokens": {"total": 0, "input": input, "output": 10, "reasoning": 4,
                       "cache": {"write": 2, "read": 100}},
            "time": {"created": ms}
        });
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![id, session, ms, ms, data.to_string()],
        )
        .unwrap();
    }

    fn current(conn: &Connection, id: &str, session: &str, kind: &str, ms: i64, input: u64) {
        let data = json!({
            "time": {"created": ms},
            "model": {"id": "big-model", "providerID": "opencode", "variant": "high"},
            "cost": 0,
            "tokens": {"input": input, "output": 10, "reasoning": 4,
                       "cache": {"read": 100, "write": 2}}
        });
        conn.execute(
            "INSERT INTO session_message VALUES (?, ?, ?, 1, ?, ?, ?)",
            rusqlite::params![id, session, kind, ms, ms, data.to_string()],
        )
        .unwrap();
    }

    fn daily(events: &[EventRow]) -> Vec<&EventRow> {
        events.iter().filter(|e| e.record_type == "daily").collect()
    }

    #[test]
    fn legacy_only_maps_tokens_and_skips_user_rows() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        legacy(&conn, "msg_1", "ses_a", "assistant", OCT_8, 50);
        legacy(&conn, "msg_2", "ses_a", "user", OCT_8, 999);
        drop(conn);

        let events = fetch_opencode_events(&opts(tmp.path())).unwrap();
        let d = daily(&events);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].source, "opencode");
        assert_eq!(d[0].date, "2026-10-08");
        assert_eq!(d[0].model_name, "big-model");
        assert_eq!(d[0].input_tokens, 50);
        assert_eq!(d[0].output_tokens, 10);
        assert_eq!(d[0].cache_read_tokens, 100);
        assert_eq!(d[0].cache_creation_tokens, 2);
        assert_eq!(d[0].reasoning_tokens, 4);
        // Reasoning is reported, not added: 50 + 10 + 100 + 2.
        assert_eq!(d[0].total_tokens, 162);
        assert_eq!(d[0].project_path, "/work/legacy");
        assert_eq!(d[0].entries, 1);
    }

    #[test]
    fn current_only_uses_assistant_rows_and_session_directory() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        conn.execute("INSERT INTO session_v2 VALUES ('ses_b', '/work/current', '1.19.6')", [])
            .unwrap();
        current(&conn, "msg_3", "ses_b", "assistant", OCT_20, 70);
        current(&conn, "msg_4", "ses_b", "user", OCT_20, 999);
        drop(conn);

        let events = fetch_opencode_events(&opts(tmp.path())).unwrap();
        let d = daily(&events);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].date, "2026-10-20");
        assert_eq!(d[0].input_tokens, 70);
        assert_eq!(d[0].project_path, "/work/current");
        let s: Vec<_> = events.iter().filter(|e| e.record_type == "session").collect();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].start_time.as_deref(), Some("2026-10-20 09:30:00"));
    }

    #[test]
    fn migrated_history_in_both_tables_counts_once() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        conn.execute("INSERT INTO session_v2 VALUES ('ses_a', '/work/current', '1')", [])
            .unwrap();
        legacy(&conn, "msg_1", "ses_a", "assistant", OCT_8, 50);
        legacy(&conn, "msg_2", "ses_a", "assistant", OCT_8, 60);
        // msg_1 was migrated into the new table; msg_5 exists only there.
        current(&conn, "msg_1", "ses_a", "assistant", OCT_8, 50);
        current(&conn, "msg_5", "ses_a", "assistant", OCT_20, 70);
        drop(conn);

        let events = fetch_opencode_events(&opts(tmp.path())).unwrap();
        let total: u64 = daily(&events).iter().map(|e| e.input_tokens).sum();
        assert_eq!(total, 50 + 60 + 70);
        let entries: u32 = daily(&events).iter().map(|e| e.entries).sum();
        assert_eq!(entries, 3);
    }

    #[test]
    fn days_back_and_end_date_window_messages() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        let now = chrono::Utc::now().timestamp_millis();
        let old = now - 30 * 86_400_000;
        legacy(&conn, "msg_old", "ses_a", "assistant", old, 50);
        current(&conn, "msg_new", "ses_a", "assistant", now, 70);
        drop(conn);

        let mut o = opts(tmp.path());
        o.days_back = Some(7);
        let events = fetch_opencode_events(&o).unwrap();
        let d = daily(&events);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].input_tokens, 70);

        let mut o = opts(tmp.path());
        o.end_date = Some(
            (chrono::Utc::now() - chrono::Duration::days(10))
                .format("%Y-%m-%d")
                .to_string(),
        );
        let events = fetch_opencode_events(&o).unwrap();
        let d = daily(&events);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].input_tokens, 50);
    }

    #[test]
    fn each_assistant_message_counts_exactly_once() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        // Same message in both tables: 50 + 10 + 100 + 2 tokens, not double.
        legacy(&conn, "msg_1", "ses_a", "assistant", OCT_8, 50);
        current(&conn, "msg_1", "ses_a", "assistant", OCT_8, 50);
        drop(conn);

        let events = fetch_opencode_events(&opts(tmp.path())).unwrap();
        assert_eq!(daily(&events)[0].total_tokens, 162);
        assert_eq!(daily(&events)[0].entries, 1);
        let s: Vec<_> = events.iter().filter(|e| e.record_type == "session").collect();
        assert_eq!(s[0].total_tokens, 162);
    }

    /// Rows the ccusage companion wrote must be overwritten, not shadowed:
    /// same key formula as parser::rows::breakdown_row.
    #[test]
    fn daily_dedup_key_matches_companion_formula() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        legacy(&conn, "msg_1", "ses_a", "assistant", OCT_8, 50);
        drop(conn);

        let events = fetch_opencode_events(&opts(tmp.path())).unwrap();
        assert_eq!(
            daily(&events)[0].dedup_key,
            make_dedup_key("opencode|m1|daily|2026-10-08|big-model|2026-10-08")
        );
    }

    #[test]
    fn missing_data_dir_returns_no_rows() {
        let tmp = TempDir::new().unwrap();
        let events = fetch_opencode_events(&opts(&tmp.path().join("nope"))).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn free_model_with_zero_reported_cost_stays_free() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        let data = json!({"time": {"created": OCT_20}, "model": {"id": "hy4-flash-free"}, "cost": 0,
                          "tokens": {"input": 721, "output": 290, "reasoning": 0,
                                     "cache": {"read": 130304, "write": 0}}});
        conn.execute(
            "INSERT INTO session_message VALUES ('msg_f', 'ses_f', 'assistant', 1, ?, ?, ?)",
            rusqlite::params![OCT_20, OCT_20, data.to_string()],
        )
        .unwrap();
        drop(conn);
        let events = fetch_opencode_events(&opts(tmp.path())).unwrap();
        assert_eq!(daily(&events)[0].cost, 0.0);
        assert_eq!(daily(&events)[0].total_tokens, 721 + 290 + 130304);
    }

    #[test]
    fn zero_reported_cost_falls_back_to_public_rates() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        let data = json!({"time": {"created": OCT_20}, "model": {"id": "claude-sonnet-4.6"}, "cost": 0,
                          "tokens": {"input": 2000000, "output": 100000, "reasoning": 0,
                                     "cache": {"read": 0, "write": 0}}});
        conn.execute(
            "INSERT INTO session_message VALUES ('msg_c', 'ses_c', 'assistant', 1, ?, ?, ?)",
            rusqlite::params![OCT_20, OCT_20, data.to_string()],
        )
        .unwrap();
        drop(conn);
        let events = fetch_opencode_events(&opts(tmp.path())).unwrap();
        assert!(daily(&events)[0].cost > 1.0, "got {}", daily(&events)[0].cost);
    }

    #[test]
    fn hashing_applies_to_session_and_project() {
        let tmp = TempDir::new().unwrap();
        let conn = db(tmp.path());
        legacy(&conn, "msg_1", "ses_a", "assistant", OCT_8, 50);
        drop(conn);
        let mut o = opts(tmp.path());
        o.hash_projects = true;
        let events = fetch_opencode_events(&o).unwrap();
        let s = events.iter().find(|e| e.record_type == "session").unwrap();
        assert_eq!(s.session_id.len(), 8);
        assert_eq!(s.project_path.len(), 8);
        assert_eq!(s.dedup_key.len(), 16);
    }
}
