use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TOKEN_PREFIX: &str = "summa_";
pub const MAX_INGEST_EVENTS: usize = 500;
pub const MAX_INGEST_BYTES: u64 = 1_500_000;
pub const MAX_ANALYTICS_DAYS: i64 = 366;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventRow {
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub record_type: String,
    #[serde(default)]
    pub record_key: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub machine_name: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub api_key_id: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub project_path: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub dedup_key: String,
    #[serde(default)]
    pub import_id: String,
    #[serde(default)]
    pub block_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_end_time: Option<String>,
    #[serde(default)]
    pub is_active: u8,
    #[serde(default)]
    pub is_gap: u8,
    #[serde(default)]
    pub entries: u32,
    #[serde(default)]
    pub burn_rate: f64,
    #[serde(default)]
    pub projection: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_limit_reset_time: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SinkAck {
    pub name: String,
    pub rows: u64,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PingSample {
    pub name: String,
    pub ok: bool,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsPoint {
    pub date: String,
    pub source: String,
    pub model_name: String,
    pub cost: f64,
    pub total_tokens: u64,
    pub entries: u64,
}

pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

pub fn sha256_hex16(input: &str) -> String {
    sha256_hex(input).chars().take(16).collect()
}

pub fn timing_safe_eq(a: &str, b: &str) -> bool {
    let ha = sha256_hex(a);
    let hb = sha256_hex(b);
    let mut diff = 0u8;
    for (x, y) in ha.as_bytes().iter().zip(hb.as_bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn ingest_body_too_large(content_length: Option<&str>) -> bool {
    content_length
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n > MAX_INGEST_BYTES)
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestParseError {
    InvalidJson,
    EventsNotArray,
    TooLarge,
    TooManyEvents,
}

impl IngestParseError {
    pub fn status(self) -> u16 {
        match self {
            Self::InvalidJson | Self::EventsNotArray => 400,
            Self::TooLarge | Self::TooManyEvents => 413,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid json",
            Self::EventsNotArray => "events must be an array",
            Self::TooLarge => "payload too large",
            Self::TooManyEvents => "too many events",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParsedIngest {
    pub events: Vec<EventRow>,
    pub rejected: usize,
}

/// Parse `/v1/ingest` from raw bytes with `serde_json`.
///
/// Avoid Worker `Request::json` (`serde_wasm_bindgen`): a type mismatch or
/// `null` string fails the whole body. Skip bad rows; keep sibling totals.
pub fn parse_ingest_bytes(bytes: &[u8]) -> std::result::Result<ParsedIngest, IngestParseError> {
    if bytes.len() as u64 > MAX_INGEST_BYTES {
        return Err(IngestParseError::TooLarge);
    }
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| IngestParseError::InvalidJson)?;
    let Some(obj) = v.as_object() else {
        return Err(IngestParseError::InvalidJson);
    };
    let arr = match obj.get("events") {
        None => return Ok(ParsedIngest::default()),
        Some(serde_json::Value::Array(arr)) => arr,
        Some(_) => return Err(IngestParseError::EventsNotArray),
    };
    if arr.len() > MAX_INGEST_EVENTS {
        return Err(IngestParseError::TooManyEvents);
    }
    let mut events = Vec::new();
    let mut rejected = 0;
    for item in arr {
        match event_from_json(item).and_then(sanitize_event) {
            Some(row) => events.push(row),
            None => rejected += 1,
        }
    }
    Ok(ParsedIngest { events, rejected })
}

pub fn event_from_json(v: &serde_json::Value) -> Option<EventRow> {
    if !v.is_object() {
        return None;
    }
    Some(EventRow {
        date: json_string(v, &["date"]),
        record_type: json_string(v, &["record_type", "recordType"]),
        record_key: json_string(v, &["record_key", "recordKey"]),
        source: json_string(v, &["source"]),
        machine_name: json_string(v, &["machine_name", "machineName"]),
        account_id: json_string(v, &["account_id", "accountId"]),
        api_key_id: json_string(v, &["api_key_id", "apiKeyId"]),
        model_name: json_string(v, &["model_name", "modelName"]),
        session_id: json_string(v, &["session_id", "sessionId"]),
        project_path: json_string(v, &["project_path", "projectPath"]),
        input_tokens: json_u64(v, &["input_tokens", "inputTokens"]),
        output_tokens: json_u64(v, &["output_tokens", "outputTokens"]),
        cache_creation_tokens: json_u64(
            v,
            &[
                "cache_creation_tokens",
                "cacheCreationTokens",
                "cacheCreationInputTokens",
            ],
        ),
        cache_read_tokens: json_u64(
            v,
            &[
                "cache_read_tokens",
                "cacheReadTokens",
                "cacheReadInputTokens",
                "cachedInputTokens",
            ],
        ),
        reasoning_tokens: json_u64(v, &["reasoning_tokens", "reasoningTokens"]),
        total_tokens: json_u64(v, &["total_tokens", "totalTokens"]),
        cost: json_f64(v, &["cost", "totalCost", "total_cost", "costUSD"]),
        dedup_key: json_string(v, &["dedup_key", "dedupKey"]),
        import_id: json_string(v, &["import_id", "importId"]),
        block_id: json_string(v, &["block_id", "blockId"]),
        start_time: json_opt_ts(v, &["start_time", "startTime"]),
        end_time: json_opt_ts(v, &["end_time", "endTime"]),
        actual_end_time: json_opt_ts(v, &["actual_end_time", "actualEndTime"]),
        is_active: json_flag(v, &["is_active", "isActive"]),
        is_gap: json_flag(v, &["is_gap", "isGap"]),
        entries: json_u64(v, &["entries"]).min(u32::MAX as u64) as u32,
        burn_rate: json_f64(v, &["burn_rate", "burnRate"]),
        projection: json_f64(v, &["projection"]),
        usage_limit_reset_time: json_opt_ts(v, &["usage_limit_reset_time", "usageLimitResetTime"]),
        created_at: json_string(v, &["created_at", "createdAt"]),
        updated_at: json_string(v, &["updated_at", "updatedAt"]),
    })
}

fn json_string(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        match v.get(*key) {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Null) | None => continue,
            Some(_) => continue,
        }
    }
    String::new()
}

fn json_u64(v: &serde_json::Value, keys: &[&str]) -> u64 {
    for key in keys {
        let Some(x) = v.get(*key) else {
            continue;
        };
        if let Some(n) = value_as_u64(x) {
            return n;
        }
    }
    0
}

fn json_f64(v: &serde_json::Value, keys: &[&str]) -> f64 {
    for key in keys {
        let Some(x) = v.get(*key) else {
            continue;
        };
        if let Some(n) = value_as_f64(x) {
            return n;
        }
    }
    0.0
}

fn json_flag(v: &serde_json::Value, keys: &[&str]) -> u8 {
    for key in keys {
        match v.get(*key) {
            Some(serde_json::Value::Bool(true)) => return 1,
            Some(serde_json::Value::Bool(false)) => return 0,
            Some(x) => {
                if let Some(n) = value_as_u64(x) {
                    return u8::from(n > 0);
                }
            }
            None => continue,
        }
    }
    0
}

fn json_opt_ts(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let s = json_string(v, keys);
    sanitize_ts(Some(s))
}

fn value_as_u64(x: &serde_json::Value) -> Option<u64> {
    if x.is_null() {
        return None;
    }
    if let Some(n) = x.as_u64() {
        return Some(n);
    }
    if let Some(n) = x.as_i64() {
        return Some(n.max(0) as u64);
    }
    if let Some(n) = x.as_f64() {
        if n.is_finite() && n >= 0.0 {
            return Some(n.trunc() as u64);
        }
        return Some(0);
    }
    if let Some(s) = x.as_str() {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if let Ok(n) = s.parse::<u64>() {
            return Some(n);
        }
        if let Ok(n) = s.parse::<f64>() {
            if n.is_finite() && n >= 0.0 {
                return Some(n.trunc() as u64);
            }
            return Some(0);
        }
    }
    None
}

fn value_as_f64(x: &serde_json::Value) -> Option<f64> {
    if x.is_null() {
        return None;
    }
    if let Some(n) = x.as_f64() {
        return Some(if n.is_finite() { n } else { 0.0 });
    }
    if let Some(n) = x.as_i64() {
        return Some(n as f64);
    }
    if let Some(n) = x.as_u64() {
        return Some(n as f64);
    }
    if let Some(s) = x.as_str() {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if let Ok(n) = s.parse::<f64>() {
            return Some(if n.is_finite() { n } else { 0.0 });
        }
    }
    None
}

fn clip_field(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}

pub fn valid_iso_date(value: &str) -> bool {
    value.len() == 10 && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
}

fn looks_like_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':'
}

fn sanitize_ts(value: Option<String>) -> Option<String> {
    let s = clip_field(value?.trim(), 32);
    if looks_like_timestamp(&s) {
        Some(s)
    } else {
        None
    }
}

/// Drop forged tenant fields and junk rows before the worker stamps identity.
pub fn sanitize_event(mut row: EventRow) -> Option<EventRow> {
    row.date = row.date.trim().to_string();
    if !valid_iso_date(&row.date) {
        return None;
    }
    row.record_type = clip_field(&row.record_type, 32);
    if row.record_type.is_empty() {
        return None;
    }
    row.record_key = clip_field(&row.record_key, 256);
    row.source = clip_field(&row.source, 64);
    row.machine_name = clip_field(&row.machine_name, 128);
    row.model_name = clip_field(&row.model_name, 256);
    row.session_id = clip_field(&row.session_id, 128);
    row.project_path = clip_field(&row.project_path, 512);
    row.import_id = clip_field(&row.import_id, 64);
    row.block_id = clip_field(&row.block_id, 128);
    row.created_at = clip_field(&row.created_at, 32);
    row.updated_at = clip_field(&row.updated_at, 32);
    row.start_time = sanitize_ts(row.start_time);
    row.end_time = sanitize_ts(row.end_time);
    row.actual_end_time = sanitize_ts(row.actual_end_time);
    row.usage_limit_reset_time = sanitize_ts(row.usage_limit_reset_time);
    row.account_id.clear();
    row.api_key_id.clear();
    row.dedup_key.clear();
    if !row.cost.is_finite() {
        row.cost = 0.0;
    }
    if !row.burn_rate.is_finite() {
        row.burn_rate = 0.0;
    }
    if !row.projection.is_finite() {
        row.projection = 0.0;
    }
    Some(row)
}

pub fn stamp_ingest_identity(row: &mut EventRow, account_id: &str, api_key_id: &str, now: &str) {
    row.account_id = account_id.to_string();
    row.api_key_id = api_key_id.to_string();
    row.dedup_key = sha256_hex16(&format!(
        "{}|{}|{}|{}|{}|{}|{}",
        row.account_id,
        row.source,
        row.machine_name,
        row.record_type,
        row.date,
        row.model_name,
        row.record_key
    ));
    if row.created_at.is_empty() {
        row.created_at = now.to_string();
    }
    row.updated_at = now.to_string();
}

pub fn ch_now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn new_id() -> String {
    let mut buf = [0u8; 16];
    let _ = getrandom::fill(&mut buf);
    hex::encode(buf)
}

pub fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn ingest_status_code(sinks: &[SinkAck]) -> u16 {
    if sinks.is_empty() {
        503
    } else if sinks.iter().any(|s| s.error.is_none()) {
        200
    } else {
        502
    }
}

pub fn ping_ok(samples: &[PingSample]) -> bool {
    !samples.is_empty() && samples.iter().all(|s| s.ok)
}

pub fn cors_allow_origin(origin: Option<&str>) -> Option<String> {
    let Some(o) = origin.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some("*".into());
    };
    if o == "https://burn.duyet.net" || o == "https://summa.duyet.net" {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_status_empty_is_unavailable() {
        assert_eq!(ingest_status_code(&[]), 503);
    }

    #[test]
    fn ingest_status_any_ok_is_200() {
        let sinks = vec![
            SinkAck {
                name: "clickhouse".into(),
                rows: 1,
                duration_ms: 2,
                error: None,
            },
            SinkAck {
                name: "motherduck".into(),
                rows: 0,
                duration_ms: 3,
                error: Some("timeout".into()),
            },
        ];
        assert_eq!(ingest_status_code(&sinks), 200);
    }

    #[test]
    fn ingest_status_all_errors_is_502() {
        let sinks = vec![SinkAck {
            name: "clickhouse".into(),
            rows: 0,
            duration_ms: 1,
            error: Some("refused".into()),
        }];
        assert_eq!(ingest_status_code(&sinks), 502);
    }

    #[test]
    fn ping_ok_requires_all_samples() {
        assert!(!ping_ok(&[]));
        assert!(ping_ok(&[PingSample {
            name: "ch".into(),
            ok: true,
            latency_ms: 4,
            error: None,
        }]));
        assert!(!ping_ok(&[
            PingSample {
                name: "ch".into(),
                ok: true,
                latency_ms: 4,
                error: None,
            },
            PingSample {
                name: "md".into(),
                ok: false,
                latency_ms: 9,
                error: Some("404".into()),
            },
        ]));
    }

    #[test]
    fn cors_allows_burn_and_localhost() {
        assert_eq!(cors_allow_origin(None).as_deref(), Some("*"));
        assert_eq!(
            cors_allow_origin(Some("https://burn.duyet.net")).as_deref(),
            Some("https://burn.duyet.net")
        );
        assert_eq!(
            cors_allow_origin(Some("http://localhost:3000")).as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(cors_allow_origin(Some("https://evil.example")), None);
    }

    #[test]
    fn sql_literal_escapes_quotes() {
        assert_eq!(sql_literal("acme"), "'acme'");
        assert_eq!(sql_literal("a'b"), "'a''b'");
    }

    #[test]
    fn token_hash_is_stable() {
        let h = sha256_hex16("summa_test");
        assert_eq!(h.len(), 16);
        assert_eq!(h, sha256_hex16("summa_test"));
        assert_ne!(h, sha256_hex16("summa_other"));
    }

    #[test]
    fn sanitize_drops_forged_tenant_and_client_dedup() {
        let mut row = EventRow::default();
        row.date = "2026-08-19".into();
        row.record_type = "daily".into();
        row.source = "cursor".into();
        row.account_id = "victim".into();
        row.api_key_id = "k".into();
        row.dedup_key = "aaaaaaaaaaaaaaaa".into();
        row.project_path = "x\u{0000}y".into();
        let out = sanitize_event(row).unwrap();
        assert!(out.account_id.is_empty());
        assert!(out.api_key_id.is_empty());
        assert!(out.dedup_key.is_empty());
        assert_eq!(out.project_path, "xy");
    }

    #[test]
    fn sanitize_rejects_bad_dates() {
        let mut row = EventRow::default();
        row.date = "2026/08/19".into();
        row.record_type = "daily".into();
        assert!(sanitize_event(row).is_none());
    }

    #[test]
    fn sanitize_drops_invalid_timestamps() {
        let mut row = EventRow::default();
        row.date = "2026-08-19".into();
        row.record_type = "session".into();
        row.start_time = Some("later".into());
        row.end_time = Some("2026-08-19 12:00:00".into());
        let out = sanitize_event(row).unwrap();
        assert!(out.start_time.is_none());
        assert_eq!(out.end_time.as_deref(), Some("2026-08-19 12:00:00"));
    }

    #[test]
    fn stamp_overwrites_client_dedup_with_account() {
        let mut row = EventRow {
            date: "2026-08-19".into(),
            record_type: "daily".into(),
            record_key: "k".into(),
            source: "cursor".into(),
            machine_name: "m".into(),
            model_name: "grok".into(),
            dedup_key: "clientkey".into(),
            ..EventRow::default()
        };
        stamp_ingest_identity(&mut row, "acc-1", "key-1", "2026-08-20 00:00:00");
        assert_eq!(row.account_id, "acc-1");
        assert_eq!(row.api_key_id, "key-1");
        assert_ne!(row.dedup_key, "clientkey");
        assert_eq!(row.dedup_key.len(), 16);
        let mut other = row.clone();
        stamp_ingest_identity(&mut other, "acc-2", "key-1", "2026-08-20 00:00:00");
        assert_ne!(row.dedup_key, other.dedup_key);
    }

    #[test]
    fn ingest_body_too_large_uses_content_length() {
        assert!(!ingest_body_too_large(None));
        assert!(!ingest_body_too_large(Some("100")));
        assert!(ingest_body_too_large(Some("2000000")));
    }

    fn sample_ingest_json() -> serde_json::Value {
        serde_json::json!({
            "events": [{
                "date": "2026-08-19",
                "record_type": "session",
                "record_key": "s1",
                "source": "cursor",
                "machine_name": "account",
                "model_name": "grok-4.5",
                "input_tokens": 100,
                "output_tokens": 20,
                "total_tokens": 120,
                "cost": 1.25,
                "entries": 1
            }]
        })
    }

    #[test]
    fn parse_ingest_success_keeps_payload_totals() {
        let parsed = parse_ingest_bytes(sample_ingest_json().to_string().as_bytes()).unwrap();
        assert_eq!(parsed.rejected, 0);
        assert_eq!(parsed.events.len(), 1);
        let row = &parsed.events[0];
        assert_eq!(row.source, "cursor");
        assert_eq!(row.total_tokens, 120);
        assert!((row.cost - 1.25).abs() < 1e-12);
        assert!(row.account_id.is_empty());
        assert!(row.dedup_key.is_empty());
    }

    #[test]
    fn parse_ingest_auth_payload_shape_is_object_with_events() {
        assert_eq!(
            parse_ingest_bytes(b"not json").unwrap_err(),
            IngestParseError::InvalidJson
        );
        assert_eq!(
            parse_ingest_bytes(b"[]").unwrap_err(),
            IngestParseError::InvalidJson
        );
        assert_eq!(
            parse_ingest_bytes(br#"{"events":null}"#).unwrap_err(),
            IngestParseError::EventsNotArray
        );
        assert_eq!(
            parse_ingest_bytes(br#"{"events":{}}"#).unwrap_err(),
            IngestParseError::EventsNotArray
        );
        let empty = parse_ingest_bytes(br#"{"events":[]}"#).unwrap();
        assert!(empty.events.is_empty());
        assert_eq!(empty.rejected, 0);
        let bare = parse_ingest_bytes(br#"{}"#).unwrap();
        assert!(bare.events.is_empty());
        assert_eq!(bare.rejected, 0);
    }

    #[test]
    fn parse_ingest_malformed_event_does_not_drop_siblings() {
        let body = serde_json::json!({
            "events": [
                {"date": "2026-08-19", "record_type": "daily", "source": "grok", "total_tokens": 9, "cost": 0.5},
                "nope",
                {"date": "bad", "record_type": "daily"},
                {"date": "2026-08-20", "record_type": "session", "source": "cursor", "input_tokens": 10.9, "totalTokens": "11", "cost": "0.25"}
            ]
        });
        let parsed = parse_ingest_bytes(body.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.rejected, 2);
        assert_eq!(parsed.events[0].source, "grok");
        assert_eq!(parsed.events[0].total_tokens, 9);
        assert_eq!(parsed.events[1].input_tokens, 10);
        assert_eq!(parsed.events[1].total_tokens, 11);
        assert!((parsed.events[1].cost - 0.25).abs() < 1e-12);
    }

    #[test]
    fn parse_ingest_camel_case_keeps_ccusage_tokens() {
        let body = serde_json::json!({
            "events": [{
                "date": "2026-08-19",
                "recordType": "daily",
                "recordKey": "2026-08-19",
                "source": "ccusage",
                "machineName": "host",
                "modelName": "claude-sonnet",
                "inputTokens": 250777,
                "outputTokens": 10,
                "cacheCreationTokens": 3,
                "cacheReadTokens": 4,
                "totalTokens": 250794,
                "totalCost": 1.5
            }]
        });
        let parsed = parse_ingest_bytes(body.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.events.len(), 1);
        let row = &parsed.events[0];
        assert_eq!(row.record_type, "daily");
        assert_eq!(row.input_tokens, 250777);
        assert_eq!(row.cache_creation_tokens, 3);
        assert_eq!(row.cache_read_tokens, 4);
        assert_eq!(row.total_tokens, 250794);
        assert!((row.cost - 1.5).abs() < 1e-12);
    }

    #[test]
    fn parse_ingest_does_not_invent_cost() {
        let body = serde_json::json!({
            "events": [{
                "date": "2026-08-19",
                "record_type": "session",
                "source": "grok",
                "input_tokens": 100,
                "output_tokens": 20,
                "total_tokens": 120
            }]
        });
        let parsed = parse_ingest_bytes(body.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.events[0].cost, 0.0);
        assert_eq!(parsed.events[0].total_tokens, 120);
    }

    #[test]
    fn parse_ingest_too_many_events_is_413() {
        let events: Vec<_> = (0..MAX_INGEST_EVENTS + 1)
            .map(|i| serde_json::json!({"date": "2026-08-19", "record_type": "daily", "record_key": i.to_string()}))
            .collect();
        let body = serde_json::json!({ "events": events });
        let err = parse_ingest_bytes(body.to_string().as_bytes()).unwrap_err();
        assert_eq!(err, IngestParseError::TooManyEvents);
        assert_eq!(err.status(), 413);
    }

    #[test]
    fn parse_ingest_null_strings_do_not_fail_the_body() {
        let body = serde_json::json!({
            "events": [{
                "date": "2026-08-19",
                "record_type": "session",
                "source": "cursor-cloud-agent",
                "model_name": null,
                "session_id": null,
                "start_time": "not-a-timestamp",
                "end_time": "2026-08-19 01:02:03",
                "is_active": true,
                "entries": 2.0,
                "cost": 0
            }]
        });
        let parsed = parse_ingest_bytes(body.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.events.len(), 1);
        let row = &parsed.events[0];
        assert!(row.model_name.is_empty());
        assert!(row.start_time.is_none());
        assert_eq!(row.end_time.as_deref(), Some("2026-08-19 01:02:03"));
        assert_eq!(row.is_active, 1);
        assert_eq!(row.entries, 2);
        assert_eq!(row.cost, 0.0);
    }

    #[test]
    fn ingest_parse_error_status_is_never_500() {
        for err in [
            IngestParseError::InvalidJson,
            IngestParseError::EventsNotArray,
            IngestParseError::TooLarge,
            IngestParseError::TooManyEvents,
        ] {
            assert!(err.status() < 500, "{err:?} must be 4xx");
        }
    }

    #[test]
    fn ingest_body_strict_serde_is_not_the_http_path() {
        let row = r#"{"date":"2026-08-19","record_type":"daily","input_tokens":1.5,"cost":"0.25"}"#;
        assert!(
            serde_json::from_str::<EventRow>(row).is_err(),
            "strict EventRow serde must not be used for /v1/ingest"
        );
        let parsed = parse_ingest_bytes(format!(r#"{{"events":[{row}]}}"#).as_bytes()).unwrap();
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].input_tokens, 1);
        assert!((parsed.events[0].cost - 0.25).abs() < 1e-12);
    }
}
