/**
 * Devin Source
 *
 * Reads Devin CLI (Cognition) local state from `$DEVIN_HOME/cli` or
 * `$XDG_DATA_HOME/devin/cli` (default `~/.local/share/devin/cli`):
 * - Token usage: ATIF transcripts. `agent_logs/devin-<sid>.json` is the
 *   current `--export` default; `transcripts/<sid>.json` is the older one.
 *   In ATIF-v1.7 only `final_metrics` carries counts — steps hold none.
 * - Session metadata: `sessions.db` (`sessions` table) for working directory,
 *   model and activity timestamps. Optional: a transcript still imports
 *   without it (project path falls back to the session id).
 *
 * Token mapping (Devin prompt tokens are cache-inclusive, like Codex/Grok):
 *   input      = total_prompt_tokens - total_cached_tokens
 *   cache_read = total_cached_tokens
 *   output     = total_completion_tokens
 *   total      = total_prompt_tokens + total_completion_tokens
 *
 * Cost: Devin bills in ACU/credits, and `sessions.metadata` reports zero under
 * zero-data-retention, so USD comes from `util::pricing` public rates — the
 * same treatment Grok Build gives its cost-less logs.
 *
 * A session with no transcript carries no local counts and is skipped rather
 * than estimated.
 */

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::model::{DataSource, EventRow, EventsSnapshotData, SourceResult};
use crate::util::date::ch_now;
use crate::util::hash::{hash_project_name_sync, make_dedup_key};
use crate::util::pricing::resolve_reported_cost;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DevinSourceOptions {
    pub machine_name: String,
    pub hash_projects: bool,
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Override the Devin data dir (tests). When None, resolves `DEVIN_HOME`,
    /// then `XDG_DATA_HOME/devin`, then `~/.local/share/devin`.
    pub base_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Parsed shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AtifTranscript {
    session_id: Option<String>,
    agent: Option<AtifAgent>,
    final_metrics: Option<AtifFinalMetrics>,
    #[serde(default)]
    steps: Vec<AtifStep>,
}

#[derive(Debug, Deserialize)]
struct AtifAgent {
    model_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AtifFinalMetrics {
    total_prompt_tokens: Option<u64>,
    total_completion_tokens: Option<u64>,
    total_cached_tokens: Option<u64>,
    total_cost_usd: Option<f64>,
    total_steps: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct AtifStep {
    timestamp: Option<String>,
    extra: Option<AtifStepExtra>,
}

#[derive(Debug, Deserialize)]
struct AtifStepExtra {
    generation_model: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SessionMeta {
    model: String,
    working_directory: String,
    created_at: Option<String>,
    last_activity_at: Option<String>,
}

/// Normalized token fields from one ATIF `final_metrics` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevinTokenMapping {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Map raw Devin session counters to EventRow token fields.
///
/// `total_prompt_tokens` is cache-inclusive, so cached tokens are split out of
/// input instead of being added on top of it.
pub fn map_devin_tokens(prompt_tokens: u64, cached_tokens: u64, completion_tokens: u64) -> DevinTokenMapping {
    let cache_read = cached_tokens.min(prompt_tokens);
    DevinTokenMapping {
        input_tokens: prompt_tokens.saturating_sub(cache_read),
        cache_read_tokens: cache_read,
        output_tokens: completion_tokens,
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

pub struct DevinSource {
    opts: DevinSourceOptions,
}

impl DevinSource {
    pub fn new(opts: DevinSourceOptions) -> Self {
        Self { opts }
    }

    pub fn name(&self) -> &'static str {
        "devin"
    }
}

#[async_trait]
impl DataSource for DevinSource {
    fn name(&self) -> &'static str {
        "devin"
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let events = fetch_devin_events(&self.opts)?;
        if self.opts.verbose {
            eprintln!("Devin Source parsed {} rows.", events.len());
        }
        Ok(SourceResult {
            source_name: self.name().to_string(),
            data: EventsSnapshotData { events },
            fetched_at: chrono::Utc::now().to_rfc3339(),
            error: None,
        })
    }
}

/// Pure entry used by fetch and unit tests (no async, injectable options).
pub fn fetch_devin_events(opts: &DevinSourceOptions) -> anyhow::Result<Vec<EventRow>> {
    let effective_since = if let Some(s) = &opts.since {
        Some(s.clone())
    } else if let Some(days) = opts.days_back {
        if days > 0 {
            let d = chrono::Utc::now() - chrono::Duration::days(days);
            Some(d.format("%Y-%m-%d").to_string())
        } else {
            None
        }
    } else {
        None
    };

    let data_dir = resolve_data_dir(opts);
    let cli_dir = data_dir.join("cli");
    let mut events: Vec<EventRow> = Vec::new();
    let now = ch_now();

    let transcripts = collect_transcripts(&cli_dir);
    if transcripts.is_empty() {
        if opts.verbose {
            eprintln!("Devin transcripts not found under {}", cli_dir.display());
        }
        return Ok(events);
    }

    let session_meta = load_sessions(&cli_dir.join("sessions.db"));
    let mut daily_sums: HashMap<String, (u64, u64, u64, u64, f64, u32, String)> = HashMap::new();

    for (session_id, atif) in transcripts {
        let metrics = match &atif.final_metrics {
            Some(m) => m,
            None => continue,
        };
        let prompt = metrics.total_prompt_tokens.unwrap_or(0);
        let cached = metrics.total_cached_tokens.unwrap_or(0);
        let completion = metrics.total_completion_tokens.unwrap_or(0);
        let mapped = map_devin_tokens(prompt, cached, completion);
        if mapped.total_tokens == 0 {
            continue;
        }

        let (first_ts, last_ts) = step_span(&atif.steps);
        let date = resolve_date(last_ts.as_deref(), session_meta.get(&session_id));
        if date.is_empty() {
            continue;
        }
        if let Some(ref eff) = effective_since {
            if &date < eff {
                continue;
            }
        }
        if let Some(ref ed) = opts.end_date {
            if &date > ed {
                continue;
            }
        }

        let meta = session_meta.get(&session_id);
        let model = resolve_model(meta, &atif);
        let cwd = meta.map(|m| m.working_directory.clone()).unwrap_or_default();
        let entries = metrics.total_steps.unwrap_or(atif.steps.len() as u32);

        let cost = resolve_reported_cost(
            &model,
            metrics.total_cost_usd.unwrap_or(0.0),
            mapped.input_tokens,
            mapped.cache_read_tokens,
            0,
            mapped.output_tokens,
        );

        let hashed_session_id = hash_project_name_sync(&session_id, opts.hash_projects);
        let hashed_proj = hash_project_name_sync(
            if cwd.is_empty() { &session_id } else { &cwd },
            opts.hash_projects,
        );

        let raw_session_key = format!(
            "devin|{}|session|{}|{}|{}",
            opts.machine_name, date, model, hashed_session_id
        );

        events.push(EventRow {
            date: date.clone(),
            record_type: "session".to_string(),
            record_key: hashed_session_id.clone(),
            source: "devin".to_string(),
            machine_name: opts.machine_name.clone(),
            account_id: String::new(),
            api_key_id: String::new(),
            model_name: model.clone(),
            session_id: hashed_session_id,
            project_path: hashed_proj,
            input_tokens: mapped.input_tokens,
            output_tokens: mapped.output_tokens,
            cache_creation_tokens: 0,
            cache_read_tokens: mapped.cache_read_tokens,
            reasoning_tokens: 0,
            total_tokens: mapped.total_tokens,
            cost: (cost * 100.0).round() / 100.0,
            dedup_key: make_dedup_key(&raw_session_key),
            import_id: opts.import_id.clone(),
            start_time: first_ts.as_deref().map(format_ch_datetime),
            end_time: last_ts.as_deref().map(format_ch_datetime),
            actual_end_time: None,
            is_active: 0,
            is_gap: 0,
            entries,
            burn_rate: 0.0,
            projection: 0.0,
            usage_limit_reset_time: None,
            block_id: String::new(),
            created_at: now.clone(),
            updated_at: now.clone(),
        });

        let daily_key = format!("{}|{}", date, model);
        let d = daily_sums
            .entry(daily_key)
            .or_insert((0, 0, 0, 0, 0.0, 0, String::new()));
        d.0 += mapped.input_tokens;
        d.1 += mapped.output_tokens;
        d.2 += mapped.cache_read_tokens;
        d.3 += mapped.total_tokens;
        d.4 += cost;
        d.5 += entries;
        if d.6.is_empty() && !cwd.is_empty() {
            d.6 = cwd;
        }
    }

    for (key, sum) in &daily_sums {
        let (input, output, cache_read, total, cost, entries, ref cwd) = *sum;
        let parts: Vec<&str> = key.splitn(2, '|').collect();
        if parts.len() < 2 {
            continue;
        }
        let date = parts[0];
        let model = parts[1];
        let hashed_proj = hash_project_name_sync(
            if cwd.is_empty() { "unknown" } else { cwd },
            opts.hash_projects,
        );

        let raw_daily_key = format!(
            "devin|{}|daily|{}|{}|{}",
            opts.machine_name, date, model, date
        );

        events.push(EventRow {
            date: date.to_string(),
            record_type: "daily".to_string(),
            record_key: date.to_string(),
            source: "devin".to_string(),
            machine_name: opts.machine_name.clone(),
            account_id: String::new(),
            api_key_id: String::new(),
            model_name: model.to_string(),
            session_id: String::new(),
            project_path: hashed_proj,
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: 0,
            cache_read_tokens: cache_read,
            reasoning_tokens: 0,
            total_tokens: total,
            cost: (cost * 100.0).round() / 100.0,
            dedup_key: make_dedup_key(&raw_daily_key),
            import_id: opts.import_id.clone(),
            start_time: None,
            end_time: None,
            actual_end_time: None,
            is_active: 0,
            is_gap: 0,
            entries,
            burn_rate: 0.0,
            projection: 0.0,
            usage_limit_reset_time: None,
            block_id: String::new(),
            created_at: now.clone(),
            updated_at: now.clone(),
        });
    }

    Ok(events)
}

/// `DEVIN_HOME` → `XDG_DATA_HOME/devin` → `~/.local/share/devin`.
fn resolve_data_dir(opts: &DevinSourceOptions) -> PathBuf {
    if let Some(ref d) = opts.base_dir {
        return d.clone();
    }
    if let Ok(h) = env::var("DEVIN_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    if let Ok(x) = env::var("XDG_DATA_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("devin");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("share")
        .join("devin")
}

/// Load every ATIF transcript, keyed by session id.
///
/// Both export directories are scanned because Devin has shipped each as the
/// default `--export` path; when a session lands in both, the larger
/// `final_metrics` wins so nothing is double counted.
fn collect_transcripts(cli_dir: &Path) -> Vec<(String, AtifTranscript)> {
    let mut by_session: HashMap<String, AtifTranscript> = HashMap::new();

    for dir in ["agent_logs", "transcripts"] {
        let path = cli_dir.join(dir);
        let entries = match fs::read_dir(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file = entry.path();
            if file.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = match fs::read_to_string(&file) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let atif: AtifTranscript = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let sid = atif
                .session_id
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    file.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.trim_start_matches("devin-").to_string())
                });
            let sid = match sid {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            match by_session.get(&sid) {
                Some(prev) if total_prompt(prev) >= total_prompt(&atif) => continue,
                _ => {
                    by_session.insert(sid, atif);
                }
            }
        }
    }

    let mut out: Vec<(String, AtifTranscript)> = by_session.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn total_prompt(atif: &AtifTranscript) -> u64 {
    atif.final_metrics
        .as_ref()
        .and_then(|m| m.total_prompt_tokens)
        .unwrap_or(0)
}

fn step_span(steps: &[AtifStep]) -> (Option<String>, Option<String>) {
    let mut first: Option<&str> = None;
    let mut last: Option<&str> = None;
    for step in steps {
        let Some(ts) = step.timestamp.as_deref() else {
            continue;
        };
        if ts.len() < 10 {
            continue;
        }
        if first.is_none() {
            first = Some(ts);
        }
        last = Some(ts);
    }
    (first.map(str::to_string), last.map(str::to_string))
}

/// Attribute a session to the date of its last recorded activity, preferring
/// the transcript's final step over the database timestamp.
fn resolve_date(last_step_ts: Option<&str>, meta: Option<&SessionMeta>) -> String {
    if let Some(ts) = last_step_ts {
        if ts.len() >= 10 {
            return ts[..10].to_string();
        }
    }
    meta.and_then(|m| {
        m.last_activity_at
            .as_deref()
            .or(m.created_at.as_deref())
            .filter(|ts| ts.len() >= 10)
            .map(|ts| ts[..10].to_string())
    })
    .unwrap_or_default()
}

/// `sessions.db` model wins, then the last step's canonical model id, then
/// the ATIF display name (`sessions.model` is empty for some sessions).
fn resolve_model(meta: Option<&SessionMeta>, atif: &AtifTranscript) -> String {
    if let Some(m) = meta {
        if !m.model.is_empty() {
            return m.model.clone();
        }
    }
    for step in atif.steps.iter().rev() {
        if let Some(m) = step.extra.as_ref().and_then(|e| e.generation_model.as_ref()) {
        if !m.is_empty() {
            return m.to_string();
        }
    }
    }
    if let Some(m) = atif.agent.as_ref().and_then(|a| a.model_name.as_ref()) {
        if !m.is_empty() {
            return m.clone();
        }
    }
    "unknown".to_string()
}

/// Read `sessions.db` read-only — Devin may hold it open, and it is large
/// enough that copying it per import is not worth it. Failure is non-fatal:
/// transcripts still import, just without project paths.
fn load_sessions(db_path: &Path) -> HashMap<String, SessionMeta> {
    let mut map = HashMap::new();
    if !db_path.exists() {
        return map;
    }
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return map,
    };
    let mut stmt = match conn
        .prepare("SELECT id, working_directory, model, created_at, last_activity_at FROM sessions")
    {
        Ok(s) => s,
        Err(_) => return map,
    };
    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1).unwrap_or_default(),
            row.get::<_, String>(2).unwrap_or_default(),
            row_i64_flex(row, 3).and_then(date_from_unix),
            row_i64_flex(row, 4).and_then(date_from_unix),
        ))
    }) {
        Ok(r) => r,
        Err(_) => return map,
    };
    for row in rows.flatten() {
        map.insert(
            row.0,
            SessionMeta {
                model: row.2,
                working_directory: row.1,
                created_at: row.3,
                last_activity_at: row.4,
            },
        );
    }
    map
}

