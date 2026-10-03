/**
 * Cursor account-wide usage source.
 *
 * Fetches usage events from Cursor’s dashboard / Admin APIs (CodexBar path)
 * and maps them into `ccusage_events` rows covering the **account** (all
 * machines), never this host’s local Cursor files as the identity.
 *
 * Auth (first match wins):
 *   1. `CURSOR_API_KEY` / credentials `cursor_api_key` → Admin
 *      `POST https://api.cursor.com/teams/filtered-usage-events`
 *   2. `CURSOR_SESSION` / `CURSOR_COOKIE` / credentials session → dashboard
 *      `POST https://cursor.com/api/dashboard/get-filtered-usage-events`
 *   3. Cursor.app local JWT in `state.vscdb` (`cursorAuth/accessToken`)
 *
 * Missing credentials skip this source (empty result, rest of import continues).
 *
 * Surface labels (`EventRow.source`):
 *   cursor | cursor-cloud-agent | cursor-api
 * The Grok Bot chat surface is emitted by `source::grok_bot` (`grok-bot`), which
 * reads these same events; see `Ownership`. Residual / unclassifiable events
 * still import as `cursor`.
 */

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;

use crate::model::{DataSource, EventRow, EventsSnapshotData, SourceResult};
use crate::util::date::ch_now;
use crate::util::hash::make_dedup_key;

/// Stable identity for account-wide Cursor rows. Must not be the importer hostname.
pub const CURSOR_ACCOUNT_MACHINE: &str = "account";

pub const SOURCE_CURSOR: &str = "cursor";
pub const SOURCE_CLOUD_AGENT: &str = "cursor-cloud-agent";
pub const SOURCE_API: &str = "cursor-api";
/// Grok Bot (x.ai) chat rows. Written by `source::grok_bot`, not by this
/// source: one usage event must have exactly one owner, and the dedup key
/// contains the source, so emitting it twice would double-count it.
pub const SOURCE_GROK_BOT: &str = "grok-bot";

const DASHBOARD_EVENTS_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const ADMIN_EVENTS_URL: &str = "https://api.cursor.com/teams/filtered-usage-events";
const DEFAULT_PAGE_SIZE: u32 = 1000;
const MAX_PAGES: u32 = 200;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CursorSourceOptions {
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Cookie header or raw WorkosCursorSessionToken value.
    pub session: Option<String>,
    /// Team Admin API key (Basic auth).
    pub api_key: Option<String>,
    /// Override Cursor.app `state.vscdb` path (tests). `None` = platform default.
    pub state_db_path: Option<PathBuf>,
    /// Skip Cursor.app local JWT lookup (tests / missing-auth cases).
    pub disable_local_auth: bool,
}

// ---------------------------------------------------------------------------
// Event JSON (dashboard + Admin share this shape)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorTokenUsage {
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub input_tokens: u64,
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub output_tokens: u64,
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub cache_write_tokens: u64,
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub cache_read_tokens: u64,
    #[serde(default, deserialize_with = "de_opt_f64_flex")]
    pub total_cents: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorUsageEvent {
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    pub timestamp: Option<i64>,
    pub model: Option<String>,
    pub kind: Option<String>,
    pub token_usage: Option<CursorTokenUsage>,
    pub is_headless: Option<bool>,
    pub is_token_based_call: Option<bool>,
    pub is_chargeable: Option<bool>,
    pub cloud_agent_id: Option<String>,
    pub service_account_id: Option<String>,
    pub service_account_name: Option<String>,
    pub conversation_id: Option<String>,
    pub user_email: Option<String>,
    pub owning_user: Option<String>,
    pub owning_team: Option<String>,
    #[serde(default, deserialize_with = "de_opt_f64_flex")]
    pub charged_cents: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_f64_flex")]
    pub requests_costs: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_f64_flex")]
    pub cursor_token_fee: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorUsagePage {
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    total_usage_events_count: Option<i64>,
    #[serde(default)]
    usage_events_display: Vec<CursorUsageEvent>,
    #[serde(default)]
    usage_events: Vec<CursorUsageEvent>,
    pagination: Option<CursorPagination>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorPagination {
    #[serde(default)]
    has_next_page: Option<bool>,
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    num_pages: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    current_page: Option<i64>,
}

impl CursorUsagePage {
    fn events(&self) -> &[CursorUsageEvent] {
        if !self.usage_events_display.is_empty() {
            &self.usage_events_display
        } else {
            &self.usage_events
        }
    }
}

// ---------------------------------------------------------------------------
// Classification + mapping (pure; no HTTP)
// ---------------------------------------------------------------------------

/// Cursor dashboard serializes missing ids as the string `"null"`.
fn present_id(s: &Option<String>) -> bool {
    match s.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => {
            let lower = t.to_ascii_lowercase();
            lower != "null" && lower != "undefined" && lower != "none" && t != "-"
        }
        _ => false,
    }
}

fn haystack(event: &CursorUsageEvent) -> String {
    [
        event.model.as_deref().unwrap_or(""),
        event.kind.as_deref().unwrap_or(""),
        event.service_account_name.as_deref().unwrap_or(""),
        event.service_account_id.as_deref().unwrap_or(""),
        event.owning_user.as_deref().unwrap_or(""),
        event.user_email.as_deref().unwrap_or(""),
    ]
    .join(" ")
    .to_ascii_lowercase()
}

fn is_api_event(event: &CursorUsageEvent) -> bool {
    if present_id(&event.service_account_id) {
        return true;
    }
    let kind = event.kind.as_deref().unwrap_or("").to_ascii_lowercase();
    // "API" / "Usage-based API" / USAGE_EVENT_KIND_*_API vs incidental "token rate"
    kind.contains("api") && !kind.contains("token rate")
}

fn is_cloud_agent_event(event: &CursorUsageEvent) -> bool {
    present_id(&event.cloud_agent_id) || event.is_headless.unwrap_or(false)
}

fn is_grok_bot_event(event: &CursorUsageEvent) -> bool {
    let h = haystack(event);
    if h.contains("grok-bot") || h.contains("grokbot") || h.contains("grok bot") {
        return true;
    }
    event
        .model
        .as_deref()
        .map(|m| m.to_ascii_lowercase().contains("grok"))
        .unwrap_or(false)
}

/// Classify one usage event into a `source` label.
///
/// Priority: API (service-account) → cloud agent (`cloudAgentId` / `isHeadless`)
/// → grok bot (grok-bot signal or grok model) → residual `cursor`.
pub fn classify_cursor_surface(event: &CursorUsageEvent) -> &'static str {
    if is_api_event(event) {
        return SOURCE_API;
    }
    if is_cloud_agent_event(event) {
        return SOURCE_CLOUD_AGENT;
    }
    if is_grok_bot_event(event) {
        return SOURCE_GROK_BOT;
    }
    SOURCE_CURSOR
}

