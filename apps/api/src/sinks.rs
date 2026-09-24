use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

use crate::auth::opt_secret;
use crate::types::{sql_literal, AnalyticsPoint, EventRow, PingSample, SinkAck};

fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

pub fn clickhouse_configured(env: &Env) -> bool {
    !opt_secret(env, "CH_HOST").is_empty()
}

pub fn motherduck_configured(env: &Env) -> bool {
    !opt_secret(env, "MOTHERDUCK_TOKEN").is_empty()
}

pub async fn fanout_write(env: &Env, rows: &[EventRow]) -> Vec<SinkAck> {
    let mut out = Vec::new();
    if clickhouse_configured(env) {
        out.push(write_clickhouse(env, rows).await);
    }
    if motherduck_configured(env) {
        out.push(write_motherduck(env, rows).await);
    }
    out
}

pub async fn collect_pings(env: &Env) -> Vec<PingSample> {
    let mut out = Vec::new();
    if clickhouse_configured(env) {
        out.push(timed("clickhouse", ping_clickhouse(env)).await);
    }
    if motherduck_configured(env) {
        out.push(timed("motherduck", ping_motherduck(env)).await);
    }
    out
}

async fn timed<F>(name: &str, fut: F) -> PingSample
where
    F: core::future::Future<Output = std::result::Result<(), String>>,
{
    let start = now_ms();
    match fut.await {
        Ok(()) => PingSample {
            name: name.into(),
            ok: true,
            latency_ms: now_ms().saturating_sub(start),
            error: None,
        },
        Err(e) => PingSample {
            name: name.into(),
            ok: false,
            latency_ms: now_ms().saturating_sub(start),
            error: Some(e),
        },
    }
}

async fn ping_clickhouse(env: &Env) -> std::result::Result<(), String> {
    clickhouse_query(env, "SELECT 1").await.map(|_| ())
}

async fn ping_motherduck(env: &Env) -> std::result::Result<(), String> {
    motherduck_query(env, "SELECT 1").await.map(|_| ())
}

async fn write_clickhouse(env: &Env, rows: &[EventRow]) -> SinkAck {
    let start = now_ms();
    if let Err(e) = ensure_ch_columns(env).await {
        return ack("clickhouse", 0, start, Some(e));
    }
    if rows.is_empty() {
        return ack("clickhouse", 0, start, None);
    }
    let account_id = rows.first().map(|r| r.account_id.as_str()).unwrap_or("");
    if account_id.is_empty() {
        return ack("clickhouse", 0, start, Some("missing account_id".into()));
    }
    let keys: Vec<String> = rows
        .iter()
        .map(|r| r.dedup_key.clone())
        .filter(|k| !k.is_empty())
        .collect();
    for chunk in keys.chunks(200) {
        let Some(sql) = dedup_delete_sql(account_id, chunk, true) else {
            continue;
        };
        if let Err(e) = clickhouse_query(env, &sql).await {
            return ack("clickhouse", 0, start, Some(e));
        }
    }
    for chunk in rows.chunks(500) {
        if let Err(e) = clickhouse_insert(env, chunk).await {
            return ack("clickhouse", 0, start, Some(e));
        }
    }
    ack("clickhouse", rows.len() as u64, start, None)
}

async fn write_motherduck(env: &Env, rows: &[EventRow]) -> SinkAck {
    let start = now_ms();
    if let Err(e) = ensure_md_table(env).await {
        return ack("motherduck", 0, start, Some(e));
    }
    if rows.is_empty() {
        return ack("motherduck", 0, start, None);
    }
    let account_id = rows.first().map(|r| r.account_id.as_str()).unwrap_or("");
    if account_id.is_empty() {
        return ack("motherduck", 0, start, Some("missing account_id".into()));
    }
    let keys: Vec<String> = rows
        .iter()
        .map(|r| r.dedup_key.clone())
        .filter(|k| !k.is_empty())
        .collect();
    for chunk in keys.chunks(200) {
        let Some(sql) = dedup_delete_sql(account_id, chunk, false) else {
            continue;
        };
        if let Err(e) = motherduck_query(env, &sql).await {
            return ack("motherduck", 0, start, Some(e));
        }
    }
    for chunk in rows.chunks(200) {
        if let Err(e) = motherduck_query(env, &motherduck_insert_sql(chunk)).await {
            return ack("motherduck", 0, start, Some(e));
        }
    }
    ack("motherduck", rows.len() as u64, start, None)
}

