/**
 * Companion (ccusage agent subcommand) data types and JSON normalization.
 *
 * ccusage 20.x exposes every agent as a subcommand (`ccusage <source> <view> --json`).
 * The JSON shape varies by source (field aliases), so we normalize into a
 * canonical `CompanionUsageRow` + `CompanionModelBreakdown` before row building.
 */

use serde_json::{Map, Value};

/// Companion model breakdown with reasoning tokens (Codex includes them).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompanionModelBreakdown {
    pub model_name: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
    pub extra_total_tokens: u64,
    pub cost: f64,
}

/// A single normalized usage row (daily / session / monthly).
#[derive(Debug, Clone, Default)]
pub struct CompanionUsageRow {
    pub date: Option<String>,
    pub last_activity: Option<String>,
    pub session_id: Option<String>,
    pub project_path: Option<String>,
    pub month: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub models_used: Vec<String>,
    pub model_breakdowns: Vec<CompanionModelBreakdown>,
}

/// Companion data returned by the fetcher: daily + monthly + session rows.
#[derive(Debug, Clone, Default)]
pub struct CompanionData {
    pub daily: Vec<CompanionUsageRow>,
    pub monthly: Vec<CompanionUsageRow>,
    pub session: Vec<CompanionUsageRow>,
}

/// Which companion command was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionCommand {
    Daily,
    Monthly,
    Session,
}

impl CompanionCommand {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompanionCommand::Daily => "daily",
            CompanionCommand::Monthly => "monthly",
            CompanionCommand::Session => "session",
        }
    }

    /// The JSON key that wraps the array for this command.
    pub fn response_key(&self) -> &'static str {
        match self {
            CompanionCommand::Daily => "daily",
            CompanionCommand::Monthly => "monthly",
            CompanionCommand::Session => "sessions",
        }
    }
}

/// All ccusage agent subcommands to import (Claude handled separately).
pub const CCUSAGE_AGENT_SOURCES: &[&str] = &[
    "codex", "opencode", "gemini", "openclaw", "amp", "droid", "codebuff",
    "pi", "goose", "kilo", "copilot", "kimi", "qwen",
];

/// Path-env var mapping for sources that support custom data dirs.
pub fn source_path_env(source: &str) -> Option<&'static str> {
    match source {
        "codex" => Some("CODEX_HOME"),
        "opencode" => Some("OPENCODE_DATA_DIR"),
        "gemini" => Some("GEMINI_DATA_DIR"),
        "openclaw" => Some("OPENCLAW_DIR"),
        _ => None,
    }
}