fn event_cost_usd(event: &CursorUsageEvent) -> f64 {
    if let Some(cents) = event.charged_cents.filter(|c| c.is_finite()) {
        return cents / 100.0;
    }
    if let Some(cents) = event
        .token_usage
        .as_ref()
        .and_then(|u| u.total_cents)
        .filter(|c| c.is_finite())
    {
        return cents / 100.0;
    }
    0.0
}

fn tokens_of(event: &CursorUsageEvent) -> (u64, u64, u64, u64, u64) {
    match &event.token_usage {
        Some(u) => {
            let total = u
                .input_tokens
                .saturating_add(u.output_tokens)
                .saturating_add(u.cache_write_tokens)
                .saturating_add(u.cache_read_tokens);
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_write_tokens,
                u.cache_read_tokens,
                total,
            )
        }
        None => (0, 0, 0, 0, 0),
    }
}

fn timestamp_ms(event: &CursorUsageEvent) -> Option<i64> {
    event.timestamp.filter(|t| *t > 0)
}

fn event_datetime(ms: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_millis(ms)
}

fn event_identity(event: &CursorUsageEvent, source: &str) -> String {
    let ts = event.timestamp.unwrap_or(0);
    let conv = event.conversation_id.as_deref().unwrap_or("");
    let model = event.model.as_deref().unwrap_or("");
    let extra = event
        .cloud_agent_id
        .as_deref()
        .or(event.service_account_id.as_deref())
        .unwrap_or("");
    format!("{source}|{ts}|{conv}|{model}|{extra}")
}

/// Parse a dashboard or Admin page JSON into events (either wrapper key).
pub fn events_from_page_json(json: &str) -> anyhow::Result<Vec<CursorUsageEvent>> {
    let page: CursorUsagePage = serde_json::from_str(json)?;
    Ok(page.events().to_vec())
}

/// Map Cursor usage-event JSON (object with `usageEventsDisplay` / `usageEvents`,
/// or a bare array) into EventRows. HTTP-free; used by tests and fetch.
pub fn map_cursor_events_json(
    json: &str,
    import_id: &str,
    since: Option<&str>,
    end_date: Option<&str>,
    now: &str,
) -> anyhow::Result<Vec<EventRow>> {
    let trimmed = json.trim();
    let events: Vec<CursorUsageEvent> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed)?
    } else {
        events_from_page_json(trimmed)?
    };
    Ok(map_cursor_events(&events, import_id, since, end_date, now))
}

pub fn map_cursor_events(
    events: &[CursorUsageEvent],
    import_id: &str,
    since: Option<&str>,
    end_date: Option<&str>,
    now: &str,
) -> Vec<EventRow> {
    map_account_events(events, import_id, since, end_date, now, Ownership::Cursor)
}

/// Grok Bot chat rows out of the same events. Used by `source::grok_bot`.
pub fn map_grok_bot_events(
    events: &[CursorUsageEvent],
    import_id: &str,
    since: Option<&str>,
    end_date: Option<&str>,
    now: &str,
) -> Vec<EventRow> {
    map_account_events(events, import_id, since, end_date, now, Ownership::GrokBot)
}