fn ack(name: &str, rows: u64, start: u64, error: Option<String>) -> SinkAck {
    SinkAck {
        name: name.into(),
        rows,
        duration_ms: now_ms().saturating_sub(start),
        error,
    }
}

async fn ensure_ch_columns(env: &Env) -> std::result::Result<(), String> {
    for sql in [
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS account_id String DEFAULT ''",
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS api_key_id String DEFAULT ''",
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS dedup_key String DEFAULT ''",
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS import_id String DEFAULT ''",
    ] {
        let _ = clickhouse_query(env, sql).await;
    }
    Ok(())
}

async fn ensure_md_table(env: &Env) -> std::result::Result<(), String> {
    motherduck_query(env, MD_CREATE_TABLE).await?;
    let _ = motherduck_query(
        env,
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS account_id VARCHAR DEFAULT ''",
    )
    .await;
    let _ = motherduck_query(
        env,
        "ALTER TABLE ccusage_events ADD COLUMN IF NOT EXISTS api_key_id VARCHAR DEFAULT ''",
    )
    .await;
    Ok(())
}

const MD_CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS ccusage_events (\
 date DATE NOT NULL, record_type VARCHAR NOT NULL, record_key VARCHAR NOT NULL, \
 source VARCHAR NOT NULL DEFAULT 'ccusage', machine_name VARCHAR NOT NULL, \
 account_id VARCHAR DEFAULT '', api_key_id VARCHAR DEFAULT '', \
 model_name VARCHAR DEFAULT '', session_id VARCHAR DEFAULT '', project_path VARCHAR DEFAULT '', \
 input_tokens BIGINT DEFAULT 0, output_tokens BIGINT DEFAULT 0, \
 cache_creation_tokens BIGINT DEFAULT 0, cache_read_tokens BIGINT DEFAULT 0, \
 reasoning_tokens BIGINT DEFAULT 0, total_tokens BIGINT DEFAULT 0, cost DOUBLE DEFAULT 0, \
 dedup_key VARCHAR DEFAULT '', import_id VARCHAR DEFAULT '', block_id VARCHAR DEFAULT '', \
 start_time TIMESTAMP, end_time TIMESTAMP, actual_end_time TIMESTAMP, \
 is_active SMALLINT DEFAULT 0, is_gap SMALLINT DEFAULT 0, entries INTEGER DEFAULT 0, \
 burn_rate DOUBLE DEFAULT 0, projection DOUBLE DEFAULT 0, usage_limit_reset_time TIMESTAMP, \
 created_at TIMESTAMP DEFAULT current_timestamp, updated_at TIMESTAMP DEFAULT current_timestamp)";

const EVENT_COLS: &str = "date, record_type, record_key, source, machine_name, \
account_id, api_key_id, model_name, session_id, project_path, input_tokens, output_tokens, \
cache_creation_tokens, cache_read_tokens, reasoning_tokens, total_tokens, cost, dedup_key, \
import_id, block_id, start_time, end_time, actual_end_time, is_active, is_gap, entries, \
burn_rate, projection, usage_limit_reset_time, created_at, updated_at";

fn sql_opt_ts(value: &Option<String>) -> String {
    match value {
        Some(v) if !v.is_empty() => sql_literal(v),
        _ => "NULL".into(),
    }
}

fn sql_f64(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        "0".into()
    }
}

fn event_values_sql(row: &EventRow) -> String {
    format!(
        "({},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{})",
        sql_literal(&row.date),
        sql_literal(&row.record_type),
        sql_literal(&row.record_key),
        sql_literal(&row.source),
        sql_literal(&row.machine_name),
        sql_literal(&row.account_id),
        sql_literal(&row.api_key_id),
        sql_literal(&row.model_name),
        sql_literal(&row.session_id),
        sql_literal(&row.project_path),
        row.input_tokens,
        row.output_tokens,
        row.cache_creation_tokens,
        row.cache_read_tokens,
        row.reasoning_tokens,
        row.total_tokens,
        sql_f64(row.cost),
        sql_literal(&row.dedup_key),
        sql_literal(&row.import_id),
        sql_literal(&row.block_id),
        sql_opt_ts(&row.start_time),
        sql_opt_ts(&row.end_time),
        sql_opt_ts(&row.actual_end_time),
        row.is_active,
        row.is_gap,
        row.entries,
        sql_f64(row.burn_rate),
        sql_f64(row.projection),
        sql_opt_ts(&row.usage_limit_reset_time),
        sql_literal(&row.created_at),
        sql_literal(&row.updated_at),
    )
}

