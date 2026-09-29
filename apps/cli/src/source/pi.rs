/**
 * Pi Coding Agent Source
 *
 * Reads pi session transcripts from `~/.pi/agent/sessions/--<path>--/`, whose
 * files end in `.jsonl` (override: `PI_CODING_AGENT_DIR`). Sessions are JSON
 * Lines; every line is an entry with a `type`.
 *
 * Usage is carried by FOUR entry types, and all four bill real tokens:
 *   - `message`      -> `message.usage` when `message.role == "assistant"`
 *   - `usage`        -> `entry.usage` (e.g. `kind: "cache_warm"`)
 *   - `compaction`   -> `entry.usage` (the summary request)
 *   - `branch_summary` -> `entry.usage` (the branch summary request)
 *
 * Counting only assistant messages silently drops cache warming and every
 * summarization turn, so the lower three are imported too.
 *
 * Sessions form a tree. `/fork` and `/clone` write a NEW file whose
 * `parentSession` points at the original, and the new file carries a copy of
 * the parent's entries. Importing both files would bill the shared prefix
 * twice, so entries are deduped by their `id` across the whole scan.
 */

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use crate::model::{DataSource, EventRow, EventsSnapshotData, SourceResult};
use crate::util::date::ch_now;
use crate::util::hash::{hash_project_name_sync, make_dedup_key};
use crate::util::pricing::resolve_reported_cost;
use crate::util::tokens::total_tokens;

#[derive(Debug, Clone)]
pub struct PiSourceOptions {
    pub machine_name: String,
    pub hash_projects: bool,
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Override pi's agent dir (tests). When None, uses `PI_CODING_AGENT_DIR`
    /// or `~/.pi/agent`.
    pub base_dir: Option<PathBuf>,
}

pub struct PiSource {
    opts: PiSourceOptions,
}

/// One billable turn, already reduced to the fields the table needs.
struct Turn {
    date: String,
    start_time: String,
    session_id: String,
    cwd: String,
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

impl PiSource {
    pub fn new(opts: PiSourceOptions) -> Self {
        Self { opts }
    }
}

impl PiSource {
    fn name(&self) -> &'static str {
        "pi"
    }
}

#[async_trait]
impl DataSource for PiSource {
    fn name(&self) -> &'static str {
        "pi"
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let o = &self.opts;
        let base_dir = match &o.base_dir {
            Some(d) => d.clone(),
            None => match env::var("PI_CODING_AGENT_DIR") {
                Ok(v) if !v.is_empty() => PathBuf::from(v),
                _ => dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join(".pi")
                    .join("agent"),
            },
        };
        let sessions_dir = base_dir.join("sessions");

        let mut events: Vec<EventRow> = Vec::new();
        let now = ch_now();

        // A machine without pi is normal, not an error.
        if !sessions_dir.is_dir() {
            if o.verbose {
                eprintln!("pi sessions directory not found: {}", sessions_dir.display());
            }
            return Ok(SourceResult {
                source_name: self.name().to_string(),
                data: EventsSnapshotData { events },
                fetched_at: now.clone(),
                error: None,
            });
        }

        let (since, until) = match window(o.since.as_deref(), o.end_date.as_deref(), o.days_back) {
            Some(w) => w,
            None => {
                return Ok(SourceResult {
                    source_name: self.name().to_string(),
                    data: EventsSnapshotData { events },
                    fetched_at: now.clone(),
                    error: None,
                })
            }
        };

        let mut files = Vec::new();
        collect_jsonl(&sessions_dir, &mut files, 0);

        // Forked sessions duplicate their parent's entries under a new file
        // name, so the entry id — not the file — is the identity.
        let mut seen_entries: HashSet<String> = HashSet::new();
        let mut turns: Vec<Turn> = Vec::new();

        for path in &files {
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            read_session(&text, path, &mut seen_entries, &mut turns);
        }

        turns.retain(|t| {
            if t.date.is_empty() {
                return false;
            }
            if let Some(s) = &since {
                if &t.date < s {
                    return false;
                }
            }
            if let Some(u) = &until {
                if &t.date > u {
                    return false;
                }
            }
            true
        });

        events = build_rows(&turns, o, &now);

        if o.verbose {
            eprintln!(
                "pi: {} files, {} turns, {} rows",
                files.len(),
                turns.len(),
                events.len()
            );
        }

        Ok(SourceResult {
            source_name: self.name().to_string(),
            data: EventsSnapshotData { events },
            fetched_at: now,
            error: None,
        })
    }
}