/// Which side of the usage-event feed a mapper owns.
///
/// The Grok Bot chat surface is a product surface of its own, so it is written
/// as `source = "grok-bot"` instead of a Cursor sub-label — but it is the same
/// underlying event, and dedup keys embed the source, so exactly one mapper may
/// claim it. Writing it from both sides would survive ReplacingMergeTree as two
/// rows and count the same chat twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// Every surface except Grok Bot.
    Cursor,
    /// Only the Grok Bot chat surface.
    GrokBot,
}

fn map_account_events(
    events: &[CursorUsageEvent],
    import_id: &str,
    since: Option<&str>,
    end_date: Option<&str>,
    now: &str,
    owner: Ownership,
) -> Vec<EventRow> {
    let mut rows: Vec<EventRow> = Vec::new();
    // daily key: date|source|model
    struct DailyAgg {
        input: u64,
        output: u64,
        cache_write: u64,
        cache_read: u64,
        total: u64,
        cost: f64,
        entries: u32,
        min_ts: String,
        max_ts: String,
    }
    let mut daily: HashMap<String, DailyAgg> = HashMap::new();

    for event in events {
        let ms = match timestamp_ms(event) {
            Some(v) => v,
            None => continue,
        };
        let dt = match event_datetime(ms) {
            Some(v) => v,
            None => continue,
        };
        let date = dt.format("%Y-%m-%d").to_string();
        if let Some(s) = since {
            if date.as_str() < s {
                continue;
            }
        }
        if let Some(e) = end_date {
            if date.as_str() > e {
                continue;
            }
        }

        let source = classify_cursor_surface(event);
        // Grok Bot events belong to `source::grok_bot`, so each event is
        // written exactly once across the two mappers.
        if (source == SOURCE_GROK_BOT) != matches!(owner, Ownership::GrokBot) {
            continue;
        }
        let model = event
            .model
            .as_deref()
            .filter(|m| !m.is_empty())
            .unwrap_or("unknown");
        let (input, output, cache_write, cache_read, total) = tokens_of(event);
        let cost = event_cost_usd(event);
        let ch_ts = dt.format("%Y-%m-%d %H:%M:%S").to_string();
        let session_id = event
            .conversation_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let identity = event_identity(event, source);
        let record_key = if session_id.is_empty() {
            identity.clone()
        } else {
            session_id.clone()
        };
        let raw_key = format!(
            "{source}|{machine}|session|{date}|{model}|{identity}",
            machine = CURSOR_ACCOUNT_MACHINE,
        );
        rows.push(EventRow {
            date: date.clone(),
            record_type: "session".to_string(),
            record_key,
            source: source.to_string(),
            machine_name: CURSOR_ACCOUNT_MACHINE.to_string(),
            account_id: String::new(),
            api_key_id: String::new(),
            model_name: model.to_string(),
            session_id,
            project_path: String::new(),
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_write,
            cache_read_tokens: cache_read,
            reasoning_tokens: 0,
            total_tokens: total,
            cost,
            dedup_key: make_dedup_key(&raw_key),
            import_id: import_id.to_string(),
            block_id: String::new(),
            start_time: Some(ch_ts.clone()),
            end_time: Some(ch_ts.clone()),
            actual_end_time: None,
            is_active: 0,
            is_gap: 0,
            entries: 1,
            burn_rate: 0.0,
            projection: 0.0,
            usage_limit_reset_time: None,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        });

        let daily_key = format!("{date}|{source}|{model}");
        let agg = daily.entry(daily_key).or_insert_with(|| DailyAgg {
            input: 0,
            output: 0,
            cache_write: 0,
            cache_read: 0,
            total: 0,
            cost: 0.0,
            entries: 0,
            min_ts: ch_ts.clone(),
            max_ts: ch_ts.clone(),
        });
        agg.input = agg.input.saturating_add(input);
        agg.output = agg.output.saturating_add(output);
        agg.cache_write = agg.cache_write.saturating_add(cache_write);
        agg.cache_read = agg.cache_read.saturating_add(cache_read);
        agg.total = agg.total.saturating_add(total);
        agg.cost += cost;
        agg.entries += 1;
        if ch_ts < agg.min_ts {
            agg.min_ts = ch_ts.clone();
        }
        if ch_ts > agg.max_ts {
            agg.max_ts = ch_ts;
        }
    }

    for (key, agg) in daily {
        let mut parts = key.splitn(3, '|');
        let date = parts.next().unwrap_or("");
        let source = parts.next().unwrap_or(SOURCE_CURSOR);
        let model = parts.next().unwrap_or("unknown");
        let raw_key = format!(
            "{source}|{machine}|daily|{date}|{model}|{date}",
            machine = CURSOR_ACCOUNT_MACHINE,
        );
        rows.push(EventRow {
            date: date.to_string(),
            record_type: "daily".to_string(),
            record_key: date.to_string(),
            source: source.to_string(),
            machine_name: CURSOR_ACCOUNT_MACHINE.to_string(),
            account_id: String::new(),
            api_key_id: String::new(),
            model_name: model.to_string(),
            session_id: String::new(),
            project_path: String::new(),
            input_tokens: agg.input,
            output_tokens: agg.output,
            cache_creation_tokens: agg.cache_write,
            cache_read_tokens: agg.cache_read,
            reasoning_tokens: 0,
            total_tokens: agg.total,
            cost: agg.cost,
            dedup_key: make_dedup_key(&raw_key),
            import_id: import_id.to_string(),
            block_id: String::new(),
            start_time: Some(agg.min_ts),
            end_time: Some(agg.max_ts),
            actual_end_time: None,
            is_active: 0,
            is_gap: 0,
            entries: agg.entries,
            burn_rate: 0.0,
            projection: 0.0,
            usage_limit_reset_time: None,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        });
    }

    rows
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

pub struct CursorSource {
    opts: CursorSourceOptions,
}

impl CursorSource {
    pub fn new(opts: CursorSourceOptions) -> Self {
        Self { opts }
    }
}

#[async_trait]
impl DataSource for CursorSource {
    fn name(&self) -> &'static str {
        SOURCE_CURSOR
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        match fetch_cursor_events(&self.opts).await {
            Ok(events) => {
                if self.opts.verbose {
                    eprintln!("Cursor Source parsed {} rows.", events.len());
                }
                Ok(SourceResult {
                    source_name: self.name().to_string(),
                    data: EventsSnapshotData { events },
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                    error: None,
                })
            }
            Err(e) => {
                if self.opts.verbose {
                    eprintln!("Cursor Source skipped/failed: {e}");
                }
                Ok(SourceResult {
                    source_name: self.name().to_string(),
                    data: EventsSnapshotData { events: Vec::new() },
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                    error: Some(e.to_string()),
                })
            }
        }
    }
}

