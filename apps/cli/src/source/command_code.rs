/**
 * Command Code Source
 *
 * Reads Command Code session transcripts from
 * `~/.commandcode/projects/<project-slug>/<session-id>.jsonl`.
 *
 * Format verified against the shipped CLI (command-code 1.69.0,
 * `dist/cli.mjs`), not inferred from docs:
 *
 *   header   {"type":"session","version":3,"id":…,"timestamp":ISO,"cwd":…}
 *   entry    {"type":"message","id":…,"parentId":…,"timestamp":ISO,
 *             "message":{"role":"assistant","model":…,"usage":{…}}}
 *
 * The persisted usage object is built by the CLI's `toSessionUsage`:
 *
 *   usage: { inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens,
 *            cacheWriteTokens1h?, costUsd? }
 *
 * `inputTokens` is NON-cached (cache arrives via `inputTokenDetails`), so the
 * total is the four-term sum and cache is never double counted. `costUsd` is
 * the CLI's own local estimate, not an invoice.
 *
 * Two hazards the format creates, both handled below:
 *   - `/fork` and `/clone` write a NEW file containing a copy of the source
 *     entries, with the header's `parentSession` pointing at the original, so
 *     scanning the whole tree bills the shared prefix twice.
 *   - Entries form a tree; the same logical turn can be reached by more than
 *     one path. Dedupe is on the entry `id`, which is the real identity.
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
pub struct CommandCodeSourceOptions {
    pub machine_name: String,
    pub hash_projects: bool,
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Override Command Code's home (tests). When None, uses
    /// `COMMANDCODE_HOME` or `~/.commandcode`.
    pub base_dir: Option<PathBuf>,
}

pub struct CommandCodeSource {
    opts: CommandCodeSourceOptions,
}

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

impl CommandCodeSource {
    pub fn new(opts: CommandCodeSourceOptions) -> Self {
        Self { opts }
    }

    fn name(&self) -> &'static str {
        "command-code"
    }
}

#[async_trait]
impl DataSource for CommandCodeSource {
    fn name(&self) -> &'static str {
        "command-code"
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let o = &self.opts;
        let base_dir = match &o.base_dir {
            Some(d) => d.clone(),
            None => match env::var("COMMANDCODE_HOME") {
                Ok(v) if !v.is_empty() => PathBuf::from(v),
                _ => dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join(".commandcode"),
            },
        };
        let projects_dir = base_dir.join("projects");

        let now = ch_now();
        let mut events: Vec<EventRow> = Vec::new();

        // A machine without Command Code is normal, not an error.
        if !projects_dir.is_dir() {
            if o.verbose {
                eprintln!(
                    "commandcode projects directory not found: {}",
                    projects_dir.display()
                );
            }
            return Ok(SourceResult {
                source_name: self.name().to_string(),
                data: EventsSnapshotData { events },
                fetched_at: now,
                error: None,
            });
        }

        let (since, until) = match window(o.since.as_deref(), o.end_date.as_deref(), o.days_back) {
            Some(w) => w,
            None => {
                return Ok(SourceResult {
                    source_name: self.name().to_string(),
                    data: EventsSnapshotData { events },
                    fetched_at: now,
                    error: None,
                })
            }
        };

        let mut files = Vec::new();
        collect_jsonl(&projects_dir, &mut files, 0);

        // A forked or cloned transcript repeats its parent's entries, so the
        // entry id — not the file — is the identity.
        let mut seen_entries: HashSet<String> = HashSet::new();
        let mut turns: Vec<Turn> = Vec::new();
        let mut last_model: Option<String> = None;

        for path in &files {
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            read_session(&text, path, &mut seen_entries, &mut last_model, &mut turns);
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
                "command-code: {} files, {} turns, {} rows",
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

/// Transcripts sit one directory deep (`projects/<slug>/<id>.jsonl`).
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
    last_model: &mut Option<String>,
    turns: &mut Vec<Turn>,
) {
    let mut session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut cwd = String::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match entry.get("type").and_then(|v| v.as_str()) {
            Some("session") => {
                if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
                    session_id = id.to_string();
                }
                // The directory is a lossy slug of cwd; the header has the
                // real path, so it is read rather than inverted.
                if let Some(c) = entry.get("cwd").and_then(|v| v.as_str()) {
                    cwd = c.to_string();
                }
                continue;
            }
            Some("message") => {}
            _ => continue,
        }

        let Some(msg) = entry.get("message") else {
            continue;
        };
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(usage) = msg.get("usage") else {
            continue;
        };

        // A forked or cloned transcript repeats the parent's entries verbatim.
        let identity = entry
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                format!(
                    "{}@{}",
                    path.display(),
                    entry.get("timestamp").and_then(|v| v.as_str()).unwrap_or("")
                )
            });
        if !seen.insert(identity) {
            continue;
        }

        let input = num(usage, "inputTokens");
        let output = num(usage, "outputTokens");
        let cache_read = num(usage, "cacheReadTokens");
        let cache_write = num(usage, "cacheWriteTokens");
        let total = total_tokens(input, output, cache_write, cache_read);
        if total == 0 {
            continue;
        }

        if let Some(m) = msg.get("model").and_then(|v| v.as_str()) {
            *last_model = Some(m.to_string());
        }
        let model = msg
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| last_model.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // `costUsd` is the CLI's own local estimate, so it is preferred but
        // still sanity-checked by resolve_reported_cost.
        let cost = resolve_reported_cost(
            &model,
            usage.get("costUsd").and_then(|v| v.as_f64()).unwrap_or(0.0),
            input,
            cache_read,
            cache_write,
            output,
        );

        let ts = entry.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let (date, start_time) = normalize_timestamp(ts);

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

fn build_rows(turns: &[Turn], o: &CommandCodeSourceOptions, now: &str) -> Vec<EventRow> {
    let mut events = Vec::new();
    let mut by_session: HashMap<(String, String, String), Acc> = HashMap::new();
    let mut by_day: HashMap<(String, String), Acc> = HashMap::new();

    for t in turns {
        by_session
            .entry((t.session_id.clone(), t.model.clone(), t.date.clone()))
            .or_insert_with(|| Acc {
                cwd: t.cwd.clone(),
                ..Default::default()
            })
            .add(t);
        by_day
            .entry((t.date.clone(), t.model.clone()))
            .or_insert_with(|| Acc {
                cwd: t.cwd.clone(),
                ..Default::default()
            })
            .add(t);
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
            source: "command-code".to_string(),
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
                "command-code|{}|session|{}|{}|{}",
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
            source: "command-code".to_string(),
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
                "command-code|{}|daily|{}|{}|{}",
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

    fn opts(dir: &Path) -> CommandCodeSourceOptions {
        CommandCodeSourceOptions {
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

    /// Write a transcript using the shapes taken from the shipped CLI.
    fn write_session(dir: &Path, slug: &str, name: &str, body: &str) {
        let d = dir.join("projects").join(slug);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), body).unwrap();
    }

    const HEADER: &str = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-03-01T09:00:00.000Z","cwd":"/Users/me/app"}"#;

    fn turn(id: &str, ts: &str, model: &str, i: u64, o: u64, cr: u64, cw: u64) -> String {
        // Built with json! rather than a format string: counting literal
        // braces in `format!` is unreadable and off-by-one silently produces
        // invalid JSON, which the parser then skips as a malformed line.
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": serde_json::Value::Null,
            "timestamp": ts,
            "message": {
                "role": "assistant",
                "model": model,
                "usage": {
                    "inputTokens": i,
                    "outputTokens": o,
                    "cacheReadTokens": cr,
                    "cacheWriteTokens": cw,
                },
            },
        })
        .to_string()
    }

    #[test]
    fn name_is_command_code() {
        let src = CommandCodeSource::new(opts(Path::new("/tmp")));
        assert_eq!(src.name(), "command-code");
    }

    #[test]
    fn missing_projects_dir_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let src = CommandCodeSource::new(opts(&tmp.path().join("nope")));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert!(r.error.is_none());
        assert!(r.data.events.is_empty());
    }

    #[test]
    fn assistant_usage_maps_to_four_token_columns() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-Users-me-app",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{}\n",
                turn("m1", "2026-03-01T09:00:05.000Z", "kimi-k2", 100, 20, 50, 10)
            ),
        );
        let src = CommandCodeSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let s: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "session")
            .collect();
        assert_eq!(s.len(), 1);
        let e = s[0];
        assert_eq!(e.source, "command-code");
        assert_eq!(e.input_tokens, 100);
        assert_eq!(e.output_tokens, 20);
        assert_eq!(e.cache_read_tokens, 50);
        assert_eq!(e.cache_creation_tokens, 10);
        // inputTokens is non-cached, so cache counts once: 100+20+10+50.
        assert_eq!(e.total_tokens, 180);
        assert_eq!(e.model_name, "kimi-k2");
        // cwd comes from the header, not the lossy directory slug.
        assert_eq!(e.project_path, "/Users/me/app");
        assert_eq!(e.date, "2026-03-01");
        assert_eq!(e.entries, 1);
    }

    #[test]
    fn cost_usd_is_used_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-p",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{}\n",
                r#"{"type":"message","id":"m1","timestamp":"2026-03-01T09:00:05.000Z","message":{"role":"assistant","model":"kimi-k2","usage":{"inputTokens":1000,"outputTokens":200,"cacheReadTokens":0,"cacheWriteTokens":0,"costUsd":0.42}}}"#
            ),
        );
        let src = CommandCodeSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let s = r
            .data
            .events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert!((s.cost - 0.42).abs() < 0.01, "cost was {}", s.cost);
    }

    #[test]
    fn forked_transcript_does_not_double_count_the_shared_prefix() {
        // `/fork` writes a new file containing a copy of the source entries and
        // a header whose parentSession points at the original.
        let tmp = tempfile::tempdir().unwrap();
        let shared = turn("shared", "2026-03-01T09:00:05.000Z", "kimi-k2", 1000, 100, 0, 0);
        write_session(
            tmp.path(),
            "-p",
            "orig.jsonl",
            &format!("{HEADER}\n{shared}\n"),
        );
        write_session(
            tmp.path(),
            "-p",
            "fork.jsonl",
            &format!(
                "{}\n{}\n{}\n",
                r#"{"type":"session","version":3,"id":"s2","timestamp":"2026-03-01T09:00:00.000Z","cwd":"/Users/me/app","parentSession":"/Users/me/.commandcode/projects/-p/orig.jsonl"}"#,
                shared,
                turn("fork-only", "2026-03-01T09:05:00.000Z", "kimi-k2", 50, 5, 0, 0),
            ),
        );

        let src = CommandCodeSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let d: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .collect();
        assert_eq!(d.len(), 1);
        // 1000 + 50: the shared turn counted once.
        assert_eq!(d[0].input_tokens, 1050, "fork shared a turn with its parent");
        assert_eq!(d[0].output_tokens, 105);
        assert_eq!(d[0].total_tokens, 1155);
    }

    #[test]
    fn non_assistant_messages_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-p",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{}\n{}\n",
                r#"{"type":"message","id":"u1","timestamp":"2026-03-01T09:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
                turn("a1", "2026-03-01T09:00:05.000Z", "kimi-k2", 10, 2, 0, 0),
            ),
        );
        let src = CommandCodeSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let s: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "session")
            .collect();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].input_tokens, 10, "user turns are not billed");
    }

    #[test]
    fn zero_token_turns_produce_no_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-p",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{}\n",
                turn("m1", "2026-03-01T09:00:05.000Z", "kimi-k2", 0, 0, 0, 0)
            ),
        );
        let src = CommandCodeSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert!(r.data.events.is_empty());
    }

    #[test]
    fn model_breakdown_produces_one_row_per_model() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-p",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{}\n{}\n",
                turn("a", "2026-03-01T09:00:05.000Z", "kimi-k2", 10, 1, 0, 0),
                turn("b", "2026-03-01T09:01:05.000Z", "deepseek-v4", 20, 2, 0, 0),
            ),
        );
        let src = CommandCodeSource::new(opts(tmp.path()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let d: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .collect();
        assert_eq!(d.len(), 2, "each model gets its own row");
    }

    #[test]
    fn since_filters_older_days() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-p",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{}\n{}\n",
                turn("old", "2026-03-01T09:00:05.000Z", "kimi-k2", 5, 1, 0, 0),
                turn("new", "2026-06-01T09:00:05.000Z", "kimi-k2", 7, 2, 0, 0),
            ),
        );
        let mut o = opts(tmp.path());
        o.since = Some("2026-05-01".into());
        let src = CommandCodeSource::new(o);
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
    fn malformed_lines_are_skipped_without_failing_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-p",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{{ not json\n\n{}\n",
                turn("a", "2026-03-01T09:00:05.000Z", "kimi-k2", 10, 1, 0, 0)
            ),
        );
        let src = CommandCodeSource::new(opts(tmp.path()));
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
    fn hashed_projects_hide_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            tmp.path(),
            "-p",
            "s1.jsonl",
            &format!(
                "{HEADER}\n{}\n",
                turn("a", "2026-03-01T09:00:05.000Z", "kimi-k2", 10, 1, 0, 0)
            ),
        );
        let mut o = opts(tmp.path());
        o.hash_projects = true;
        let src = CommandCodeSource::new(o);
        let r = futures::executor::block_on(src.fetch()).unwrap();
        let s = r
            .data
            .events
            .iter()
            .find(|e| e.record_type == "session")
            .unwrap();
        assert_eq!(s.project_path.len(), 8);
        assert!(!s.project_path.contains("Users"));
    }
}