/// Resolve the inclusive `YYYY-MM-DD` bounds, or `None` when unparseable.
fn window(
    since: Option<&str>,
    end_date: Option<&str>,
    days_back: Option<i64>,
) -> Option<(Option<String>, Option<String>)> {
    let since = match since {
        Some(s) => Some(s.to_string()),
        None => days_back.filter(|d| *d > 0).map(|d| {
            (chrono::Utc::now() - chrono::Duration::days(d))
                .format("%Y-%m-%d")
                .to_string()
        }),
    };
    if let Some(s) = &since {
        if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_err() {
            return None;
        }
    }
    if let Some(e) = end_date {
        if chrono::NaiveDate::parse_from_str(e, "%Y-%m-%d").is_err() {
            return None;
        }
    }
    Some((since, end_date.map(|e| e.to_string())))
}

/// Sessions live one directory deep (`--<path>--/<file>.jsonl`); allow a
/// little slack for relocated layouts without walking an unbounded tree.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out, depth + 1);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn read_session(
    text: &str,
    path: &Path,
    seen: &mut HashSet<String>,
    turns: &mut Vec<Turn>,
) {
    let mut session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut cwd = String::new();
    // `model_change` / `thinking_level_change` carry the selected model for
    // entries that omit it, such as compaction and branch summaries.
    let mut last_model: Option<String> = None;
    let mut last_provider: Option<String> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if entry_type == "session" {
            if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
                session_id = id.to_string();
            }
            if let Some(c) = entry.get("cwd").and_then(|v| v.as_str()) {
                cwd = c.to_string();
            }
            if let Some(p) = entry.get("provider").and_then(|v| v.as_str()) {
                last_provider = Some(p.to_string());
            }
            if let Some(m) = entry.get("modelId").and_then(|v| v.as_str()) {
                last_model = Some(m.to_string());
            }
            continue;
        }

        if entry_type == "model_change" {
            if let Some(m) = entry.get("modelId").and_then(|v| v.as_str()) {
                last_model = Some(m.to_string());
            }
            if let Some(p) = entry.get("provider").and_then(|v| v.as_str()) {
                last_provider = Some(p.to_string());
            }
            continue;
        }

        // Locate this entry's usage, whichever of the four shapes carries it.
        let (usage, provider, model) = match entry_type {
            "message" => {
                let Some(msg) = entry.get("message") else {
                    continue;
                };
                if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                    continue;
                }
                let Some(u) = msg.get("usage") else {
                    continue;
                };
                (
                    u,
                    msg.get("provider").and_then(|v| v.as_str()),
                    msg.get("model").and_then(|v| v.as_str()),
                )
            }
            "usage" | "compaction" | "branch_summary" => (
                match entry.get("usage") {
                    Some(u) => u,
                    None => continue,
                },
                entry.get("provider").and_then(|v| v.as_str()),
                entry.get("model").and_then(|v| v.as_str()),
            ),
            _ => continue,
        };

        // A forked session file repeats its parent's entries verbatim.
        let identity = entry
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                format!("{}@{}", path.display(), entry.get("timestamp").and_then(|v| v.as_str()).unwrap_or(""))
            });
        if !seen.insert(identity) {
            continue;
        }

        let input = num(usage, "input");
        let output = num(usage, "output");
        let cache_read = num(usage, "cacheRead");
        let cache_write = num(usage, "cacheWrite");
        let reported_total = num(usage, "totalTokens");
        let summed = total_tokens(input, output, cache_write, cache_read);
        // pi reports the provider's own total, which already includes cache.
        // Trust it, and only fall back to the four-term sum when absent.
        let total = if reported_total > 0 { reported_total } else { summed };
        if total == 0 {
            continue;
        }

        // Assistant messages name the model that answered, so they also seed
        // the fallback that compaction and branch summaries inherit.
        if let Some(m) = model {
            last_model = Some(m.to_string());
        }
        if let Some(p) = provider {
            last_provider = Some(p.to_string());
        }
        let model = model
            .map(|s| s.to_string())
            .or_else(|| last_model.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let ts = entry.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let (date, start_time) = normalize_timestamp(ts);

        let cost = match usage.get("cost") {
            Some(c) => resolve_reported_cost(
                &model,
                c.get("total").and_then(|v| v.as_f64()).unwrap_or(0.0),
                input,
                cache_read,
                cache_write,
                output,
            ),
            None => resolve_reported_cost(&model, 0.0, input, cache_read, cache_write, output),
        };

        turns.push(Turn {
            date,
            start_time,
            session_id: session_id.clone(),
            cwd: cwd.clone(),
            model,
            input,
            output,
            cache_read,
            cache_write,
            cost,
        });
    }
}

fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Entry timestamps are ISO 8601 strings; assistant messages also carry a
/// nested Unix-ms `timestamp` that is more precise.
fn normalize_timestamp(ts: &str) -> (String, String) {
    if ts.is_empty() {
        return (String::new(), String::new());
    }
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => (
            dt.format("%Y-%m-%d").to_string(),
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        ),
        Err(_) => (String::new(), String::new()),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_rows(turns: &[Turn], o: &PiSourceOptions, now: &str) -> Vec<EventRow> {
    let mut events = Vec::new();
    let mut by_session: HashMap<(String, String, String), Acc> = HashMap::new();
    let mut by_day: HashMap<(String, String), Acc> = HashMap::new();

    for t in turns {
        let session_key = (t.session_id.clone(), t.model.clone(), t.date.clone());
        let e = by_session.entry(session_key).or_insert_with(|| Acc {
            cwd: t.cwd.clone(),
            ..Default::default()
        });
        e.add(t);

        let day_key = (t.date.clone(), t.model.clone());
        let d = by_day.entry(day_key).or_insert_with(|| Acc {
            cwd: t.cwd.clone(),
            ..Default::default()
        });
        d.add(t);
    }

    for ((session_id, model, date), acc) in &by_session {
        let session_key = hash_project_name_sync(session_id, o.hash_projects);
        let project = hash_project_name_sync(
            if acc.cwd.is_empty() { session_id } else { &acc.cwd },
            o.hash_projects,
        );
        events.push(EventRow {
            date: date.clone(),
            record_type: "session".to_string(),
            record_key: session_key.clone(),
            source: "pi".to_string(),
            machine_name: o.machine_name.clone(),
            model_name: model.clone(),
            session_id: session_id.clone(),
            project_path: project,
            input_tokens: acc.input,
            output_tokens: acc.output,
            cache_creation_tokens: acc.cache_write,
            cache_read_tokens: acc.cache_read,
            total_tokens: acc.total(),
            cost: acc.cost,
            dedup_key: make_dedup_key(&format!(
                "pi|{}|session|{}|{}|{}",
                o.machine_name, date, model, session_key
            )),
            import_id: o.import_id.clone(),
            start_time: acc.first_start.clone(),
            entries: acc.entries,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            ..EventRow::default()
        });
    }

    for ((date, model), acc) in &by_day {
        let project = hash_project_name_sync(
            if acc.cwd.is_empty() { "unknown" } else { &acc.cwd },
            o.hash_projects,
        );
        events.push(EventRow {
            date: date.clone(),
            record_type: "daily".to_string(),
            record_key: date.clone(),
            source: "pi".to_string(),
            machine_name: o.machine_name.clone(),
            model_name: model.clone(),
            project_path: project,
            input_tokens: acc.input,
            output_tokens: acc.output,
            cache_creation_tokens: acc.cache_write,
            cache_read_tokens: acc.cache_read,
            total_tokens: acc.total(),
            cost: (acc.cost * 100.0).round() / 100.0,
            dedup_key: make_dedup_key(&format!(
                "pi|{}|daily|{}|{}|{}",
                o.machine_name, date, model, date
            )),
            import_id: o.import_id.clone(),
            entries: acc.entries,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            ..EventRow::default()
        });
    }

    events
}

#[derive(Default)]
struct Acc {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    entries: u32,
    cwd: String,
    first_start: Option<String>,
}

impl Acc {
    fn add(&mut self, t: &Turn) {
        self.input += t.input;
        self.output += t.output;
        self.cache_read += t.cache_read;
        self.cache_write += t.cache_write;
        self.cost += t.cost;
        self.entries = self.entries.saturating_add(1);
        if self.cwd.is_empty() {
            self.cwd = t.cwd.clone();
        }
        if self.first_start.is_none() {
            self.first_start = Some(t.start_time.clone());
        }
    }

    fn total(&self) -> u64 {
        total_tokens(self.input, self.output, self.cache_write, self.cache_read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path) -> PiSourceOptions {
        PiSourceOptions {
            machine_name: "test-machine".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "import-1".into(),
            base_dir: Some(dir.to_path_buf()),
        }
    }

    /// Write a session transcript using the exact shapes from pi's
    /// documented `session-format` page.
    fn write_session(dir: &Path, bucket: &str, name: &str, body: &str) {
        let d = dir.join("sessions").join(bucket);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), body).unwrap();
    }

    #[test]
    fn name_is_pi() {
        let src = PiSource::new(opts(Path::new("/tmp")));
        assert_eq!(src.name(), "pi");
    }

    #[test]
    fn missing_sessions_dir_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let src = PiSource::new(opts(&tmp.path().join("nope")));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert!(r.error.is_none());
        assert!(r.data.events.is_empty());
    }

    #[test]
    fn assistant_usage_splits_into_four_columns() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--Users-me-proj--",
            "2024-12-03T14_00_00_s1.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/Users/me/proj"}"#,
                "\n",
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":100,"output":20,"cacheRead":50,"cacheWrite":10,"totalTokens":180,"cost":{"input":0.3,"output":0.3,"cacheRead":0.015,"cacheWrite":0.0375,"total":0.6525}}}}"#,
                "\n",
            ),
        );

        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let session: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "session")
            .collect();
        assert_eq!(session.len(), 1);
        let s = session[0];
        assert_eq!(s.source, "pi");
        assert_eq!(s.input_tokens, 100);
        assert_eq!(s.output_tokens, 20);
        assert_eq!(s.cache_read_tokens, 50);
        assert_eq!(s.cache_creation_tokens, 10);
        assert_eq!(s.total_tokens, 180);
        assert_eq!(s.model_name, "claude-sonnet-4-5");
        assert_eq!(s.project_path, "/Users/me/proj");
        assert_eq!(s.date, "2024-12-03");
        assert_eq!(s.entries, 1);
        assert!((s.cost - 0.6525).abs() < 0.01, "cost was {}", s.cost);
    }

    #[test]
    fn usage_entries_count_beyond_assistant_messages() {
        // Cache warming is a `usage` entry, not a message. Counting only
        // assistant turns would report 0 tokens for a pure cache-warm turn.
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--Users-me-proj--",
            "s2.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s2","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p"}"#,
                "\n",
                r#"{"type":"usage","id":"u1","parentId":"x","timestamp":"2024-12-03T14:08:00.000Z","kind":"cache_warm","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":0,"output":0,"cacheRead":50000,"cacheWrite":0,"totalTokens":50000,"cost":{"input":0,"output":0,"cacheRead":0.015,"cacheWrite":0,"total":0.015}}}"#,
                "\n",
            ),
        );

        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let daily: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .collect();
        assert_eq!(daily.len(), 1, "cache warming must not be dropped");
        assert_eq!(daily[0].cache_read_tokens, 50000);
        assert_eq!(daily[0].total_tokens, 50000);
    }

    #[test]
    fn compaction_usage_uses_last_known_model() {
        // Compaction entries carry usage but no model of their own.
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--Users-me-proj--",
            "s3.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s3","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p"}"#,
                "\n",
                r#"{"type":"message","id":"m1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-4-5","usage":{"input":10,"output":2,"totalTokens":12}}}"#,
                "\n",
                r#"{"type":"compaction","id":"c1","timestamp":"2024-12-03T14:10:00.000Z","summary":"s","firstKeptEntryId":"m1","tokensBefore":50000,"usage":{"input":900,"output":100,"totalTokens":1000}}"#,
                "\n",
            ),
        );

        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let daily: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .collect();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].model_name, "claude-opus-4-5");
        assert_eq!(daily[0].input_tokens, 910);
        assert_eq!(daily[0].output_tokens, 102);
    }

    #[test]
    fn forked_session_does_not_double_count_shared_prefix() {
        // `/fork` writes a new file containing a copy of the parent's entries.
        // Both files are on disk, so a naive scan bills the shared turn twice.
        let tmp = tempfile::tempdir().unwrap();
        let shared = r#"{"type":"message","id":"shared-turn","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":1000,"output":100,"totalTokens":1100}}}"#;
        let header = r#"{"type":"session","version":3,"id":"s4","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p"}"#;

        write_session(
            tmp.path(),
            "--Users-me-proj--",
            "orig.jsonl",
            &format!("{header}\n{shared}\n"),
        );
        // The fork repeats the parent's id and adds its own turn.
        write_session(
            tmp.path(),
            "--Users-me-proj--",
            "fork.jsonl",
            &format!(
                "{}\n{}\n{}\n",
                r#"{"type":"session","version":3,"id":"s5","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p","parentSession":"/x/orig.jsonl"}"#,
                shared,
                r#"{"type":"message","id":"fork-only","timestamp":"2024-12-03T14:05:00.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":50,"output":5,"totalTokens":55}}}"#,
            ),
        );

        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let daily: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .collect();
        assert_eq!(daily.len(), 1);
        // 1000 + 50 input, 100 + 5 output: the shared 1000/100 counted once.
        assert_eq!(daily[0].input_tokens, 1050, "fork shared a turn with its parent");
        assert_eq!(daily[0].output_tokens, 105);
        assert_eq!(daily[0].total_tokens, 1155);
    }

    #[test]
    fn total_falls_back_to_four_term_sum_without_totalTokens() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--p--",
            "s6.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s6","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p"}"#,
                "\n",
                r#"{"type":"message","id":"m1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","model":"m","usage":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4}}}"#,
                "\n",
            ),
        );
        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let s = r
            .data
            .events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(s.total_tokens, 10, "1+2+3+4 with cache counted once");
    }

    #[test]
    fn zero_token_turns_produce_no_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--p--",
            "s7.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s7","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p"}"#,
                "\n",
                r#"{"type":"message","id":"m1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","model":"m","usage":{"input":0,"output":0}}}"#,
                "\n",
            ),
        );
        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert!(r.data.events.is_empty(), "no fabricated zero rows");
    }

    #[test]
    fn since_and_end_date_filter_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--p--",
            "s8.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s8","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/p"}"#,
                "\n",
                r#"{"type":"message","id":"old","timestamp":"2026-01-01T10:00:00.000Z","message":{"role":"assistant","model":"m","usage":{"input":5,"output":1,"totalTokens":6}}}"#,
                "\n",
                r#"{"type":"message","id":"new","timestamp":"2026-06-01T10:00:00.000Z","message":{"role":"assistant","model":"m","usage":{"input":7,"output":2,"totalTokens":9}}}"#,
                "\n",
            ),
        );
        let mut o = opts(tmp.path());
        o.since = Some("2026-05-01".into());
        let src = PiSource::new(o);
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let d: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .collect();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].date, "2026-06-01");
        assert_eq!(d[0].input_tokens, 7);
    }

    #[test]
    fn model_breakdown_produces_one_row_per_model() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--p--",
            "s9.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s9","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/p"}"#,
                "\n",
                r#"{"type":"message","id":"a","timestamp":"2026-01-01T10:00:00.000Z","message":{"role":"assistant","model":"claude-sonnet-4-5","usage":{"input":10,"output":1,"totalTokens":11}}}"#,
                "\n",
                r#"{"type":"message","id":"b","timestamp":"2026-01-01T10:01:00.000Z","message":{"role":"assistant","model":"gpt-5.5","usage":{"input":20,"output":2,"totalTokens":22}}}"#,
                "\n",
            ),
        );
        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let d: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .collect();
        assert_eq!(d.len(), 2, "each model gets its own row");
        assert!(d.iter().all(|e| e.total_tokens > 0));
    }

    #[test]
    fn malformed_lines_are_skipped_without_failing_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--p--",
            "s10.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s10","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/p"}"#,
                "\n",
                "{ this is not json\n",
                "\n",
                r#"{"type":"message","id":"a","timestamp":"2026-01-01T10:00:00.000Z","message":{"role":"assistant","model":"m","usage":{"input":10,"output":1,"totalTokens":11}}}"#,
                "\n",
            ),
        );
        let src = PiSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let s: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "session")
            .collect();
        assert_eq!(s.len(), 1, "one bad line must not drop its siblings");
        assert_eq!(s[0].input_tokens, 10);
    }

    #[test]
    fn hashed_projects_hide_cwd_but_keep_it_stable() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "--p--",
            "s11.jsonl",
            concat!(
                r#"{"type":"session","version":3,"id":"s11","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/Users/me/secret-proj"}"#,
                "\n",
                r#"{"type":"message","id":"a","timestamp":"2026-01-01T10:00:00.000Z","message":{"role":"assistant","model":"m","usage":{"input":10,"output":1,"totalTokens":11}}}"#,
                "\n",
            ),
        );
        let mut o = opts(tmp.path());
        o.hash_projects = true;
        let src = PiSource::new(o);
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let s = r
            .data
            .events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(s.project_path.len(), 8);
        assert!(!s.project_path.contains("secret-proj"));
    }
}