pub fn dedup_delete_sql(account_id: &str, keys: &[String], clickhouse: bool) -> Option<String> {
    if account_id.is_empty() || keys.is_empty() {
        return None;
    }
    let list = keys.iter().map(|k| sql_literal(k)).collect::<Vec<_>>().join(",");
    let account = sql_literal(account_id);
    if clickhouse {
        Some(format!(
            "ALTER TABLE ccusage_events DELETE WHERE account_id = {account} AND dedup_key IN ({list})"
        ))
    } else {
        Some(format!(
            "DELETE FROM ccusage_events WHERE account_id = {account} AND dedup_key IN ({list})"
        ))
    }
}

pub fn motherduck_insert_sql(rows: &[EventRow]) -> String {
    let values = rows.iter().map(event_values_sql).collect::<Vec<_>>().join(",");
    format!("INSERT INTO ccusage_events ({EVENT_COLS}) VALUES {values}")
}

pub fn clickhouse_default_port(proto: &str, port: &str) -> String {
    if !port.is_empty() {
        return port.into();
    }
    if proto == "https" {
        "443".into()
    } else {
        "8123".into()
    }
}

fn clickhouse_base_url(env: &Env) -> String {
    let host = opt_secret(env, "CH_HOST");
    let proto = opt_secret(env, "CH_PROTOCOL");
    let proto = if proto.is_empty() { "http".into() } else { proto };
    let port = clickhouse_default_port(&proto, &opt_secret(env, "CH_PORT"));
    format!("{proto}://{host}:{port}/")
}

fn build_clickhouse_url(base_url: &str, database: &str, query: Option<&str>) -> String {
    let mut url = base_url.to_string();
    let mut params = Vec::new();
    if let Some(query) = query {
        params.push(format!("query={}", urlencoding(query)));
    }
    if !database.is_empty() {
        params.push(format!("database={}", urlencoding(database)));
    }
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }
    url
}

fn clickhouse_request_url(env: &Env, query: Option<&str>) -> String {
    build_clickhouse_url(
        &clickhouse_base_url(env),
        &opt_secret(env, "CH_DATABASE"),
        query,
    )
}