/// Resolve auth, fetch pages, map. Missing auth → empty Ok (not an error).
pub async fn fetch_cursor_events(opts: &CursorSourceOptions) -> anyhow::Result<Vec<EventRow>> {
    let raw = fetch_cursor_usage_events(opts).await?;
    let now = ch_now();
    Ok(map_cursor_events(
        &raw,
        &opts.import_id,
        opts.since.as_deref(),
        opts.end_date.as_deref(),
        &now,
    ))
}

/// Raw usage events only, no mapping. Shared with `source::grok_bot`, which
/// maps the Grok Bot surface of the same events; the two sources therefore
/// read one API response instead of fetching the account twice.
pub async fn fetch_cursor_usage_events(
    opts: &CursorSourceOptions,
) -> anyhow::Result<Vec<CursorUsageEvent>> {
    let auth = resolve_cursor_auth(opts);
    let auth = match auth {
        Some(a) => a,
        None => {
            if opts.verbose {
                eprintln!("Cursor Source: no session/API key; skipping");
            }
            return Ok(Vec::new());
        }
    };

    let (since_ms, until_ms) = window_millis(opts.since.as_deref(), opts.days_back, opts.end_date.as_deref());
    match auth {
        CursorAuth::AdminApiKey(key) => {
            fetch_admin_pages(&key, since_ms, until_ms, opts.verbose).await
        }
        CursorAuth::Session(cookie) => {
            fetch_dashboard_pages(&cookie, since_ms, until_ms, opts.verbose).await
        }
    }
}

enum CursorAuth {
    AdminApiKey(String),
    Session(String),
}

fn resolve_cursor_auth(opts: &CursorSourceOptions) -> Option<CursorAuth> {
    let api = opts
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| env_nonempty("CURSOR_API_KEY"));
    if let Some(key) = api {
        return Some(CursorAuth::AdminApiKey(key));
    }

    let session = opts
        .session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| env_nonempty("CURSOR_SESSION"))
        .or_else(|| env_nonempty("CURSOR_COOKIE"));
    if let Some(s) = session {
        return Some(CursorAuth::Session(normalize_cookie_header(&s)));
    }

    if opts.disable_local_auth {
        return None;
    }
    let db_path = opts
        .state_db_path
        .clone()
        .or_else(default_cursor_state_db);
    if let Some(path) = db_path {
        if let Some(jwt) = read_cursor_access_token(&path) {
            if jwt_unexpired(&jwt) {
                return Some(CursorAuth::Session(cookie_from_access_token(&jwt)));
            }
        }
    }
    None
}