/// Devin stores `sessions` timestamps as INTEGER unix seconds; tolerate REAL.
fn row_i64_flex(row: &rusqlite::Row, idx: usize) -> Option<i64> {
    if let Ok(v) = row.get::<_, Option<i64>>(idx) {
        return v;
    }
    row.get::<_, f64>(idx).ok().map(|f| f as i64)
}

fn date_from_unix(seconds: i64) -> Option<String> {
    if seconds <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(seconds, 0).map(|dt| dt.format("%Y-%m-%d").to_string())
}

fn format_ch_datetime(iso: &str) -> String {
    // "2026-09-23T18:49:58.144646647Z" → "2026-09-23 18:49:58"
    if iso.len() >= 19 {
        let date = &iso[0..10];
        let time = &iso[11..19];
        format!("{} {}", date, time)
    } else {
        iso.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_fixture(dir: &Path, relative: &str, content: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    fn base_opts(base: PathBuf) -> DevinSourceOptions {
        DevinSourceOptions {
            machine_name: "test-host".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "import-test".into(),
            base_dir: Some(base),
        }
    }

    fn atif_json(sid: &str, model: &str, prompt: u64, cached: u64, completion: u64, steps: u32) -> String {
        format!(
            r#"{{
              "schema_version": "ATIF-v1.7",
              "session_id": "{sid}",
              "agent": {{ "name": "devin", "model_name": "{model}" }},
              "steps": [
                {{ "step_id": 1, "timestamp": "2026-09-23T18:00:00.000Z", "source": "user", "message": {{}}, "extra": {{}} }},
                {{ "step_id": 2, "timestamp": "2026-09-23T18:49:58.144Z", "source": "agent", "message": {{}}, "extra": {{ "generation_model": "{model}" }} }}
              ],
              "final_metrics": {{
                "total_prompt_tokens": {prompt},
                "total_completion_tokens": {completion},
                "total_cached_tokens": {cached},
                "total_steps": {steps}
              }}
            }}"#
        )
    }

    #[test]
    fn test_name() {
        let src = DevinSource::new(base_opts(PathBuf::from("/nonexistent")));
        assert_eq!(src.name(), "devin");
    }

    #[test]
    fn prompt_tokens_are_cache_inclusive() {
        let m = map_devin_tokens(10_000, 8_000, 500);
        assert_eq!(m.input_tokens, 2_000);
        assert_eq!(m.cache_read_tokens, 8_000);
        assert_eq!(m.output_tokens, 500);
        assert_eq!(m.total_tokens, 10_500);
    }

    #[test]
    fn cached_cannot_exceed_prompt() {
        let m = map_devin_tokens(100, 250, 0);
        assert_eq!(m.cache_read_tokens, 100);
        assert_eq!(m.input_tokens, 0);
        assert_eq!(m.total_tokens, 100);
    }

    #[test]
    fn reads_agent_logs_transcript() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-veil-hardcover.json",
            &atif_json("veil-hardcover", "swe-2-high", 21_321, 640, 29, 9),
        );

        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let sessions: Vec<_> = events.iter().filter(|e| e.record_type == "session").collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source, "devin");
        assert_eq!(sessions[0].machine_name, "test-host");
        assert_eq!(sessions[0].date, "2026-09-23");
        assert_eq!(sessions[0].model_name, "swe-2-high");
        assert_eq!(sessions[0].input_tokens, 20_681);
        assert_eq!(sessions[0].cache_read_tokens, 640);
        assert_eq!(sessions[0].output_tokens, 29);
        assert_eq!(sessions[0].total_tokens, 21_350);
        assert_eq!(sessions[0].entries, 9);
        assert_eq!(
            sessions[0].start_time.as_deref(),
            Some("2026-09-23 18:00:00")
        );
        assert_eq!(sessions[0].end_time.as_deref(), Some("2026-09-23 18:49:58"));
    }

    #[test]
    fn reads_legacy_transcripts_dir() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/transcripts/literate-stone.json",
            &atif_json("literate-stone", "swe-2-high", 132_414, 64_026, 4_648, 16),
        );

        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let sessions: Vec<_> = events.iter().filter(|e| e.record_type == "session").collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].input_tokens, 68_388);
        assert_eq!(sessions[0].total_tokens, 137_062);
    }

    #[test]
    fn same_session_in_both_dirs_is_not_double_counted() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-melted-guarantee.json",
            &atif_json("melted-guarantee", "swe-2-high", 10_923_484, 10_727_846, 54_163, 86),
        );
        // Stale partial copy in the legacy dir must lose to the larger metrics.
        write_fixture(
            tmp.path(),
            "cli/transcripts/melted-guarantee.json",
            &atif_json("melted-guarantee", "swe-2-high", 500, 400, 10, 2),
        );

        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let sessions: Vec<_> = events.iter().filter(|e| e.record_type == "session").collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].total_tokens, 10_977_647);
    }

    #[test]
    fn daily_rows_aggregate_per_date_and_model() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-a.json",
            &atif_json("a", "swe-2-high", 1_000, 800, 100, 5),
        );
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-b.json",
            &atif_json("b", "swe-2-high", 2_000, 1_500, 200, 7),
        );
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-c.json",
            &atif_json("c", "swe-2-max", 4_000, 0, 400, 3),
        );

        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let daily: Vec<_> = events.iter().filter(|e| e.record_type == "daily").collect();
        assert_eq!(daily.len(), 2);

        let high = daily.iter().find(|e| e.model_name == "swe-2-high").unwrap();
        assert_eq!(high.total_tokens, 3_300);
        assert_eq!(high.input_tokens, 700);
        assert_eq!(high.cache_read_tokens, 2_300);
        assert_eq!(high.entries, 12);

        let max = daily.iter().find(|e| e.model_name == "swe-2-max").unwrap();
        assert_eq!(max.total_tokens, 4_400);
        assert_eq!(max.input_tokens, 4_000);
    }

    #[test]
    fn skips_transcripts_without_final_metrics() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-empty.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"empty","steps":[]}"#,
        );
        assert!(fetch_devin_events(&base_opts(tmp.path().to_path_buf()))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn skips_zero_token_transcripts() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-zero.json",
            &atif_json("zero", "swe-2-high", 0, 0, 0, 0),
        );
        assert!(fetch_devin_events(&base_opts(tmp.path().to_path_buf()))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn missing_data_dir_returns_no_rows() {
        let tmp = TempDir::new().unwrap();
        let events = fetch_devin_events(&base_opts(tmp.path().join("nope"))).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn since_and_end_date_filter_sessions() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-old.json",
            &atif_json("old", "swe-2-high", 1_000, 0, 10, 2),
        );
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-old2.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"old2",
                "steps":[{"step_id":1,"timestamp":"2026-09-01T00:00:00.000Z","source":"user","message":{}}],
                "final_metrics":{"total_prompt_tokens":500,"total_completion_tokens":5,"total_cached_tokens":0,"total_steps":1}}"#,
        );

        let mut opts = base_opts(tmp.path().to_path_buf());
        opts.since = Some("2026-09-10".into());
        let events = fetch_devin_events(&opts).unwrap();
        let sessions: Vec<_> = events.iter().filter(|e| e.record_type == "session").collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].date, "2026-09-23");

        let mut opts = base_opts(tmp.path().to_path_buf());
        opts.end_date = Some("2026-09-10".into());
        let events = fetch_devin_events(&opts).unwrap();
        let sessions: Vec<_> = events.iter().filter(|e| e.record_type == "session").collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].date, "2026-09-01");
    }

    #[test]
    fn sessions_db_supplies_model_and_project_path() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-db.json",
            &atif_json("db", "SWE-2 High", 1_000, 0, 10, 2),
        );
        let db = tmp.path().join("cli/sessions.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, \
                 model TEXT NOT NULL, created_at INTEGER NOT NULL, last_activity_at INTEGER NOT NULL);
                 INSERT INTO sessions VALUES ('db', '/opt/workspace/app', 'swe-2-high', 1789887681, 1789887999);",
            )
            .unwrap();
        }

        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(session.model_name, "swe-2-high");
        assert_eq!(session.project_path, "/opt/workspace/app");
    }

    #[test]
    fn falls_back_to_step_model_when_db_model_empty() {
        let tmp = TempDir::new().unwrap();
        // Real ATIF: `agent.model_name` is the display label, while the step's
        // `extra.generation_model` carries the canonical id summa stores.
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-nodb.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"nodb",
                "agent":{"name":"devin","model_name":"SWE-2 Max"},
                "steps":[{"step_id":1,"timestamp":"2026-09-23T18:49:58.144Z","source":"agent",
                          "message":{},"extra":{"generation_model":"swe-2-max"}}],
                "final_metrics":{"total_prompt_tokens":1000,"total_completion_tokens":10,
                                 "total_cached_tokens":0,"total_steps":2}}"#,
        );
        let db = tmp.path().join("cli/sessions.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, \
                 model TEXT NOT NULL, created_at INTEGER NOT NULL, last_activity_at INTEGER NOT NULL);
                 INSERT INTO sessions VALUES ('nodb', '/opt/work', '', 1789887681, 1789887999);",
            )
            .unwrap();
        }

        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(session.model_name, "swe-2-max");
        assert_eq!(session.project_path, "/opt/work");
    }

    #[test]
    fn falls_back_to_display_name_when_no_canonical_model() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-display.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"display",
                "agent":{"name":"devin","model_name":"SWE-2 High"},
                "steps":[{"step_id":1,"timestamp":"2026-09-23T18:00:00.000Z","source":"user","message":{},"extra":{}}],
                "final_metrics":{"total_prompt_tokens":1000,"total_completion_tokens":10,
                                 "total_cached_tokens":0,"total_steps":1}}"#,
        );
        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(session.model_name, "SWE-2 High");
    }

    #[test]
    fn no_model_anywhere_falls_back_to_unknown() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-nomodel.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"nomodel",
                "steps":[{"step_id":1,"timestamp":"2026-09-23T18:00:00.000Z","source":"user","message":{},"extra":{}}],
                "final_metrics":{"total_prompt_tokens":1000,"total_completion_tokens":10,
                                 "total_cached_tokens":0,"total_steps":1}}"#,
        );
        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(session.model_name, "unknown");
    }

    #[test]
    fn date_falls_back_to_db_activity_when_steps_have_no_timestamp() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-nots.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"nots",
                "agent":{"model_name":"swe-2-high"},
                "steps":[{"step_id":1,"source":"agent","message":{}}],
                "final_metrics":{"total_prompt_tokens":1000,"total_completion_tokens":10,
                                 "total_cached_tokens":0,"total_steps":1}}"#,
        );
        let db = tmp.path().join("cli/sessions.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, \
                 model TEXT NOT NULL, created_at INTEGER NOT NULL, last_activity_at INTEGER NOT NULL);
                 INSERT INTO sessions VALUES ('nots', '/opt/work', 'swe-2-high', 1789819200, 1789840799);",
            )
            .unwrap();
        }
        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(session.date, "2026-09-19");
    }

    #[test]
    fn hashing_applies_to_session_and_project() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-hash.json",
            &atif_json("sess-hash", "swe-2-high", 1_000, 0, 10, 2),
        );
        let mut opts = base_opts(tmp.path().to_path_buf());
        opts.hash_projects = true;
        let events = fetch_devin_events(&opts).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(session.session_id.len(), 8);
        assert_eq!(session.project_path.len(), 8);
        assert_eq!(session.dedup_key.len(), 16);
    }

    #[test]
    fn reported_cost_wins_when_present() {
        let tmp = TempDir::new().unwrap();
        // Volume must make $1.25 plausible, or resolve_reported_cost rejects it
        // as an absurd blended rate and falls back to the public-rate estimate.
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-cost.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"cost",
                "agent":{"model_name":"swe-2-high"},
                "steps":[{"step_id":1,"timestamp":"2026-09-23T10:00:00.000Z","source":"agent","message":{}}],
                "final_metrics":{"total_prompt_tokens":500000,"total_completion_tokens":20000,
                                 "total_cached_tokens":0,"total_cost_usd":1.25,"total_steps":1}}"#,
        );
        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert!((session.cost - 1.25).abs() < 1e-9, "got {}", session.cost);
    }

    #[test]
    fn absurd_reported_cost_falls_back_to_estimate() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-bad-cost.json",
            r#"{"schema_version":"ATIF-v1.7","session_id":"bad",
                "agent":{"model_name":"claude-sonnet-4.6"},
                "steps":[{"step_id":1,"timestamp":"2026-09-23T10:00:00.000Z","source":"agent","message":{}}],
                "final_metrics":{"total_prompt_tokens":2000,"total_completion_tokens":100,
                                 "total_cached_tokens":0,"total_cost_usd":900.0,"total_steps":1}}"#,
        );
        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        // 2M-style estimate scaled: 2k input + 100 output at Sonnet rates ≈ $0.0015.
        assert!(session.cost < 1.0, "absurd report must not be trusted, got {}", session.cost);
    }

    #[test]
    fn zero_cost_falls_back_to_public_rates() {
        let tmp = TempDir::new().unwrap();
        write_fixture(
            tmp.path(),
            "cli/agent_logs/devin-est.json",
            &atif_json("est", "claude-sonnet-4.6", 2_000_000, 0, 100_000, 4),
        );
        let events = fetch_devin_events(&base_opts(tmp.path().to_path_buf())).unwrap();
        let session = events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        // 2M input @ $3 + 100k output @ $15 = $7.50
        assert!((session.cost - 7.5).abs() < 0.02, "got {}", session.cost);
    }
}