fn apply_clickhouse_headers(env: &Env, headers: &Headers) -> std::result::Result<(), String> {
    let user = opt_secret(env, "CH_USER");
    let pass = opt_secret(env, "CH_PASSWORD");
    if !user.is_empty() || !pass.is_empty() {
        let b64 = base64(&format!("{user}:{pass}"));
        headers
            .set("authorization", &format!("Basic {b64}"))
            .map_err(|e| e.to_string())?;
    }
    let id = opt_secret(env, "CF_ACCESS_CLIENT_ID");
    let id = if id.is_empty() {
        opt_secret(env, "CH_ACCESS_CLIENT_ID")
    } else {
        id
    };
    let secret = opt_secret(env, "CF_ACCESS_CLIENT_SECRET");
    let secret = if secret.is_empty() {
        opt_secret(env, "CH_ACCESS_CLIENT_SECRET")
    } else {
        secret
    };
    if clickhouse_access_configured(&id, &secret) {
        headers
            .set("CF-Access-Client-Id", &id)
            .map_err(|e| e.to_string())?;
        headers
            .set("CF-Access-Client-Secret", &secret)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn clickhouse_access_configured(id: &str, secret: &str) -> bool {
    !id.is_empty() && !secret.is_empty()
}

pub async fn clickhouse_query(env: &Env, sql: &str) -> std::result::Result<String, String> {
    let url = clickhouse_request_url(env, None);
    clickhouse_post(env, &url, sql).await
}

async fn clickhouse_insert(env: &Env, rows: &[EventRow]) -> std::result::Result<(), String> {
    let mut body = String::new();
    for row in rows {
        body.push_str(&serde_json::to_string(row).map_err(|e| e.to_string())?);
        body.push('\n');
    }
    let url = clickhouse_request_url(
        env,
        Some("INSERT INTO ccusage_events FORMAT JSONEachRow"),
    );
    clickhouse_post(env, &url, &body).await.map(|_| ())
}

async fn clickhouse_post(env: &Env, url: &str, body: &str) -> std::result::Result<String, String> {
    let headers = Headers::new();
    headers
        .set("content-type", "text/plain; charset=UTF-8")
        .map_err(|e| e.to_string())?;
    apply_clickhouse_headers(env, &headers)?;
    fetch_post(url, headers, body).await
}

fn motherduck_sql_url(env: &Env) -> String {
    let url = opt_secret(env, "MOTHERDUCK_SQL_URL");
    if url.is_empty() || url.ends_with("/v1/query") {
        "https://api.motherduck.com/mcp".into()
    } else {
        url
    }
}

fn motherduck_tool(sql: &str) -> &'static str {
    let head = sql
        .trim_start()
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_uppercase();
    if head.starts_with("SELECT") || head.starts_with("SHOW") || head.starts_with("DESCRIBE") {
        "query"
    } else {
        "query_rw"
    }
}

pub async fn motherduck_query(env: &Env, sql: &str) -> std::result::Result<String, String> {
    let token = opt_secret(env, "MOTHERDUCK_TOKEN");
    let database = opt_secret(env, "MOTHERDUCK_DATABASE");
    let database = if database.is_empty() {
        "ccusage".into()
    } else {
        database
    };
    let url = motherduck_sql_url(env);
    if url.contains("/mcp") {
        return motherduck_mcp(&url, &token, &database, sql).await;
    }
    let body = serde_json::json!({ "sql": format!("USE {database};\n{sql}"), "database": database });
    http_post_json(&url, &body.to_string(), Some(&token)).await
}

async fn motherduck_mcp(
    url: &str,
    token: &str,
    database: &str,
    sql: &str,
) -> std::result::Result<String, String> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": motherduck_tool(sql),
            "arguments": { "database": database, "sql": sql }
        }
    });
    let headers = Headers::new();
    headers
        .set("content-type", "application/json")
        .map_err(|e| e.to_string())?;
    headers
        .set("accept", "application/json, text/event-stream")
        .map_err(|e| e.to_string())?;
    headers
        .set("authorization", &format!("Bearer {token}"))
        .map_err(|e| e.to_string())?;
    let text = fetch_post(url, headers, &payload.to_string()).await?;
    parse_mcp_result(&text)
}

pub fn parse_mcp_result(text: &str) -> std::result::Result<String, String> {
    let text = unwrap_sse(text);
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("motherduck mcp json: {e}"))?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("mcp error");
        return Err(msg.into());
    }
    let result = v.get("result").cloned().unwrap_or(v);
    if result.get("isError").and_then(|x| x.as_bool()) == Some(true) {
        let msg = result
            .pointer("/content/0/text")
            .and_then(|x| x.as_str())
            .unwrap_or("mcp tool error");
        return Err(msg.chars().take(400).collect());
    }
    let sc = result.get("structuredContent").cloned().unwrap_or(result);
    if sc.get("success").and_then(|x| x.as_bool()) == Some(false) {
        let msg = sc
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("motherduck query failed");
        return Err(msg.into());
    }
    if let Some(objs) = mcp_rows_as_objects(&sc) {
        return serde_json::to_string(&objs).map_err(|e| e.to_string());
    }
    Ok(sc.to_string())
}

fn unwrap_sse(text: &str) -> String {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("event:") && !trimmed.starts_with("data:") {
        return text.to_string();
    }
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() {
        text.to_string()
    } else {
        data
    }
}

fn mcp_rows_as_objects(sc: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    let cols = sc.get("columns")?.as_array()?;
    let rows = sc.get("rows")?.as_array()?;
    let names: Vec<String> = cols
        .iter()
        .map(|c| c.as_str().unwrap_or("").to_string())
        .collect();
    Some(
        rows.iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                if let Some(vals) = row.as_array() {
                    for (i, name) in names.iter().enumerate() {
                        if name.is_empty() {
                            continue;
                        }
                        obj.insert(name.clone(), vals.get(i).cloned().unwrap_or(serde_json::Value::Null));
                    }
                }
                serde_json::Value::Object(obj)
            })
            .collect(),
    )
}

async fn http_post_json(url: &str, body: &str, bearer: Option<&str>) -> std::result::Result<String, String> {
    let headers = Headers::new();
    headers
        .set("content-type", "application/json")
        .map_err(|e| e.to_string())?;
    if let Some(t) = bearer {
        headers
            .set("authorization", &format!("Bearer {t}"))
            .map_err(|e| e.to_string())?;
    }
    fetch_post(url, headers, body).await
}