fn env_nonempty(key: &str) -> Option<String> {
    env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn normalize_cookie_header(raw: &str) -> String {
    let s = raw.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("cookie:") {
        return s[7..].trim().to_string();
    }
    if s.contains('=') {
        return s.to_string();
    }
    format!("WorkosCursorSessionToken={s}")
}

fn window_millis(
    since: Option<&str>,
    days_back: Option<i64>,
    end_date: Option<&str>,
) -> (Option<i64>, Option<i64>) {
    let start = if let Some(s) = since.filter(|s| !s.is_empty()) {
        naive_date_start_ms(s)
    } else if let Some(days) = days_back.filter(|d| *d > 0) {
        let d = chrono::Utc::now() - chrono::Duration::days(days);
        Some(d.timestamp_millis())
    } else {
        None
    };
    let end = end_date
        .filter(|s| !s.is_empty())
        .and_then(naive_date_end_ms);
    (start, end)
}

fn naive_date_start_ms(ymd: &str) -> Option<i64> {
    let nd = chrono::NaiveDate::parse_from_str(&ymd[..ymd.len().min(10)], "%Y-%m-%d").ok()?;
    Some(nd.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

fn naive_date_end_ms(ymd: &str) -> Option<i64> {
    let nd = chrono::NaiveDate::parse_from_str(&ymd[..ymd.len().min(10)], "%Y-%m-%d").ok()?;
    Some(nd.and_hms_milli_opt(23, 59, 59, 999)?.and_utc().timestamp_millis())
}

async fn fetch_dashboard_pages(
    cookie: &str,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    verbose: bool,
) -> anyhow::Result<Vec<CursorUsageEvent>> {
    let client = http_client()?;
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut body = serde_json::json!({
            "page": page,
            "pageSize": DEFAULT_PAGE_SIZE,
        });
        if let Some(s) = start_ms {
            body["startDate"] = Value::String(s.to_string());
        }
        if let Some(e) = end_ms {
            body["endDate"] = Value::String(e.to_string());
        }
        let resp = client
            .post(DASHBOARD_EVENTS_URL)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Origin", "https://cursor.com")
            .header("Cookie", cookie)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            anyhow::bail!("Cursor dashboard auth failed ({status})");
        }
        if !status.is_success() {
            anyhow::bail!("Cursor dashboard HTTP {status}: {}", trunc(&text));
        }
        let parsed: CursorUsagePage = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Cursor dashboard JSON: {e}: {}", trunc(&text)))?;
        let events = parsed.events();
        if verbose {
            eprintln!(
                "Cursor dashboard page {page}: {} events (total={:?})",
                events.len(),
                parsed.total_usage_events_count
            );
        }
        if events.is_empty() {
            break;
        }
        all.extend(events.iter().cloned());
        let short_page = events.len() < DEFAULT_PAGE_SIZE as usize;
        let no_next = parsed
            .pagination
            .as_ref()
            .and_then(|p| p.has_next_page)
            == Some(false);
        if short_page || no_next {
            break;
        }
    }
    Ok(all)
}

async fn fetch_admin_pages(
    api_key: &str,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    verbose: bool,
) -> anyhow::Result<Vec<CursorUsageEvent>> {
    let client = http_client()?;
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut body = serde_json::json!({
            "page": page,
            "pageSize": DEFAULT_PAGE_SIZE,
        });
        if let Some(s) = start_ms {
            body["startDate"] = Value::Number(s.into());
        }
        if let Some(e) = end_ms {
            body["endDate"] = Value::Number(e.into());
        }
        let resp = client
            .post(ADMIN_EVENTS_URL)
            .basic_auth(api_key, Some(""))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            anyhow::bail!("Cursor Admin API auth failed ({status})");
        }
        if !status.is_success() {
            anyhow::bail!("Cursor Admin API HTTP {status}: {}", trunc(&text));
        }
        let parsed: CursorUsagePage = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Cursor Admin JSON: {e}: {}", trunc(&text)))?;
        let events = parsed.events();
        if verbose {
            eprintln!(
                "Cursor admin page {page}: {} events (total={:?})",
                events.len(),
                parsed.total_usage_events_count
            );
        }
        if events.is_empty() {
            break;
        }
        all.extend(events.iter().cloned());
        let short_page = events.len() < DEFAULT_PAGE_SIZE as usize;
        let no_next = parsed
            .pagination
            .as_ref()
            .and_then(|p| p.has_next_page)
            == Some(false);
        let past_last = parsed.pagination.as_ref().and_then(|p| {
            let cur = p.current_page?;
            let n = p.num_pages?;
            Some(cur >= n)
        }) == Some(true);
        if short_page || no_next || past_last {
            break;
        }
    }
    Ok(all)
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?)
}

fn trunc(s: &str) -> String {
    let t = s.trim();
    if t.len() > 240 {
        format!("{}…", &t[..240])
    } else {
        t.to_string()
    }
}

pub fn default_cursor_state_db() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    if cfg!(target_os = "macos") {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb"),
        )
    } else {
        let base = env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"));
        Some(
            base.join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb"),
        )
    }
}

/// Read `cursorAuth/accessToken` from Cursor.app's VS Code state DB.
pub fn read_cursor_access_token(db_path: &Path) -> Option<String> {
    if !db_path.exists() {
        return None;
    }
    let temp_dir = env::temp_dir();
    let id = uuid::Uuid::new_v4();
    let temp_db = temp_dir.join(format!("cursor-auth-{id}.vscdb"));
    if fs::copy(db_path, &temp_db).is_err() {
        return None;
    }
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    let shm = PathBuf::from(format!("{}-shm", db_path.display()));
    if wal.exists() {
        let _ = fs::copy(&wal, format!("{}-wal", temp_db.display()));
    }
    if shm.exists() {
        let _ = fs::copy(&shm, format!("{}-shm", temp_db.display()));
    }
    let token = (|| {
        let conn = Connection::open(&temp_db).ok()?;
        let value: String = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1",
                ["cursorAuth/accessToken"],
                |row| row.get(0),
            )
            .ok()?;
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.starts_with('"') {
            serde_json::from_str::<String>(trimmed).ok()
        } else {
            Some(trimmed.to_string())
        }
    })();
    let _ = fs::remove_file(&temp_db);
    let _ = fs::remove_file(format!("{}-wal", temp_db.display()));
    let _ = fs::remove_file(format!("{}-shm", temp_db.display()));
    token
}

