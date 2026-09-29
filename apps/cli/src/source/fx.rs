/**
 * fx (Vercel Labs) Source
 *
 * Reads fx's local usage ledger at `~/.fx/usage.jsonl` (override: `FX_HOME`).
 * fx keeps this ledger itself: `fx usage` and `/usage` are computed from it, so
 * the numbers here are the same ones fx shows the user.
 *
 * The file is JSON Lines. Every line is a record tagged by `kind`:
 *   - `generation` -> `fact`: one settled model request, the only billable kind
 *   - `coverage`   -> when tracking started; no tokens
 *   - `pending`    -> a request awaiting usage data from the provider
 *   - `incident`   -> a coverage gap marker; no tokens
 *
 * `pending` and `incident` are deliberately NOT imported: a pending request has
 * no token counts yet, and an incident only records that something was lost.
 * Emitting rows for either would fabricate spend.
 *
 * Records are appended and compacted, so a stable read is the last complete
 * line. A torn final line is dropped rather than guessed at.
 */

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use crate::model::{DataSource, EventRow, EventsSnapshotData, SourceResult};
use crate::util::date::ch_now;
use crate::util::hash::{hash_project_name_sync, make_dedup_key};
use crate::util::pricing::resolve_reported_cost;
use crate::util::tokens::total_tokens;

#[derive(Debug, Clone)]
pub struct FxSourceOptions {
    pub machine_name: String,
    pub hash_projects: bool,
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Override fx's home (tests). When None, uses `FX_HOME` or `~/.fx`.
    pub base_dir: Option<PathBuf>,
}

pub struct FxSource {
    opts: FxSourceOptions,
}

struct Request {
    date: String,
    start_time: String,
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

impl FxSource {
    pub fn new(opts: FxSourceOptions) -> Self {
        Self { opts }
    }

    fn name(&self) -> &'static str {
        "fx"
    }
}

#[async_trait]
impl DataSource for FxSource {
    fn name(&self) -> &'static str {
        "fx"
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let o = &self.opts;
        let base_dir = match &o.base_dir {
            Some(d) => d.clone(),
            None => match env::var("FX_HOME") {
                Ok(v) if !v.is_empty() => PathBuf::from(v),
                _ => dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join(".fx"),
            },
        };
        let usage_path = base_dir.join("usage.jsonl");

        let now = ch_now();
        let mut events: Vec<EventRow> = Vec::new();

        // No fx on this machine is normal, not an error.
        if !usage_path.is_file() {
            if o.verbose {
                eprintln!("fx usage ledger not found: {}", usage_path.display());
            }
            return Ok(SourceResult {
                source_name: self.name().to_string(),
                data: EventsSnapshotData { events },
                fetched_at: now,
                error: None,
            });
        }

        let Ok(text) = fs::read_to_string(&usage_path) else {
            return Ok(SourceResult {
                source_name: self.name().to_string(),
                data: EventsSnapshotData { events },
                fetched_at: now,
                error: None,
            });
        };

        let (since, until) = window(o.since.as_deref(), o.end_date.as_deref(), o.days_back);

        let mut requests: Vec<Request> = Vec::new();
        for line in complete_lines(&text) {
            let Some(req) = parse_line(line) else {
                continue;
            };
            if let Some(s) = &since {
                if &req.date < s {
                    continue;
                }
            }
            if let Some(u) = &until {
                if &req.date > u {
                    continue;
                }
            }
            requests.push(req);
        }

        events = build_rows(&requests, o, &now);