/// Companion command executor options (for dependency-injected testing).
#[derive(Debug, Clone)]
pub struct CompanionCommandOptions {
    pub source: String,
    pub command: CompanionCommand,
    pub runner: String, // "npx" or "bunx"
    pub timeout_ms: u64,
    pub env: Map<String, Value>,
    pub date_flags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Normalization helpers — mirror the TS normalizeUsageRow / normalizeModelBreakdowns
// ---------------------------------------------------------------------------

/// Get a numeric field from a JSON object, checking aliases.
fn get_number(obj: &Map<String, Value>, keys: &[&str]) -> u64 {
    for k in keys {
        if let Some(v) = obj.get(*k) {
            if let Some(n) = v.as_f64() {
                return n as u64;
            }
        }
    }
    0
}

/// Get a f64 cost field from a JSON object, checking aliases.
fn get_cost(obj: &Map<String, Value>, keys: &[&str]) -> f64 {
    for k in keys {
        if let Some(v) = obj.get(*k) {
            if let Some(n) = v.as_f64() {
                return n;
            }
        }
    }
    0.0
}

/// Get a string field from a JSON object, checking aliases.
fn get_string<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = obj.get(*k) {
            if let Some(s) = v.as_str() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Get a string field or fallback.
fn get_string_or<'a>(obj: &'a Map<String, Value>, keys: &[&str], fallback: &str) -> String {
    get_string(obj, keys).unwrap_or_else(|| fallback.to_string())
}

/// Normalize a raw JSON value into a `CompanionUsageRow`.
///
/// Mirrors the TS `normalizeUsageRow(command, row)`.
pub fn normalize_usage_row(command: CompanionCommand, raw: &Value) -> CompanionUsageRow {
    let obj = match raw.as_object() {
        Some(o) => o,
        None => {
            return CompanionUsageRow {
                models_used: vec![],
                model_breakdowns: vec![],
                ..Default::default()
            };
        }
    };

    let raw_models = obj.get("modelBreakdowns").or_else(|| obj.get("models")).unwrap_or(&Value::Null);
    let model_breakdowns = normalize_model_breakdowns(raw_models);

    let models_used = match obj.get("modelsUsed") {
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        _ => model_breakdowns.iter().map(|m| m.model_name.clone()).collect(),
    };

    let mut row = CompanionUsageRow {
        input_tokens: get_number(obj, &["inputTokens", "input_tokens"]),
        output_tokens: get_number(obj, &["outputTokens", "output_tokens"]),
        cache_creation_tokens: get_number(obj, &[
            "cacheCreationTokens", "cacheCreationInputTokens", "cache_creation_tokens",
        ]),
        cache_read_tokens: get_number(obj, &[
            "cacheReadTokens", "cacheReadInputTokens", "cache_read_tokens", "cachedInputTokens",
        ]),
        reasoning_tokens: get_number(obj, &[
            "reasoningTokens", "reasoningOutputTokens", "thoughtsTokens", "reasoning_tokens",
        ]),
        total_tokens: get_number(obj, &["totalTokens", "total_tokens"]),
        total_cost: get_cost(obj, &["totalCost", "costUSD", "cost", "total_cost"]),
        models_used,
        model_breakdowns,
        ..Default::default()
    };

    // Monthly: construct month from numeric month + year if needed
    if command == CompanionCommand::Monthly {
        if row.month.is_none() {
            let month = get_number(obj, &["month"]);
            let year = get_number(obj, &["year"]);
            if month > 0 && year > 0 {
                row.month = Some(format!("{}-{:02}", year, month));
            } else if let Some(m) = get_string(obj, &["month"]) {
                row.month = Some(m);
            }
        }
    }

    // Session: resolve session ID, project path, last activity
    if command == CompanionCommand::Session {
        let sid = get_string_or(obj, &["sessionId", "session", "id", "session_id"], "unknown");
        row.session_id = Some(sid.clone());
        row.project_path = Some(get_string_or(obj, &["projectPath", "directory", "path", "project_path"], &sid));
        row.last_activity = get_string(obj, &["lastActivity", "last_activity", "date"]);
        if row.last_activity.is_none() {
            row.last_activity = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    // Also capture date/lastActivity for daily/monthly
    if row.date.is_none() {
        row.date = get_string(obj, &["date"]);
    }

    row
}

/// Normalize raw model breakdown data into typed breakdowns.
///
/// Handles three input shapes (mirrors TS `normalizeModelBreakdowns`):
/// - Array of objects: `[{ modelName: "m", input_tokens: 5, ... }]`
/// - Array of strings: `["gpt-5"]` (zero counts)
/// - Object map: `{ "gpt-5": { inputTokens: 3, ... } }`
pub fn normalize_model_breakdowns(raw: &Value) -> Vec<CompanionModelBreakdown> {
    if let Some(arr) = raw.as_array() {
        return arr.iter().map(|item| {
            if let Some(s) = item.as_str() {
                // Array of strings
                CompanionModelBreakdown {
                    model_name: s.to_string(),
                    ..Default::default()
                }
            } else if let Some(obj) = item.as_object() {
                // Array of objects
                CompanionModelBreakdown {
                    model_name: get_string_or(obj, &["modelName", "model", "name"], "unknown"),
                    input_tokens: get_number(obj, &["inputTokens", "input_tokens"]),
                    output_tokens: get_number(obj, &["outputTokens", "output_tokens"]),
                    cache_creation_tokens: get_number(obj, &[
                        "cacheCreationTokens", "cacheCreationInputTokens", "cache_creation_tokens",
                    ]),
                    cache_read_tokens: get_number(obj, &[
                        "cacheReadTokens", "cacheReadInputTokens", "cache_read_tokens",
                        "cachedInputTokens",
                    ]),
                    reasoning_tokens: get_number(obj, &[
                        "reasoningTokens", "reasoningOutputTokens", "thoughtsTokens",
                        "reasoning_tokens",
                    ]),
                    extra_total_tokens: get_number(obj, &["extraTotalTokens", "extra_total_tokens"]),
                    cost: get_cost(obj, &["cost", "costUSD", "totalCost"]),
                }
            } else {
                CompanionModelBreakdown::default()
            }
        }).collect();
    }

    // Object map: { "model-name": { inputTokens, outputTokens, ... } }
    if let Some(obj) = raw.as_object() {
        let empty_map: Map<String, Value> = Map::new();
        return obj.iter().map(|(model_name, value)| {
            let v = value.as_object().unwrap_or(&empty_map);
            CompanionModelBreakdown {
                model_name: model_name.clone(),
                input_tokens: get_number(v, &["inputTokens", "input_tokens"]),
                output_tokens: get_number(v, &["outputTokens", "output_tokens"]),
                cache_creation_tokens: get_number(v, &[
                    "cacheCreationTokens", "cacheCreationInputTokens", "cache_creation_tokens",
                ]),
                cache_read_tokens: get_number(v, &[
                    "cacheReadTokens", "cacheReadInputTokens", "cache_read_tokens",
                    "cachedInputTokens",
                ]),
                reasoning_tokens: get_number(v, &[
                    "reasoningTokens", "reasoningOutputTokens", "thoughtsTokens",
                    "reasoning_tokens",
                ]),
                extra_total_tokens: get_number(v, &["extraTotalTokens", "extra_total_tokens"]),
                cost: get_cost(v, &["cost", "costUSD", "totalCost"]),
            }
        }).collect();
    }

    vec![]
}

/// Unwrap a raw ccusage JSON response into an array of row values.
///
/// Mirrors TS `normalizeCompanionRows`: handles array, wrapped object
/// (`{daily: [...]}`), or `{data: [...]}`.
pub fn normalize_companion_rows(command: CompanionCommand, raw: &Value) -> Vec<Value> {
    let key = command.response_key(); // "daily" | "monthly" | "sessions"
    let rows = match raw {
        Value::Array(arr) => arr.clone(),
        Value::Object(obj) => {
            if let Some(arr) = obj.get(key) {
                arr.as_array().cloned().unwrap_or_default()
            } else if let Some(arr) = obj.get("data") {
                arr.as_array().cloned().unwrap_or_default()
            } else {
                vec![]
            }
        }
        _ => vec![],
    };

    if rows.is_empty() || !rows[0].is_array() && !rows[0].is_object() {
        // If the single element is an array, unwrap it
        if rows.len() == 1 {
            if let Some(inner) = rows[0].as_array() {
                return inner.clone();
            }
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_codex_cached_input_tokens() {
        let row = normalize_usage_row(CompanionCommand::Daily, &json!({
            "inputTokens": 100,
            "cachedInputTokens": 50
        }));
        assert_eq!(row.input_tokens, 100);
        assert_eq!(row.cache_read_tokens, 50);
    }

    #[test]
    fn normalize_reasoning_aliases() {
        let row = normalize_usage_row(CompanionCommand::Daily, &json!({"reasoningOutputTokens": 9}));
        assert_eq!(row.reasoning_tokens, 9);
        let row = normalize_usage_row(CompanionCommand::Daily, &json!({"thoughtsTokens": 11}));
        assert_eq!(row.reasoning_tokens, 11);
        let row = normalize_usage_row(CompanionCommand::Daily, &json!({"reasoning_tokens": 13}));
        assert_eq!(row.reasoning_tokens, 13);
    }

    #[test]
    fn normalize_session_alias_fallback() {
        let row = normalize_usage_row(CompanionCommand::Session, &json!({
            "id": "s1", "directory": "/repo", "date": "2026-01-01"
        }));
        assert_eq!(row.session_id.as_deref(), Some("s1"));
        assert_eq!(row.project_path.as_deref(), Some("/repo"));
        assert_eq!(row.last_activity.as_deref(), Some("2026-01-01"));
    }

    #[test]
    fn normalize_session_project_fallback_to_id() {
        let row = normalize_usage_row(CompanionCommand::Session, &json!({"sessionId": "abc"}));
        assert_eq!(row.session_id.as_deref(), Some("abc"));
        assert_eq!(row.project_path.as_deref(), Some("abc"));
    }

    #[test]
    fn normalize_non_object_returns_empty() {
        let row = normalize_usage_row(CompanionCommand::Daily, &json!(null));
        assert!(row.models_used.is_empty());
        assert!(row.model_breakdowns.is_empty());
    }

    #[test]
    fn normalize_model_breakdowns_aliases() {
        let out = normalize_model_breakdowns(&json!([
            {"model": "m", "input_tokens": 5, "cachedInputTokens": 7, "extraTotalTokens": 9}
        ]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model_name, "m");
        assert_eq!(out[0].input_tokens, 5);
        assert_eq!(out[0].cache_read_tokens, 7);
        assert_eq!(out[0].extra_total_tokens, 9);
    }

    #[test]
    fn normalize_model_breakdowns_strings() {
        let out = normalize_model_breakdowns(&json!(["gpt-5"]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model_name, "gpt-5");
        assert_eq!(out[0].input_tokens, 0);
        assert_eq!(out[0].cost, 0.0);
    }

    #[test]
    fn normalize_model_breakdowns_object_map() {
        let out = normalize_model_breakdowns(&json!({
            "gpt-5": {"inputTokens": 3, "outputTokens": 4}
        }));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model_name, "gpt-5");
        assert_eq!(out[0].input_tokens, 3);
        assert_eq!(out[0].output_tokens, 4);
    }

    #[test]
    fn normalize_model_breakdowns_null() {
        let out = normalize_model_breakdowns(&Value::Null);
        assert!(out.is_empty());
    }
}