fn cookie_from_access_token(jwt: &str) -> String {
    if let Some(uid) = jwt_user_id(jwt) {
        format!("WorkosCursorSessionToken={uid}%3A%3A{jwt}")
    } else {
        format!("WorkosCursorSessionToken={jwt}")
    }
}

fn jwt_unexpired(jwt: &str) -> bool {
    match jwt_payload(jwt).and_then(|v| v.get("exp").and_then(|e| e.as_i64())) {
        Some(exp) => exp > chrono::Utc::now().timestamp() + 60,
        None => true,
    }
}

fn jwt_user_id(jwt: &str) -> Option<String> {
    let payload = jwt_payload(jwt)?;
    let sub = payload.get("sub")?.as_str()?;
    let id = sub.rsplit('|').next()?.to_string();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn jwt_payload(jwt: &str) -> Option<Value> {
    let part = jwt.split('.').nth(1)?;
    let bytes = b64url_decode(part)?;
    serde_json::from_slice(&bytes).ok()
}

fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let padded = input.trim_end_matches('=');
    let bytes = padded.as_bytes();
    let mut out = Vec::with_capacity(padded.len() * 3 / 4 + 1);
    let mut i = 0;
    while i < bytes.len() {
        let a = val(bytes[i])?;
        let b = if i + 1 < bytes.len() {
            val(bytes[i + 1])?
        } else {
            0
        };
        let c = if i + 2 < bytes.len() {
            val(bytes[i + 2])?
        } else {
            0
        };
        let d = if i + 3 < bytes.len() {
            val(bytes[i + 3])?
        } else {
            0
        };
        out.push((a << 2) | (b >> 4));
        if i + 2 < bytes.len() {
            out.push((b << 4) | (c >> 2));
        }
        if i + 3 < bytes.len() {
            out.push((c << 6) | d);
        }
        i += 4;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Flexible number decoding (Cursor serializes some numbers as strings)
// ---------------------------------------------------------------------------

fn de_opt_i64_flex<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(i64_from_value(&Value::deserialize(deserializer).map_err(serde::de::Error::custom)?))
}

fn de_opt_f64_flex<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(f64_from_value(&Value::deserialize(deserializer).map_err(serde::de::Error::custom)?))
}

fn de_u64_flex<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(u64_from_value(&Value::deserialize(deserializer).map_err(serde::de::Error::custom)?).unwrap_or(0))
}

fn i64_from_value(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<i64>()
                    .ok()
                    .or_else(|| t.parse::<f64>().ok().map(|f| f as i64))
            }
        }
        _ => None,
    }
}

fn f64_from_value(v: &Value) -> Option<f64> {
    match v {
        Value::Null => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok().filter(|f| f.is_finite())
            }
        }
        _ => None,
    }
}