        if o.verbose {
            eprintln!(
                "fx: {} settled requests, {} rows",
                requests.len(),
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

/// Only whole lines are safe to read. fx appends one record per line and
/// compacts in place, so a torn tail is a partial write, not a record.
fn complete_lines(text: &str) -> impl Iterator<Item = &str> {
    let body = match text.rfind('\n') {
        Some(i) => &text[..=i],
        None => "",
    };
    body.lines().filter(|l| !l.trim().is_empty())
}

fn window(
    since: Option<&str>,
    end_date: Option<&str>,
    days_back: Option<i64>,
) -> (Option<String>, Option<String>) {
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
            return (None, None);
        }
    }
    if let Some(e) = end_date {
        if chrono::NaiveDate::parse_from_str(e, "%Y-%m-%d").is_err() {
            return (None, None);
        }
    }
    (since, end_date.map(|e| e.to_string()))
}

fn parse_line(line: &str) -> Option<Request> {
    let entry: Value = serde_json::from_str(line).ok()?;
    if entry.get("schema_version").and_then(|v| v.as_i64()) != Some(1) {
        return None;
    }
    if entry.get("kind").and_then(|v| v.as_str()) != Some("generation") {
        return None;
    }
    let fact = entry.get("fact")?;
    if fact.get("id").and_then(|v| v.as_str()).is_none() {
        return None;
    }

    let input = num(fact, "input_tokens");
    let output = num(fact, "output_tokens");
    let cache_read = num(fact, "cache_read_tokens");
    let cache_write = num(fact, "cache_write_tokens");
    // fx reports input/output/cache separately, so the total is the four-term
    // sum. `total_tokens` is absent from the fact on purpose.
    let total = total_tokens(input, output, cache_write, cache_read);
    if total == 0 {
        return None;
    }

    let model = fact.get("model").and_then(|v| v.as_str()).unwrap_or("unknown");
    let cost = resolve_reported_cost(
        model,
        fact.get("total_cost").and_then(|v| v.as_f64()).unwrap_or(0.0),
        input,
        cache_read,
        cache_write,
        output,
    );

    let ms = fact.get("created_at_ms").and_then(|v| v.as_i64()).unwrap_or(0);
    let (date, start_time) = match chrono::DateTime::from_timestamp_millis(ms) {
        Some(dt) => (
            dt.format("%Y-%m-%d").to_string(),
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        ),
        None => (String::new(), String::new()),
    };
    if date.is_empty() {
        return None;
    }

    Some(Request {
        date,
        start_time,
        model: model.to_string(),
        input,
        output,
        cache_read,
        cache_write,
        cost,
    })
}

fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

#[derive(Default)]
struct Acc {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    entries: u32,
    first_start: Option<String>,
}

impl Acc {
    fn add(&mut self, r: &Request) {
        self.input += r.input;
        self.output += r.output;
        self.cache_read += r.cache_read;
        self.cache_write += r.cache_write;
        self.cost += r.cost;
        self.entries = self.entries.saturating_add(1);
        if self.first_start.is_none() {
            self.first_start = Some(r.start_time.clone());
        }
    }

    fn total(&self) -> u64 {
        total_tokens(self.input, self.output, self.cache_write, self.cache_read)
    }
}

fn build_rows(requests: &[Request], o: &FxSourceOptions, now: &str) -> Vec<EventRow> {
    let mut events = Vec::new();
    let mut by_day: HashMap<(String, String), Acc> = HashMap::new();
    for r in requests {
        by_day
            .entry((r.date.clone(), r.model.clone()))
            .or_default()
            .add(r);
    }

    for ((date, model), acc) in &by_day {
        let project = hash_project_name_sync("fx", o.hash_projects);
        events.push(EventRow {
            date: date.clone(),
            record_type: "daily".to_string(),
            record_key: date.clone(),
            source: "fx".to_string(),
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
                "fx|{}|daily|{}|{}|{}",
                o.machine_name, date, model, date
            )),
            import_id: o.import_id.clone(),
            start_time: acc.first_start.clone(),
            entries: acc.entries,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            ..EventRow::default()
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &PathBuf) -> FxSourceOptions {
        FxSourceOptions {
            machine_name: "test-machine".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "import-1".into(),
            base_dir: Some(dir.clone()),
        }
    }

    /// Build a ledger using fx's real `usage.jsonl` record shapes.
    fn ledger(generation_facts: &[(&str, u64, u64, u64, u64, f64)], trailing_partial: bool) -> String {
        let mut s = String::from(
            "{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":1700000000000}\n",
        );
        for (id, ms, i, o, cr, cost) in generation_facts {
            s.push_str(&format!(
                "{{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{{\"id\":\"{id}\",\"created_at_ms\":{ms},\"model\":\"anthropic/claude-sonnet-4-5\",\"input_tokens\":{i},\"output_tokens\":{o},\"cache_read_tokens\":{cr},\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"billable_web_search_calls\":0,\"total_cost\":{cost}}}}}\n"
            ));
        }
        if trailing_partial {
            s.push_str("{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{\"id\":\"torn\"");
        }
        s
    }

    fn write(base: &PathBuf, body: &str) {
        fs::create_dir_all(base).unwrap();
        fs::write(base.join("usage.jsonl"), body).unwrap();
    }

    #[test]
    fn name_is_fx() {
        let src = FxSource::new(opts(&PathBuf::from("/tmp")));
        assert_eq!(src.name(), "fx");
    }