async fn fetch_post(url: &str, headers: Headers, body: &str) -> std::result::Result<String, String> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_headers(headers);
    init.with_body(Some(wasm_bindgen::JsValue::from_str(body)));
    let req = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
    let mut resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;
    let status = resp.status_code();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !(200..300).contains(&status) {
        return Err(http_error(status, &text));
    }
    Ok(text)
}

pub fn http_error(status: u16, body: &str) -> String {
    let snippet: String = body.chars().take(400).collect();
    let mut msg = format!("http {status}: {snippet}");
    if status == 1003
        || snippet.contains("1003")
        || snippet.to_ascii_lowercase().contains("direct ip")
    {
        msg.push_str(
            " (Workers cannot fetch raw IPs; set CH_HOST to a public hostname)",
        );
    }
    msg
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn base64(input: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(T[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < bytes.len() {
            out.push(T[(b2 & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn json_u64(v: &serde_json::Value, key: &str) -> u64 {
    let Some(x) = v.get(key) else {
        return 0;
    };
    if let Some(n) = x.as_u64() {
        return n;
    }
    if let Some(n) = x.as_i64() {
        return n.max(0) as u64;
    }
    if let Some(n) = x.as_f64() {
        return n.max(0.0) as u64;
    }
    if let Some(s) = x.as_str() {
        return s
            .parse::<f64>()
            .ok()
            .map(|n| n.max(0.0) as u64)
            .unwrap_or(0);
    }
    0
}

fn point_from_json(v: &serde_json::Value) -> Option<AnalyticsPoint> {
    if !v.is_object() {
        return None;
    }
    Some(AnalyticsPoint {
        date: v.get("date").and_then(|x| x.as_str()).unwrap_or("").into(),
        source: v.get("source").and_then(|x| x.as_str()).unwrap_or("").into(),
        model_name: v.get("model_name").and_then(|x| x.as_str()).unwrap_or("").into(),
        cost: v.get("cost").and_then(|x| x.as_f64()).unwrap_or(0.0),
        total_tokens: json_u64(v, "total_tokens"),
        entries: json_u64(v, "entries"),
    })
}

fn points_from_array(arr: &[serde_json::Value]) -> Option<Vec<AnalyticsPoint>> {
    if arr.iter().any(|v| !v.is_object()) {
        return None;
    }
    Some(arr.iter().filter_map(point_from_json).collect())
}

pub fn parse_analytics_payload(text: &str) -> Vec<AnalyticsPoint> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(arr) = v.as_array() {
            if let Some(points) = points_from_array(arr) {
                return points;
            }
        }
        for key in ["data", "rows", "result"] {
            if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
                if let Some(points) = points_from_array(arr) {
                    return points;
                }
            }
        }
        if let Some(point) = point_from_json(&v) {
            if !point.date.is_empty() || !point.source.is_empty() {
                return vec![point];
            }
        }
    }
    parse_json_each_row(text)
}

pub fn parse_json_each_row(text: &str) -> Vec<AnalyticsPoint> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(point) = point_from_json(&v) {
            out.push(point);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> EventRow {
        EventRow {
            date: "2026-08-19".into(),
            record_type: "daily".into(),
            record_key: "k1".into(),
            source: "cursor".into(),
            machine_name: "account".into(),
            account_id: "acc".into(),
            model_name: "grok".into(),
            total_tokens: 10,
            cost: 1.5,
            dedup_key: "deadbeef".into(),
            created_at: "2026-08-19 00:00:00".into(),
            updated_at: "2026-08-19 00:00:00".into(),
            ..EventRow::default()
        }
    }

    #[test]
    fn now_ms_is_nonzero_on_native() {
        assert!(now_ms() > 0);
    }

    #[test]
    fn dedup_delete_requires_account_and_scopes_keys() {
        assert!(dedup_delete_sql("", &["abc".into()], true).is_none());
        assert!(dedup_delete_sql("acc", &[], false).is_none());
        let ch = dedup_delete_sql("acc", &["k1".into(), "k'2".into()], true).unwrap();
        assert!(ch.starts_with("ALTER TABLE ccusage_events DELETE"));
        assert!(ch.contains("account_id = 'acc'"));
        assert!(ch.contains("dedup_key IN ('k1','k''2')"));
        let md = dedup_delete_sql("acc", &["k1".into()], false).unwrap();
        assert!(md.starts_with("DELETE FROM ccusage_events"));
        assert!(md.contains("account_id = 'acc'"));
    }

    #[test]
    fn motherduck_insert_contains_values_not_only_delete() {
        let sql = motherduck_insert_sql(&[sample_row()]);
        assert!(sql.starts_with("INSERT INTO ccusage_events"));
        assert!(sql.contains("'2026-08-19'"));
        assert!(sql.contains("'deadbeef'"));
        assert!(sql.contains("1.5"));
        assert!(!sql.contains("DELETE"));
    }

    #[test]
    fn motherduck_insert_escapes_quotes() {
        let mut row = sample_row();
        row.project_path = "it's".into();
        let sql = motherduck_insert_sql(&[row]);
        assert!(sql.contains("'it''s'"));
    }

    #[test]
    fn http_error_explains_direct_ip() {
        let msg = http_error(403, "error code: 1003");
        assert!(msg.contains("1003"));
        assert!(msg.contains("public hostname"));
    }

    #[test]
    fn parse_payload_json_each_row() {
        let text = "{\"date\":\"2026-01-01\",\"source\":\"cursor\",\"cost\":2.0,\"total_tokens\":9,\"entries\":1}\n";
        let points = parse_analytics_payload(text);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source, "cursor");
        assert_eq!(points[0].total_tokens, 9);
    }

    #[test]
    fn parse_payload_wrapped_rows() {
        let text = r#"{"data":[{"date":"2026-01-02","source":"grok","cost":3,"total_tokens":4,"entries":2}]}"#;
        let points = parse_analytics_payload(text);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source, "grok");
        assert_eq!(points[0].entries, 2);
    }

    #[test]
    fn motherduck_tool_select_is_readonly() {
        assert_eq!(motherduck_tool("SELECT 1"), "query");
        assert_eq!(motherduck_tool("INSERT INTO t VALUES (1)"), "query_rw");
        assert_eq!(motherduck_tool("DELETE FROM t"), "query_rw");
    }

    #[test]
    fn parse_mcp_select_rows() {
        let text = r#"{
            "jsonrpc":"2.0","id":2,
            "result":{
                "isError":false,
                "structuredContent":{
                    "success":true,
                    "columns":["date","source","cost","total_tokens","entries"],
                    "rows":[["2026-08-19","cursor",1.25,9,2]]
                }
            }
        }"#;
        let json = parse_mcp_result(text).unwrap();
        let points = parse_analytics_payload(&json);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source, "cursor");
        assert_eq!(points[0].total_tokens, 9);
        assert_eq!(points[0].entries, 2);
    }

    #[test]
    fn parse_mcp_float_counts() {
        let text = r#"{
            "result":{
                "structuredContent":{
                    "success":true,
                    "columns":["date","source","cost","total_tokens","entries"],
                    "rows":[["2026-08-19","grok",1.5,24800000.0,12.0]]
                }
            }
        }"#;
        let json = parse_mcp_result(text).unwrap();
        let points = parse_analytics_payload(&json);
        assert_eq!(points[0].total_tokens, 24_800_000);
        assert_eq!(points[0].entries, 12);
    }

    #[test]
    fn parse_mcp_error() {
        let text = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"nope"}}"#;
        assert!(parse_mcp_result(text).unwrap_err().contains("nope"));
    }

    #[test]
    fn https_defaults_to_443() {
        assert_eq!(clickhouse_default_port("https", ""), "443");
        assert_eq!(clickhouse_default_port("http", ""), "8123");
        assert_eq!(clickhouse_default_port("https", "8443"), "8443");
    }

    #[test]
    fn clickhouse_url_includes_database_for_reads_and_writes() {
        let read = build_clickhouse_url("https://clickhouse:8443/", "duyet analytics", None);
        assert_eq!(read, "https://clickhouse:8443/?database=duyet%20analytics");
        let write = build_clickhouse_url(
            "https://clickhouse:8443/",
            "duyet analytics",
            Some("INSERT INTO ccusage_events FORMAT JSONEachRow"),
        );
        assert!(write.contains("query=INSERT%20INTO%20ccusage_events%20FORMAT%20JSONEachRow"));
        assert!(write.contains("database=duyet%20analytics"));
    }

    #[test]
    fn access_needs_both_parts() {
        assert!(!clickhouse_access_configured("", "s"));
        assert!(!clickhouse_access_configured("id", ""));
        assert!(clickhouse_access_configured("id", "s"));
    }
}