fn u64_from_value(v: &Value) -> Option<u64> {
    match v {
        Value::Null => None,
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok()))
            .or_else(|| n.as_f64().and_then(|f| if f >= 0.0 { Some(f as u64) } else { None })),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<u64>()
                    .ok()
                    .or_else(|| t.parse::<f64>().ok().map(|f| f as u64))
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "totalUsageEventsCount": 5,
      "usageEventsDisplay": [
        {
          "timestamp": "1750979225854",
          "userEmail": "dev@example.com",
          "conversationId": "conv-editor",
          "model": "claude-4.5-sonnet",
          "kind": "Usage-based",
          "isTokenBasedCall": true,
          "isChargeable": true,
          "isHeadless": false,
          "tokenUsage": {
            "inputTokens": 126,
            "outputTokens": 450,
            "cacheWriteTokens": 6112,
            "cacheReadTokens": 11964,
            "totalCents": 20.18232
          },
          "chargedCents": 21.36232
        },
        {
          "timestamp": "1750979300000",
          "userEmail": "dev@example.com",
          "conversationId": "conv-cloud",
          "cloudAgentId": "bc_run_abc",
          "model": "claude-4.5-sonnet",
          "kind": "Usage-based",
          "isHeadless": true,
          "tokenUsage": {
            "inputTokens": 200,
            "outputTokens": 50,
            "cacheWriteTokens": 10,
            "cacheReadTokens": 5,
            "totalCents": 4.0
          },
          "chargedCents": 5.0
        },
        {
          "timestamp": "1750979400000",
          "userEmail": "agent-runner@example.com",
          "serviceAccountId": "sa_abc123",
          "serviceAccountName": "Nightly CI Agent",
          "conversationId": "conv-api",
          "model": "claude-4.5-sonnet",
          "kind": "Usage-based",
          "isHeadless": true,
          "tokenUsage": {
            "inputTokens": 80,
            "outputTokens": 20,
            "cacheWriteTokens": 0,
            "cacheReadTokens": 0,
            "totalCents": 1.5
          },
          "chargedCents": 2.0
        },
        {
          "timestamp": "1750979500000",
          "userEmail": "dev@example.com",
          "conversationId": "conv-grok",
          "model": "grok-4.5",
          "kind": "Grok Bot",
          "isHeadless": false,
          "tokenUsage": {
            "inputTokens": 40,
            "outputTokens": 10,
            "cacheWriteTokens": 0,
            "cacheReadTokens": 8,
            "totalCents": 0.8
          },
          "chargedCents": 1.0
        },
        {
          "timestamp": "1750979600000",
          "kind": "USAGE_EVENT_KIND_INCLUDED_IN_ULTRA",
          "isHeadless": false,
          "serviceAccountId": "null",
          "cloudAgentId": "null",
          "chargedCents": 3
        }
      ]
    }"#;

    fn sessions<'a>(rows: &'a [EventRow]) -> Vec<&'a EventRow> {
        rows.iter().filter(|e| e.record_type == "session").collect()
    }

    /// The fixture's Grok Bot chat event, as `source::grok_bot` sees it.
    fn grok_bot_event() -> CursorUsageEvent {
        events_from_page_json(FIXTURE)
            .expect("fixture must parse")
            .into_iter()
            .find(|e| e.conversation_id.as_deref() == Some("conv-grok"))
            .expect("fixture has a grok bot event")
    }

    #[test]
    fn fixture_maps_three_surfaces_plus_residual() {
        let rows = map_cursor_events_json(FIXTURE, "import-test", None, None, "2026-08-19 00:00:00")
            .expect("fixture must parse");
        let sess = sessions(&rows);
        // The Grok Bot chat event is written by `source::grok_bot` instead.
        assert_eq!(sess.len(), 4, "unclassifiable events still produce a row");

        let editor = sess.iter().find(|e| e.session_id == "conv-editor").unwrap();
        let cloud = sess.iter().find(|e| e.session_id == "conv-cloud").unwrap();
        let api = sess.iter().find(|e| e.session_id == "conv-api").unwrap();
        let residual = sess
            .iter()
            .find(|e| e.session_id.is_empty() && e.model_name == "unknown")
            .expect("unclassifiable residual row");

        assert_eq!(editor.source, SOURCE_CURSOR);
        assert_eq!(cloud.source, SOURCE_CLOUD_AGENT);
        assert_eq!(api.source, SOURCE_API);
        assert_eq!(residual.source, SOURCE_CURSOR);
        assert!(sess.iter().all(|e| e.source != SOURCE_GROK_BOT));
        assert_eq!(classify_cursor_surface(&grok_bot_event()), SOURCE_GROK_BOT);

        let sources: Vec<&str> = [editor, cloud, api, residual].iter().map(|e| e.source.as_str()).collect();
        for i in 0..sources.len() {
            for j in (i + 1)..sources.len() {
                assert_ne!(sources[i], sources[j], "each kind must differ");
            }
        }

        assert_eq!(editor.input_tokens, 126);
        assert_eq!(editor.output_tokens, 450);
        assert_eq!(editor.cache_creation_tokens, 6112);
        assert_eq!(editor.cache_read_tokens, 11964);
        assert_eq!(editor.total_tokens, 126 + 450 + 6112 + 11964);
        assert!((editor.cost - 0.2136232).abs() < 1e-6, "chargedCents 21.36232 → USD, got {}", editor.cost);

        assert_eq!(cloud.input_tokens, 200);
        assert!((cloud.cost - 0.05).abs() < 1e-9);
        assert_eq!(api.input_tokens, 80);
        assert!((api.cost - 0.02).abs() < 1e-9);
        assert_eq!(residual.total_tokens, 0);
        assert!((residual.cost - 0.03).abs() < 1e-9);

        for row in &sess {
            assert_eq!(row.machine_name, CURSOR_ACCOUNT_MACHINE);
            assert_ne!(row.machine_name, hostname_hint());
            assert_eq!(row.dedup_key.len(), 16);
            assert!(!row.dedup_key.is_empty());
        }
    }

    /// Cursor and `source::grok_bot` read the same feed, so they must partition
    /// it: every event is claimed once. Emitting the Grok Bot chat from both
    /// sides would produce two dedup keys, and the sinks would keep both.
    #[test]
    fn cursor_and_grok_bot_claim_disjoint_events() {
        let events = events_from_page_json(FIXTURE).expect("fixture must parse");
        let now = "2026-08-19 00:00:00";
        let cursor_rows = map_cursor_events(&events, "imp", None, None, now);
        let bot_rows = map_grok_bot_events(&events, "imp", None, None, now);

        let bot = sessions(&bot_rows);
        assert_eq!(bot.len(), 1, "only the grok bot chat event: {bot:?}");
        assert_eq!(bot[0].session_id, "conv-grok");
        assert_eq!(bot[0].cache_read_tokens, 8);
        assert!((bot[0].cost - 0.01).abs() < 1e-9);
        assert_eq!(sessions(&cursor_rows).len(), 4);

        let keys: std::collections::HashSet<_> = cursor_rows
            .iter()
            .chain(bot_rows.iter())
            .map(|r| r.dedup_key.clone())
            .collect();
        assert_eq!(
            keys.len(),
            cursor_rows.len() + bot_rows.len(),
            "a duplicate dedup key means the same event is imported twice"
        );
    }

    #[test]
    fn repeated_account_imports_produce_identical_dedup_keys() {
        // The anti-double-count invariant for account-wide sources: two hosts
        // importing the same Cursor account must land on the same dedup_key, so
        // ReplacingMergeTree collapses them. Asserting only that the hostname
        // variant *differs* (below) does not prove this — it is the identity
        // across machines that stops double counting.
        let a = map_cursor_events_json(FIXTURE, "imp-a", None, None, "2026-08-19 00:00:00").unwrap();
        let b = map_cursor_events_json(FIXTURE, "imp-b", None, None, "2026-08-19 00:00:00").unwrap();
        assert_eq!(a.len(), b.len());
        let keys_a: std::collections::HashSet<_> = a.iter().map(|r| r.dedup_key.clone()).collect();
        let keys_b: std::collections::HashSet<_> = b.iter().map(|r| r.dedup_key.clone()).collect();
        assert!(
            keys_a == keys_b,
            "account-wide rows must not be host-scoped or the same event is counted twice"
        );
        // import_id legitimately differs per run; only identity must match.
        assert!(a.iter().all(|r| r.import_id == "imp-a"));
        assert!(b.iter().all(|r| r.import_id == "imp-b"));
    }

    #[test]
    fn account_identity_ignores_importer_hostname() {
        let rows = map_cursor_events_json(FIXTURE, "imp", None, None, "2026-08-19 00:00:00").unwrap();
        assert!(rows.iter().all(|r| r.machine_name == "account"));
        assert!(rows.iter().all(|r| r.machine_name != "duet-macbook"));
        let raw = format!(
            "{source}|account|session|2025-06-26|claude-4.5-sonnet|x",
            source = SOURCE_CURSOR
        );
        let with_host = format!(
            "{source}|duet-macbook|session|2025-06-26|claude-4.5-sonnet|x",
            source = SOURCE_CURSOR
        );
        assert_ne!(make_dedup_key(&raw), make_dedup_key(&with_host));
        let editor = rows
            .iter()
            .find(|e| e.record_type == "session" && e.session_id == "conv-editor")
            .unwrap();
        assert!(!editor.dedup_key.is_empty());
    }

    #[test]
    fn admin_wrapper_key_usage_events_parses() {
        let json = r#"{
          "totalUsageEventsCount": 1,
          "usageEvents": [
            {
              "timestamp": 1750979225854,
              "model": "claude-4.5-sonnet",
              "isHeadless": false,
              "tokenUsage": {"inputTokens": "10", "outputTokens": 2, "cacheWriteTokens": 0, "cacheReadTokens": 0, "totalCents": "1.5"},
              "chargedCents": "1.5"
            }
          ]
        }"#;
        let rows = map_cursor_events_json(json, "i", None, None, "now").unwrap();
        let sess = sessions(&rows);
        assert_eq!(sess.len(), 1);
        assert_eq!(sess[0].input_tokens, 10);
        assert_eq!(sess[0].output_tokens, 2);
        assert!((sess[0].cost - 0.015).abs() < 1e-9);
        assert_eq!(sess[0].source, SOURCE_CURSOR);
    }

    #[test]
    fn string_null_ids_are_not_api_or_cloud_agent() {
        let event: CursorUsageEvent = serde_json::from_str(
            r#"{"timestamp":"1","model":"claude-4.5-sonnet","isHeadless":false,"serviceAccountId":"null","cloudAgentId":"null","kind":"USAGE_EVENT_KIND_INCLUDED_IN_ULTRA"}"#,
        )
        .unwrap();
        assert_eq!(classify_cursor_surface(&event), SOURCE_CURSOR);
    }

    #[test]
    fn classify_priority_api_beats_headless_and_grok() {
        let event: CursorUsageEvent = serde_json::from_str(
            r#"{"timestamp":"1","model":"grok-4.5","isHeadless":true,"serviceAccountId":"sa_1","cloudAgentId":"bc_1"}"#,
        )
        .unwrap();
        assert_eq!(classify_cursor_surface(&event), SOURCE_API);
    }

    #[test]
    fn missing_auth_returns_empty_not_error() {
        let opts = CursorSourceOptions {
            import_id: "t".into(),
            disable_local_auth: true,
            session: None,
            api_key: None,
            ..CursorSourceOptions::default()
        };
        let src = CursorSource::new(opts);
        let result = futures::executor::block_on(async move { src.fetch().await }).unwrap();
        assert_eq!(result.source_name, SOURCE_CURSOR);
        assert!(result.data.events.is_empty());
        // no credentials → skip, not a hard source error
        assert!(result.error.is_none());
    }

    fn hostname_hint() -> &'static str {
        "test-host"
    }
}
