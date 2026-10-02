//! Telemetry ingest + analytics for `summa serve`.
//!
//! Machines POST events; this process dedups via `dedup_key` and fans out
//! to MotherDuck and ClickHouse. burn.duyet.net pulls `/v1/analytics`.

use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::model::EventRow;
use crate::util::date::ch_now;
use crate::util::hash::make_dedup_key;

pub mod client;

pub const DEFAULT_BIND: &str = "127.0.0.1:8787";
pub const DEFAULT_ANALYTICS_DAYS: i64 = 30;
pub const CORS_BURN: &str = "https://burn.duyet.net";
pub use client::{TelemetrySink, DEFAULT_ENDPOINT};

#[derive(Debug, Clone, Deserialize)]
pub struct IngestBody {
    #[serde(default)]
    pub events: Vec<EventRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngestResponse {
    pub accepted: usize,
    /// Rows the hub dropped before storing (bad date, empty record_type, …).
    /// `accepted` counts what it parsed, so without this the shortfall is
    /// invisible.
    #[serde(default)]
    pub rejected: u64,
    pub sinks: Vec<SinkAck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SinkAck {
    pub name: String,
    pub rows: u64,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HealthBody {
    pub ok: bool,
    pub service: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct StatusBody {
    pub ok: bool,
    pub bind: String,
    pub last_ingest_at: Option<String>,
    pub last_accepted: u64,
    pub ping: Vec<PingSample>,
    pub sinks: Vec<SinkAck>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PingSample {
    pub name: String,
    pub ok: bool,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnalyticsPoint {
    pub date: String,
    pub source: String,
    pub model_name: String,
    pub cost: f64,
    pub total_tokens: u64,
    pub entries: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnalyticsBody {
    pub since: String,
    pub until: String,
    pub group: String,
    pub points: Vec<AnalyticsPoint>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SourceTotal {
    pub source: String,
    pub cost: f64,
    pub total_tokens: u64,
    pub entries: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnalyticsSummary {
    pub since: String,
    pub until: String,
    pub days: i64,
    pub cost: f64,
    pub total_tokens: u64,
    pub entries: u64,
    /// Mean daily cost over the inclusive window (zero if days < 1).
    pub cost_per_day: f64,
    pub by_source: Vec<SourceTotal>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PingBody {
    pub ok: bool,
    pub samples: Vec<PingSample>,
}

/// Fill missing dedup_key / timestamps so multi-host Cursor/Grok rows collapse.
pub fn prepare_events(mut events: Vec<EventRow>) -> Vec<EventRow> {
    let now = ch_now();
    for e in &mut events {
        if e.dedup_key.is_empty() {
            e.dedup_key = make_dedup_key(&format!(
                "{}|{}|{}|{}|{}|{}|{}",
                e.account_id, e.source, e.machine_name, e.record_type, e.date, e.model_name,
                e.record_key
            ));
        }
        if e.created_at.is_empty() {
            e.created_at = now.clone();
        }
        if e.updated_at.is_empty() {
            e.updated_at = now.clone();
        }
    }
    events
}

pub fn analytics_sql(group: &str) -> &'static str {
    match group {
        "model" => {
            "SELECT CAST(date AS VARCHAR), source, model_name, \
             COALESCE(sum(cost), 0), COALESCE(sum(total_tokens), 0), COALESCE(sum(entries), 0) \
             FROM ccusage_events \
             WHERE record_type = 'daily' AND date >= ? AND date <= ? \
             GROUP BY 1, 2, 3 ORDER BY 1, 2, 3"
        }
        _ => {
            "SELECT CAST(date AS VARCHAR), source, '' AS model_name, \
             COALESCE(sum(cost), 0), COALESCE(sum(total_tokens), 0), COALESCE(sum(entries), 0) \
             FROM ccusage_events \
             WHERE record_type = 'daily' AND date >= ? AND date <= ? \
             GROUP BY 1, 2 ORDER BY 1, 2"
        }
    }
}

pub fn clickhouse_analytics_sql(group: &str) -> String {
    let extra = if group == "model" {
        "source, model_name"
    } else {
        "source, '' AS model_name"
    };
    format!(
        "SELECT date, {extra}, \
         sum(cost) AS cost, sum(total_tokens) AS total_tokens, sum(entries) AS entries \
         FROM ccusage_events FINAL \
         WHERE record_type = 'daily' AND date >= '{{since}}' AND date <= '{{until}}' \
         GROUP BY date, source{} \
         ORDER BY date, source",
        if group == "model" { ", model_name" } else { "" }
    )
}

pub fn bearer_ok(expected: &str, header: Option<&str>, alt: Option<&str>) -> bool {
    if expected.is_empty() {
        return true;
    }
    if let Some(h) = header {
        let h = h.trim();
        if let Some(t) = h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")) {
            return t.trim() == expected;
        }
        if h == expected {
            return true;
        }
    }
    alt.map(|t| t.trim() == expected).unwrap_or(false)
}

/// Inclusive `[since, until]` window as `YYYY-MM-DD`.
/// Prefer explicit dates; else last `days` (default 30) ending today UTC.
pub fn analytics_window(
    since: Option<&str>,
    until: Option<&str>,
    days: Option<i64>,
) -> anyhow::Result<(String, String)> {
    let today = Utc::now().date_naive();
    let until_d = match until.filter(|s| !s.is_empty()) {
        Some(s) => parse_iso_date(s)?,
        None => today,
    };
    let since_d = match since.filter(|s| !s.is_empty()) {
        Some(s) => parse_iso_date(s)?,
        None => {
            let n = days.unwrap_or(DEFAULT_ANALYTICS_DAYS).max(1);
            until_d - Duration::days(n - 1)
        }
    };
    if since_d > until_d {
        anyhow::bail!("since ({since_d}) is after until ({until_d})");
    }
    Ok((
        since_d.format("%Y-%m-%d").to_string(),
        until_d.format("%Y-%m-%d").to_string(),
    ))
}

pub fn parse_iso_date(s: &str) -> anyhow::Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("invalid date `{s}` (want YYYY-MM-DD)"))
}

pub fn valid_iso_date(s: &str) -> bool {
    parse_iso_date(s).is_ok()
}

pub fn inclusive_days(since: &str, until: &str) -> i64 {
    match (parse_iso_date(since), parse_iso_date(until)) {
        (Ok(a), Ok(b)) => (b - a).num_days().max(0) + 1,
        _ => 1,
    }
}

/// Roll daily points into totals + per-source burn. Cost/day uses the
/// inclusive calendar window, not the count of days that have rows.
pub fn summarize_points(since: &str, until: &str, points: &[AnalyticsPoint]) -> AnalyticsSummary {
    let days = inclusive_days(since, until);
    let mut cost = 0.0;
    let mut total_tokens: u64 = 0;
    let mut entries: u64 = 0;
    let mut by: Vec<SourceTotal> = Vec::new();
    for p in points {
        cost += p.cost;
        total_tokens = total_tokens.saturating_add(p.total_tokens);
        entries = entries.saturating_add(p.entries);
        if let Some(row) = by.iter_mut().find(|s| s.source == p.source) {
            row.cost += p.cost;
            row.total_tokens = row.total_tokens.saturating_add(p.total_tokens);
            row.entries = row.entries.saturating_add(p.entries);
        } else {
            by.push(SourceTotal {
                source: p.source.clone(),
                cost: p.cost,
                total_tokens: p.total_tokens,
                entries: p.entries,
            });
        }
    }
    by.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));
    AnalyticsSummary {
        since: since.to_string(),
        until: until.to_string(),
        days,
        cost,
        total_tokens,
        entries,
        cost_per_day: if days > 0 { cost / days as f64 } else { 0.0 },
        by_source: by,
    }
}

pub fn ping_ok(samples: &[PingSample]) -> bool {
    !samples.is_empty() && samples.iter().all(|s| s.ok)
}

/// Origins burn.duyet.net and local dev may call the analytics API from a browser.
pub fn cors_allow_origin(origin: Option<&str>) -> Option<String> {
    let Some(o) = origin.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some("*".into());
    };
    if o == CORS_BURN {
        return Some(o.to_string());
    }
    if let Some(rest) = o.strip_prefix("http://localhost") {
        if rest.is_empty() || rest.starts_with(':') || rest.starts_with('/') {
            return Some(o.to_string());
        }
    }
    if let Some(rest) = o.strip_prefix("http://127.0.0.1") {
        if rest.is_empty() || rest.starts_with(':') || rest.starts_with('/') {
            return Some(o.to_string());
        }
    }
    None
}

pub fn clickhouse_analytics_query(group: &str, since: &str, until: &str) -> anyhow::Result<String> {
    if !valid_iso_date(since) || !valid_iso_date(until) {
        anyhow::bail!("analytics dates must be YYYY-MM-DD");
    }
    let sql = clickhouse_analytics_sql(group)
        .replace("{since}", since)
        .replace("{until}", until);
    Ok(format!("{sql} FORMAT JSONEachRow"))
}

/// HTTP 200 if ≥1 sink wrote without error; 502 if every configured sink
/// failed; 503 if none were configured.
pub fn ingest_status_code(sinks: &[SinkAck]) -> u16 {
    if sinks.is_empty() {
        return 503;
    }
    if sinks.iter().any(|s| s.error.is_none()) {
        200
    } else {
        502
    }
}

pub fn sidebar_html(bind: &str, version: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8>\
         <title>summa</title>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <style>body{{font:14px/1.4 system-ui,sans-serif;margin:12px;color:#111;background:#fafafa}}\
         h1{{font-size:16px;margin:0 0 8px}}code{{font-size:12px}}</style></head>\
         <body><h1>summa telemetry</h1>\
         <p>v{version} · {bind}</p>\
         <p><a href=/health>health</a> · <a href=/ping>ping</a> · <a href=/status>status</a></p>\
         <p>POST /v1/ingest · GET /v1/analytics</p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_fills_dedup_and_timestamps() {
        let mut row = EventRow::default();
        row.source = "cursor".into();
        row.machine_name = "account".into();
        row.record_type = "daily".into();
        row.date = "2026-08-19".into();
        row.model_name = "grok-4.5".into();
        row.record_key = "2026-08-19".into();
        let out = prepare_events(vec![row.clone(), row]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].dedup_key.len(), 16);
        assert_eq!(out[0].dedup_key, out[1].dedup_key);
        assert!(!out[0].created_at.is_empty());
    }

    #[test]
    fn bearer_optional_when_empty() {
        assert!(bearer_ok("", None, None));
        assert!(!bearer_ok("secret", None, None));
        assert!(bearer_ok("secret", Some("Bearer secret"), None));
        assert!(bearer_ok("secret", None, Some("secret")));
        assert!(!bearer_ok("secret", Some("Bearer other"), None));
    }

    #[test]
    fn analytics_sql_groups() {
        assert!(analytics_sql("source").contains("GROUP BY 1, 2"));
        assert!(analytics_sql("model").contains("GROUP BY 1, 2, 3"));
        let ch = clickhouse_analytics_sql("source");
        assert!(ch.contains("FINAL"));
        assert!(ch.contains("{since}"));
    }

    #[test]
    fn window_defaults_inclusive_days() {
        let (since, until) = analytics_window(None, Some("2026-08-20"), Some(7)).unwrap();
        assert_eq!(until, "2026-08-20");
        assert_eq!(since, "2026-08-14");
        assert_eq!(inclusive_days(&since, &until), 7);
    }

    #[test]
    fn window_rejects_inverted_range() {
        assert!(analytics_window(Some("2026-08-20"), Some("2026-08-01"), None).is_err());
        assert!(!valid_iso_date("08-20-2026"));
        assert!(valid_iso_date("2026-08-20"));
    }

    #[test]
    fn summarize_uses_calendar_days_not_row_days() {
        let points = vec![AnalyticsPoint {
            date: "2026-08-20".into(),
            source: "cursor".into(),
            model_name: String::new(),
            cost: 14.0,
            total_tokens: 100,
            entries: 2,
        }];
        let s = summarize_points("2026-08-14", "2026-08-20", &points);
        assert_eq!(s.days, 7);
        assert!((s.cost_per_day - 2.0).abs() < 1e-9);
        assert_eq!(s.by_source.len(), 1);
        assert_eq!(s.by_source[0].source, "cursor");
    }

    #[test]
    fn ingest_http_codes() {
        assert_eq!(ingest_status_code(&[]), 503);
        let ok = SinkAck {
            name: "duckdb".into(),
            rows: 1,
            duration_ms: 1,
            error: None,
        };
        let bad = SinkAck {
            name: "clickhouse".into(),
            rows: 0,
            duration_ms: 1,
            error: Some("down".into()),
        };
        assert_eq!(ingest_status_code(&[ok.clone()]), 200);
        assert_eq!(ingest_status_code(&[ok, bad.clone()]), 200);
        assert_eq!(ingest_status_code(&[bad]), 502);
    }

    #[test]
    fn cors_burn_and_localhost() {
        assert_eq!(
            cors_allow_origin(Some("https://burn.duyet.net")).as_deref(),
            Some("https://burn.duyet.net")
        );
        assert_eq!(
            cors_allow_origin(Some("http://localhost:3000")).as_deref(),
            Some("http://localhost:3000")
        );
        assert!(cors_allow_origin(Some("https://evil.example")).is_none());
    }

    #[test]
    fn ping_requires_all_ok() {
        assert!(!ping_ok(&[]));
        assert!(ping_ok(&[PingSample {
            name: "duckdb".into(),
            ok: true,
            latency_ms: 2,
            error: None,
        }]));
        assert!(!ping_ok(&[
            PingSample {
                name: "duckdb".into(),
                ok: true,
                latency_ms: 2,
                error: None,
            },
            PingSample {
                name: "clickhouse".into(),
                ok: false,
                latency_ms: 5,
                error: Some("timeout".into()),
            },
        ]));
    }
}