    #[test]
    fn missing_ledger_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let src = FxSource::new(opts(&tmp.path().join("nope")));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert!(r.error.is_none());
        assert!(r.data.events.is_empty());
    }

    #[test]
    fn generation_fact_becomes_a_daily_row() {
        let tmp = tempfile::tempdir().unwrap();
        // 2026-01-15T00:00:00Z
        write(
            &tmp.path().to_path_buf(),
            &ledger(&[("gen_a", 1768435200000, 100, 20, 30, 0.25)], false),
        );
        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 1);
        let d = &r.data.events[0];
        assert_eq!(d.source, "fx");
        assert_eq!(d.record_type, "daily");
        assert_eq!(d.date, "2026-01-15");
        assert_eq!(d.input_tokens, 100);
        assert_eq!(d.output_tokens, 20);
        assert_eq!(d.cache_read_tokens, 30);
        assert_eq!(d.cache_creation_tokens, 0);
        // 100 + 20 + 30 cache: cache counts once, not double.
        assert_eq!(d.total_tokens, 150);
        assert_eq!(d.model_name, "anthropic/claude-sonnet-4-5");
        assert!((d.cost - 0.25).abs() < 0.01, "cost was {}", d.cost);
    }

    #[test]
    fn pending_and_incident_records_never_become_rows() {
        // Both describe requests with no settled token counts. Billing them
        // would invent spend that never happened.
        let tmp = tempfile::tempdir().unwrap();
        let mut body = ledger(&[("gen_a", 1768435200000, 10, 2, 0, 0.01)], false);
        body.push_str("{\"schema_version\":1,\"kind\":\"pending\",\"id\":\"gen_b\",\"observed_at_ms\":1768435201000}\n");
        body.push_str("{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":1768435202000,\"completeness\":\"incomplete\"}\n");
        write(&tmp.path().to_path_buf(), &body);

        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 1);
        assert_eq!(r.data.events[0].input_tokens, 10);
    }

    #[test]
    fn torn_final_line_is_dropped() {
        // fx appends whole lines; a partial tail is an interrupted write.
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().to_path_buf(),
            &ledger(&[("gen_a", 1768435200000, 10, 2, 0, 0.01)], true),
        );
        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 1);
        assert_eq!(r.data.events[0].input_tokens, 10, "torn line must not be parsed");
    }

    #[test]
    fn multiple_requests_aggregate_per_day_and_model() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().to_path_buf(),
            &ledger(
                &[
                    ("gen_a", 1768435200000, 10, 2, 1, 0.01),
                    ("gen_b", 1768435260000, 20, 4, 2, 0.02),
                ],
                false,
            ),
        );
        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 1);
        let d = &r.data.events[0];
        assert_eq!(d.input_tokens, 30);
        assert_eq!(d.output_tokens, 6);
        assert_eq!(d.cache_read_tokens, 3);
        assert_eq!(d.entries, 2);
    }

    #[test]
    fn distinct_days_do_not_merge() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().to_path_buf(),
            &ledger(
                &[
                    ("gen_a", 1768435200000, 10, 2, 0, 0.01),
                    ("gen_b", 1768521600000, 30, 5, 0, 0.03),
                ],
                false,
            ),
        );
        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 2);
        let dates: std::collections::HashSet<_> =
            r.data.events.iter().map(|e| e.date.clone()).collect();
        assert_eq!(dates.len(), 2);
    }

    #[test]
    fn zero_token_fact_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().to_path_buf(),
            &ledger(&[("gen_a", 1768435200000, 0, 0, 0, 0.0)], false),
        );
        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert!(r.data.events.is_empty());
    }

    #[test]
    fn since_filters_older_days() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().to_path_buf(),
            &ledger(
                &[
                    ("gen_a", 1768435200000, 10, 2, 0, 0.01), // 2026-01-15
                    ("gen_b", 1800000000000, 99, 9, 0, 0.09), // 2027-01-15
                ],
                false,
            ),
        );
        let mut o = opts(&tmp.path().to_path_buf());
        o.since = Some("2026-06-01".into());
        let src = FxSource::new(o);
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 1);
        assert_eq!(r.data.events[0].date, "2027-01-15");
    }

    #[test]
    fn unknown_schema_version_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = ledger(&[("gen_a", 1768435200000, 10, 2, 0, 0.01)], false);
        body.push_str("{\"schema_version\":2,\"kind\":\"generation\",\"fact\":{\"id\":\"future\",\"created_at_ms\":1768435200000,\"model\":\"m\",\"input_tokens\":999,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"total_cost\":9.99}}\n");
        write(&tmp.path().to_path_buf(), &body);
        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 1);
        assert_eq!(r.data.events[0].input_tokens, 10, "v2 records must not be guessed at");
    }

    #[test]
    fn malformed_lines_do_not_drop_valid_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = ledger(&[("gen_a", 1768435200000, 10, 2, 0, 0.01)], false);
        body.push_str("not json at all\n");
        write(&tmp.path().to_path_buf(), &body);
        let src = FxSource::new(opts(&tmp.path().to_path_buf()));
        let r = futures::executor::block_on(src.fetch()).unwrap();
        assert_eq!(r.data.events.len(), 1);
    }

    #[test]
    fn gateway_model_names_resolve_through_the_model_part() {
        // fx stores "provider/model"; pricing must still see "claude".
        let r = crate::util::pricing::rates_for_model("anthropic/claude-sonnet-4-5");
        assert!(r.output > 0.0, "gateway-prefixed names must not fall to free");
    }
}
