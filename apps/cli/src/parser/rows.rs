/**
 * Row builders: transform ccusage/companion data into flat EventRow vectors.
 *
 * Preserves all behavioral invariants from the TS `parsers.ts`:
 * - Breakdown rows: one per model per record, reasoning_tokens=0 for ccusage
 * - Block rows: use the source's own totalTokens (not the formula)
 * - Dedup keys: SHA-256 of `source|machine|record_type|date|model|record_key`
 * - total_tokens: input + output + cacheCreation + cacheRead (no reasoning),
 *   plus source-reported extra tokens for companion rows
 * - distribute_cost: proportional, last row absorbs rounding
 */

use crate::model::EventRow;
use crate::parser::companion::{CompanionData, CompanionModelBreakdown, CompanionUsageRow};
use crate::parser::types::{
    BlockUsage, DailyUsage, ProjectDailyUsage, SessionUsage, ModelBreakdown,
};
use crate::util::date::{ch_now, parse_date};
use crate::util::hash::{hash_project_name_sync, make_dedup_key};
use crate::util::tokens::total_tokens;
use crate::parser::cost::{distribute_cost, BreakdownForCost};
use serde_json::Value;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Breakdown fields shared by both ccusage and companion row building.
pub struct BreakdownInput {
    pub model_name: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub extra_total_tokens: u64,
    pub cost: f64,
}

impl From<&ModelBreakdown> for BreakdownInput {
    fn from(bd: &ModelBreakdown) -> Self {
        BreakdownInput {
            model_name: bd.model_name.clone(),
            input_tokens: bd.input_tokens,
            output_tokens: bd.output_tokens,
            cache_creation_tokens: bd.cache_creation_tokens,
            cache_read_tokens: bd.cache_read_tokens,
            extra_total_tokens: 0,
            cost: bd.cost,
        }
    }
}

impl From<&CompanionModelBreakdown> for BreakdownInput {
    fn from(bd: &CompanionModelBreakdown) -> Self {
        BreakdownInput {
            model_name: bd.model_name.clone(),
            input_tokens: bd.input_tokens,
            output_tokens: bd.output_tokens,
            cache_creation_tokens: bd.cache_creation_tokens,
            cache_read_tokens: bd.cache_read_tokens,
            extra_total_tokens: bd.extra_total_tokens,
            cost: bd.cost,
        }
    }
}

/// Common scope fields for a breakdown row.
pub struct RowScope {
    pub date: String,
    pub record_type: &'static str,
    pub record_key: String,
    pub source: String,
    pub machine_name: String,
    pub session_id: Option<String>,
    pub project_path: Option<String>,
}

// ---------------------------------------------------------------------------
// Extraction helpers (ported from parsers.ts)
// ---------------------------------------------------------------------------

/// Extract burn rate from `number | { costPerHour } | null`.
pub fn extract_burn_rate(data: &Value) -> Option<f64> {
    if data.is_null() {
        return None;
    }
    if let Some(n) = data.as_f64() {
        return Some(n);
    }
    if let Some(obj) = data.as_object() {
        if let Some(n) = obj.get("costPerHour").and_then(|v| v.as_f64()) {
            return Some(n);
        }
    }
    None
}

/// Extract projection from `number | { totalCost } | null`.
pub fn extract_projection(data: &Value) -> Option<f64> {
    if data.is_null() {
        return None;
    }
    if let Some(n) = data.as_f64() {
        return Some(n);
    }
    if let Some(obj) = data.as_object() {
        if let Some(n) = obj.get("totalCost").and_then(|v| v.as_f64()) {
            return Some(n);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Row constructors
// ---------------------------------------------------------------------------

/// Build a breakdown row: one row per model per record.
fn breakdown_row(
    now: &str,
    scope: &RowScope,
    bd: &BreakdownInput,
    reasoning_tokens: u64,
    import_id: &str,
) -> EventRow {
    let raw_key = format!(
        "{}|{}|{}|{}|{}|{}",
        scope.source, scope.machine_name, scope.record_type, scope.date, bd.model_name, scope.record_key
    );
    let dedup_key = make_dedup_key(&raw_key);

    EventRow {
        date: scope.date.clone(),
        record_type: scope.record_type.to_string(),
        record_key: scope.record_key.clone(),
        source: scope.source.clone(),
        machine_name: scope.machine_name.clone(),
        account_id: String::new(),
        api_key_id: String::new(),
        model_name: bd.model_name.clone(),
        session_id: scope.session_id.clone().unwrap_or_default(),
        project_path: scope.project_path.clone().unwrap_or_default(),
        input_tokens: bd.input_tokens,
        output_tokens: bd.output_tokens,
        cache_creation_tokens: bd.cache_creation_tokens,
        cache_read_tokens: bd.cache_read_tokens,
        reasoning_tokens,
        total_tokens: total_tokens(
            bd.input_tokens,
            bd.output_tokens,
            bd.cache_creation_tokens,
            bd.cache_read_tokens,
        )
        .saturating_add(bd.extra_total_tokens),
        cost: bd.cost,
        dedup_key,
        import_id: import_id.to_string(),
        block_id: String::new(),
        start_time: None,
        end_time: None,
        actual_end_time: None,
        is_active: 0,
        is_gap: 0,
        entries: 0,
        burn_rate: 0.0,
        projection: 0.0,
        usage_limit_reset_time: None,
        created_at: now.to_string(),
        updated_at: now.to_string(),
    }
}

/// Build a block row — uses the source's own `total_tokens` (NOT the formula).
fn block_row(now: &str, source: &str, machine_name: &str, item: &BlockUsage) -> EventRow {
    let start_parsed = crate::util::date::parse_date_time(&item.start_time);
    let date = start_parsed
        .as_ref()
        .and_then(|s| s.split(' ').next().map(|d| d.to_string()))
        .unwrap_or_else(|| chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string());

    EventRow {
        date,
        record_type: "block".to_string(),
        record_key: item.id.clone(),
        source: source.to_string(),
        machine_name: machine_name.to_string(),
        account_id: String::new(),
        api_key_id: String::new(),
        model_name: String::new(),
        session_id: String::new(),
        project_path: String::new(),
        input_tokens: item.token_counts.input_tokens,
        output_tokens: item.token_counts.output_tokens,
        cache_creation_tokens: item.token_counts.cache_creation_tokens,
        cache_read_tokens: item.token_counts.cache_read_tokens,
        reasoning_tokens: 0,
        total_tokens: item.total_tokens, // block rows: use source's own total
        cost: item.cost_usd,
        dedup_key: String::new(),
        import_id: String::new(),
        block_id: item.id.clone(),
        start_time: crate::util::date::parse_date_time(&item.start_time),
        end_time: crate::util::date::parse_date_time(&item.end_time),
        actual_end_time: item.actual_end_time.as_deref()
            .and_then(crate::util::date::parse_date_time),
        is_active: if item.is_active { 1 } else { 0 },
        is_gap: if item.is_gap { 1 } else { 0 },
        entries: item.entries,
        burn_rate: extract_burn_rate(&item.burn_rate).unwrap_or(0.0),
        projection: extract_projection(&item.projection).unwrap_or(0.0),
        usage_limit_reset_time: item.usage_limit_reset_time.as_deref()
            .and_then(crate::util::date::parse_date_time),
        created_at: now.to_string(),
        updated_at: now.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Fallback breakdowns (when no modelBreakdowns present)
// ---------------------------------------------------------------------------

/// Fallback model name from modelsUsed/models array, or "unknown".
fn first_model(models_used: &[String], models: &[String]) -> String {
    models_used.first()
        .or_else(|| models.first())
        .cloned()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Create fallback ModelBreakdown from a parent DailyUsage's totals.
pub fn fallback_breakdown(item: &DailyUsage) -> ModelBreakdown {
    ModelBreakdown {
        model_name: first_model(&item.models_used, &item.models),
        input_tokens: item.input_tokens,
        output_tokens: item.output_tokens,
        cache_creation_tokens: item.cache_creation_tokens,
        cache_read_tokens: item.cache_read_tokens,
        cost: item.total_cost,
    }
}

/// Create fallback CompanionModelBreakdown from a CompanionUsageRow.
fn fallback_companion_breakdown(row: &CompanionUsageRow) -> CompanionModelBreakdown {
    CompanionModelBreakdown {
        model_name: row.models_used.first()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string()),
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cache_creation_tokens: row.cache_creation_tokens,
        cache_read_tokens: row.cache_read_tokens,
        reasoning_tokens: row.reasoning_tokens,
        extra_total_tokens: 0,
        cost: row.total_cost,
    }
}

fn companion_breakdowns(row: &CompanionUsageRow) -> Vec<CompanionModelBreakdown> {
    let mut breakdowns = if row.model_breakdowns.is_empty() {
        vec![fallback_companion_breakdown(row)]
    } else {
        row.model_breakdowns.clone()
    };
    let known_total = breakdowns.iter().fold(0u64, |sum, breakdown| {
        sum.saturating_add(
            total_tokens(
                breakdown.input_tokens,
                breakdown.output_tokens,
                breakdown.cache_creation_tokens,
                breakdown.cache_read_tokens,
            )
            .saturating_add(breakdown.extra_total_tokens),
        )
    });
    let residual = row.total_tokens.saturating_sub(known_total);
    if residual > 0 {
        if let Some(last) = breakdowns.last_mut() {
            last.extra_total_tokens = last.extra_total_tokens.saturating_add(residual);
        }
    }
    breakdowns
}

/// Create fallback ModelBreakdown from a SessionUsage (used only in fallback path).
fn fallback_session_breakdown(item: &SessionUsage) -> ModelBreakdown {
    ModelBreakdown {
        model_name: first_model(&item.models_used, &[]),
        input_tokens: item.input_tokens,
        output_tokens: item.output_tokens,
        cache_creation_tokens: item.cache_creation_tokens,
        cache_read_tokens: item.cache_read_tokens,
        cost: item.total_cost,
    }
}

/// Create fallback ModelBreakdown from a ProjectDailyUsage.
fn fallback_project_breakdown(item: &ProjectDailyUsage) -> ModelBreakdown {
    ModelBreakdown {
        model_name: first_model(&item.models_used, &[]),
        input_tokens: item.input_tokens,
        output_tokens: item.output_tokens,
        cache_creation_tokens: item.cache_creation_tokens,
        cache_read_tokens: item.cache_read_tokens,
        cost: item.total_cost,
    }
}

// ---------------------------------------------------------------------------
// Cost distribution helper (generic over breakdown types that have tokens+cost)
// ---------------------------------------------------------------------------

/// Run distribute_cost on a vector of (output, input, cost) triples,
/// writing results back. Returns the distributed costs in order.
fn distribute_costs(
    output_tokens: &[u64],
    input_tokens: &[u64],
    parent_cost: f64,
) -> Vec<f64> {
    if output_tokens.is_empty() {
        return vec![];
    }
    let mut bds: Vec<BreakdownForCost> = (0..output_tokens.len())
        .map(|i| BreakdownForCost {
            output_tokens: output_tokens[i],
            input_tokens: input_tokens[i],
            cost: 0.0,
        })
        .collect();
    let already_present = output_tokens.iter().enumerate()
        .any(|(_i, _)| {
            // Check if costs are already non-zero
            // We use a different approach: caller passes parent_cost=0 if costs present
            false // placeholder — we always distribute here
        });

    // Check if per-model costs are already present
    let total_present: f64 = output_tokens.iter().enumerate()
        .map(|(_i, _)| 0.0) // cost not available here; caller checks
        .sum();
    let _ = already_present;
    let _ = total_present;

    // We need the original costs to check if they're present.
    // This function is only called when costs need distribution.
    distribute_cost(&mut bds, parent_cost);
    bds.iter().map(|b| b.cost).collect()
}

/// Distribute cost across cloned breakdowns, writing results back.
/// Works for any breakdown type that implements HasCost.
trait HasCost {
    fn output_tokens(&self) -> u64;
    fn input_tokens(&self) -> u64;
    fn cost(&self) -> f64;
    fn set_cost(&mut self, c: f64);
}

fn distribute_and_write<T: HasCost>(breakdowns: &mut [T], parent_cost: f64) {
    // Check if per-model costs already present
    let total: f64 = breakdowns.iter().map(|b| b.cost()).sum();
    if total > 0.0 || parent_cost <= 0.0 {
        return;
    }

    let mut for_cost: Vec<BreakdownForCost> = breakdowns.iter()
        .map(|b| BreakdownForCost {
            output_tokens: b.output_tokens(),
            input_tokens: b.input_tokens(),
            cost: b.cost(),
        })
        .collect();
    distribute_cost(&mut for_cost, parent_cost);

    for (i, bd) in breakdowns.iter_mut().enumerate() {
        if i < for_cost.len() {
            bd.set_cost(for_cost[i].cost);
        }
    }
}

impl HasCost for ModelBreakdown {
    fn output_tokens(&self) -> u64 { self.output_tokens }
    fn input_tokens(&self) -> u64 { self.input_tokens }
    fn cost(&self) -> f64 { self.cost }
    fn set_cost(&mut self, c: f64) { self.cost = c; }
}

impl HasCost for CompanionModelBreakdown {
    fn output_tokens(&self) -> u64 { self.output_tokens }
    fn input_tokens(&self) -> u64 { self.input_tokens }
    fn cost(&self) -> f64 { self.cost }
    fn set_cost(&mut self, c: f64) { self.cost = c; }
}

// ---------------------------------------------------------------------------
// Main builder: ccusage
// ---------------------------------------------------------------------------

/// Options for fetching ccusage data (defined here so the parser module
/// owns the shared data contract consumed by `build_ccusage_event_rows`).
#[derive(Debug, Clone, Default)]
pub struct CcusageFetchOptions {
    pub timeout: Option<u64>,
    pub max_retries: Option<u32>,
    pub verbose: Option<bool>,
    pub since: Option<String>,
    pub end_date: Option<String>,
}

/// ccusage data fetched by the fetcher.
#[derive(Debug, Clone, Default)]
pub struct CcusageData {
    pub daily: Vec<DailyUsage>,
    pub session: Vec<SessionUsage>,
    pub blocks: Vec<BlockUsage>,
    pub projects: HashMap<String, Vec<ProjectDailyUsage>>,
}

/// Build flat event rows from ccusage data.
///
/// Explodes model breakdowns into individual rows: one row per
/// (record × model). Blocks get model_name=''. Monthly is skipped
/// (derivable from daily via GROUP BY toYYYYMM(date)).
pub fn build_ccusage_event_rows(
    data: &CcusageData,
    machine_name: &str,
    hash_projects: bool,
    import_id: &str,
) -> Vec<EventRow> {
    let now = ch_now();
    let mut events: Vec<EventRow> = Vec::new();
    let source = "ccusage";

    // Daily
    for item in &data.daily {
        let date = parse_date(&item.date).unwrap_or_default();

        // Clone breakdowns (TS: item.modelBreakdowns.map(bd => ({ ...bd })))
        let mut breakdowns: Vec<ModelBreakdown> = if !item.model_breakdowns.is_empty() {
            item.model_breakdowns.clone()
        } else {
            vec![fallback_breakdown(item)]
        };

        // Distribute cost
        distribute_and_write(&mut breakdowns, item.total_cost);

        for bd in &breakdowns {
            let input = BreakdownInput::from(bd);
            events.push(breakdown_row(
                &now,
                &RowScope {
                    date: date.clone(),
                    record_type: "daily",
                    record_key: date.clone(),
                    source: source.to_string(),
                    machine_name: machine_name.to_string(),
                    session_id: None,
                    project_path: None,
                },
                &input,
                0, // ccusage has no reasoning tokens
                import_id,
            ));
        }
    }

    // Session
    for item in &data.session {
        let sid = hash_project_name_sync(&item.session_id, hash_projects);
        let pp = hash_project_name_sync(&item.project_path, hash_projects);
        let date = parse_date(&item.last_activity).unwrap_or_default();

        let mut breakdowns: Vec<ModelBreakdown> = if !item.model_breakdowns.is_empty() {
            item.model_breakdowns.clone()
        } else {
            vec![fallback_session_breakdown(item)]
        };
        distribute_and_write(&mut breakdowns, item.total_cost);

        for bd in &breakdowns {
            let input = BreakdownInput::from(bd);
            events.push(breakdown_row(
                &now,
                &RowScope {
                    date: date.clone(),
                    record_type: "session",
                    record_key: sid.clone(),
                    source: source.to_string(),
                    machine_name: machine_name.to_string(),
                    session_id: Some(sid.clone()),
                    project_path: Some(pp.clone()),
                },
                &input,
                0,
                import_id,
            ));
        }
    }

    // Blocks
    for item in &data.blocks {
        events.push(block_row(&now, source, machine_name, item));
    }

    // Projects (daily --instances)
    for (project_id, items) in &data.projects {
        let pp = hash_project_name_sync(project_id, hash_projects);
        for item in items {
            let date = parse_date(&item.date).unwrap_or_default();
            let record_key = format!("{}:{}", date, pp);

            let mut breakdowns: Vec<ModelBreakdown> = if !item.model_breakdowns.is_empty() {
                item.model_breakdowns.clone()
            } else {
                vec![fallback_project_breakdown(item)]
            };
            distribute_and_write(&mut breakdowns, item.total_cost);

            for bd in &breakdowns {
                let input = BreakdownInput::from(bd);
                events.push(breakdown_row(
                    &now,
                    &RowScope {
                        date: date.clone(),
                        record_type: "project_daily",
                        record_key: record_key.clone(),
                        source: source.to_string(),
                        machine_name: machine_name.to_string(),
                        session_id: None,
                        project_path: Some(pp.clone()),
                    },
                    &input,
                    0,
                    import_id,
                ));
            }
        }
    }

    events
}

// ---------------------------------------------------------------------------
// Main builder: companion
// ---------------------------------------------------------------------------

/// Build flat event rows from companion data (codex/opencode/gemini/…).
///
/// Key difference from ccusage: reasoning_tokens is carried from the
/// breakdown (not zeroed).
pub fn build_companion_event_rows(
    data: &CompanionData,
    machine_name: &str,
    source: &str,
    hash_projects: bool,
    import_id: &str,
) -> Vec<EventRow> {
    let now = ch_now();
    let mut events: Vec<EventRow> = Vec::new();

    // Daily
    for item in &data.daily {
        let date_str = item.date.as_deref()
            .or_else(|| item.last_activity.as_deref())
            .unwrap_or("");
        if date_str.is_empty() {
            continue;
        }
        let date = parse_date(date_str).unwrap_or_default();

        let mut breakdowns = companion_breakdowns(item);
        distribute_and_write(&mut breakdowns, item.total_cost);

        for bd in &breakdowns {
            let input = BreakdownInput::from(bd);
            events.push(breakdown_row(
                &now,
                &RowScope {
                    date: date.clone(),
                    record_type: "daily",
                    record_key: date.clone(),
                    source: source.to_string(),
                    machine_name: machine_name.to_string(),
                    session_id: None,
                    project_path: None,
                },
                &input,
                bd.reasoning_tokens, // companion carries reasoning
                import_id,
            ));
        }
    }

    // Session
    for item in &data.session {
        let sid_raw = item.session_id.clone().unwrap_or_else(|| "unknown".to_string());
        let sid = hash_project_name_sync(&sid_raw, hash_projects);
        let pp_raw = item.project_path.clone().unwrap_or_else(|| sid_raw.clone());
        let pp = hash_project_name_sync(&pp_raw, hash_projects);

        let date_str = item.last_activity.as_deref()
            .or_else(|| item.date.as_deref())
            .unwrap_or("");
        if date_str.is_empty() {
            continue;
        }
        let date = parse_date(date_str).unwrap_or_default();

        let mut breakdowns = companion_breakdowns(item);
        distribute_and_write(&mut breakdowns, item.total_cost);

        for bd in &breakdowns {
            let input = BreakdownInput::from(bd);
            events.push(breakdown_row(
                &now,
                &RowScope {
                    date: date.clone(),
                    record_type: "session",
                    record_key: sid.clone(),
                    source: source.to_string(),
                    machine_name: machine_name.to_string(),
                    session_id: Some(sid.clone()),
                    project_path: Some(pp.clone()),
                },
                &input,
                bd.reasoning_tokens,
                import_id,
            ));
        }
    }

    events
}

// ---------------------------------------------------------------------------
// Tests (golden rows — port of tests/unit/parsers.test.ts)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::types::DailyUsage;
    use serde_json::json;
    use std::collections::HashMap as StdHashMap;

    const MACHINE: &str = "test-machine";

    /// End-to-end: camelCase CLI JSON → parse → rows must keep tokens with cost.
    #[test]
    fn daily_rows_from_camel_case_json_keep_tokens_with_cost() {
        let raw = json!({
            "date": "2026-08-07",
            "inputTokens": 250777,
            "outputTokens": 64149,
            "cacheCreationTokens": 520714,
            "cacheReadTokens": 11176815,
            "totalTokens": 12012455,
            "totalCost": 301.2222,
            "modelsUsed": ["claude-opus-4-8"],
            "modelBreakdowns": [{
                "modelName": "claude-opus-4-8",
                "inputTokens": 250777,
                "outputTokens": 64149,
                "cacheCreationTokens": 520714,
                "cacheReadTokens": 11176815,
                "cost": 301.2222
            }]
        });
        let day: DailyUsage = serde_json::from_value(raw).expect("parse daily");
        let data = CcusageData {
            daily: vec![day],
            session: vec![],
            blocks: vec![],
            projects: StdHashMap::new(),
        };
        let rows = build_ccusage_event_rows(&data, MACHINE, false, "test-import");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_tokens, 250777 + 64149 + 520714 + 11176815);
        assert!(rows[0].total_tokens > 0);
        assert!((rows[0].cost - 301.2222).abs() < 1e-6);
        assert_eq!(rows[0].model_name, "claude-opus-4-8");
    }

    #[test]
    fn daily_one_row_per_model_breakdown_with_formula_total() {
        let data = CcusageData {
            daily: vec![DailyUsage {
                date: "2025-01-05".to_string(),
                total_cost: 0.05,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "claude-3-5-sonnet".to_string(),
                    input_tokens: 1000,
                    output_tokens: 2000,
                    cache_creation_tokens: 100,
                    cache_read_tokens: 200,
                    cost: 0.05,
                }],
                ..Default::default()
            }],
            session: vec![],
            blocks: vec![],
            projects: StdHashMap::new(),
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row.date, "2025-01-05");
        assert_eq!(row.record_type, "daily");
        assert_eq!(row.record_key, "2025-01-05");
        assert_eq!(row.source, "ccusage");
        assert_eq!(row.machine_name, MACHINE);
        assert_eq!(row.model_name, "claude-3-5-sonnet");
        assert_eq!(row.session_id, "");
        assert_eq!(row.project_path, "");
        assert_eq!(row.input_tokens, 1000);
        assert_eq!(row.output_tokens, 2000);
        assert_eq!(row.cache_creation_tokens, 100);
        assert_eq!(row.cache_read_tokens, 200);
        assert_eq!(row.reasoning_tokens, 0);
        assert_eq!(row.total_tokens, 3300); // formula: 1000+2000+100+200
        assert_eq!(row.cost, 0.05);
        assert_eq!(row.dedup_key, "2989cf7bd2e15426");
        assert_eq!(row.import_id, "");
        assert_eq!(row.block_id, "");
        assert_eq!(row.start_time, None);
        assert_eq!(row.end_time, None);
        assert_eq!(row.actual_end_time, None);
        assert_eq!(row.is_active, 0);
        assert_eq!(row.is_gap, 0);
        assert_eq!(row.entries, 0);
        assert_eq!(row.burn_rate, 0.0);
        assert_eq!(row.projection, 0.0);
        assert_eq!(row.usage_limit_reset_time, None);
    }

    #[test]
    fn session_record_key_is_session_id() {
        let data = CcusageData {
            daily: vec![],
            session: vec![SessionUsage {
                session_id: "sess-1".to_string(),
                project_path: "/repo".to_string(),
                last_activity: "2025-01-06".to_string(),
                total_cost: 0.025,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "claude-3-5-sonnet".to_string(),
                    input_tokens: 500,
                    output_tokens: 1000,
                    cache_creation_tokens: 50,
                    cache_read_tokens: 100,
                    cost: 0.025,
                }],
                ..Default::default()
            }],
            blocks: vec![],
            projects: StdHashMap::new(),
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row.date, "2025-01-06");
        assert_eq!(row.record_type, "session");
        assert_eq!(row.record_key, "sess-1");
        assert_eq!(row.session_id, "sess-1");
        assert_eq!(row.project_path, "/repo");
        assert_eq!(row.total_tokens, 1650); // 500+1000+50+100
        assert_eq!(row.dedup_key, "89a672922d64efe9");
    }

    #[test]
    fn block_record_key_is_block_id_and_uses_source_total() {
        let data = CcusageData {
            daily: vec![],
            session: vec![],
            blocks: vec![BlockUsage {
                id: "block-123".to_string(),
                start_time: "2025-01-05T10:00:00.000Z".to_string(),
                end_time: "2025-01-05T15:00:00.000Z".to_string(),
                actual_end_time: None,
                is_active: true,
                is_gap: false,
                entries: 5,
                token_counts: Default::default(),
                total_tokens: 99999, // deliberately != formula
                cost_usd: 0.25,
                usage_limit_reset_time: Some("2025-01-05T16:00:00.000Z".to_string()),
                burn_rate: serde_json::json!({"costPerHour": 0.1}),
                projection: serde_json::json!({"totalCost": 0.5}),
                ..Default::default()
            }],
            projects: StdHashMap::new(),
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row.record_type, "block");
        assert_eq!(row.record_key, "block-123");
        assert_eq!(row.block_id, "block-123");
        assert_eq!(row.model_name, "");
        assert_eq!(row.reasoning_tokens, 0);
        assert_eq!(row.total_tokens, 99999); // source total, not formula
        assert_eq!(row.cost, 0.25);
        assert_eq!(row.is_active, 1);
        assert_eq!(row.is_gap, 0);
        assert_eq!(row.entries, 5);
        assert_eq!(row.burn_rate, 0.1);
        assert_eq!(row.projection, 0.5);
        assert!(row.start_time.is_some());
        assert!(row.end_time.is_some());
        assert_eq!(row.actual_end_time, None);
    }

    #[test]
    fn project_daily_record_key_format() {
        let mut projects: StdHashMap<String, Vec<ProjectDailyUsage>> = StdHashMap::new();
        projects.insert("/repo".to_string(), vec![ProjectDailyUsage {
            date: "2025-01-05".to_string(),
            total_cost: 0.025,
            model_breakdowns: vec![ModelBreakdown {
                model_name: "claude-3-5-sonnet".to_string(),
                input_tokens: 500,
                output_tokens: 1000,
                cache_creation_tokens: 50,
                cache_read_tokens: 100,
                cost: 0.025,
            }],
            ..Default::default()
        }]);

        let data = CcusageData {
            daily: vec![],
            session: vec![],
            blocks: vec![],
            projects,
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.record_type, "project_daily");
        assert_eq!(row.record_key, "2025-01-05:/repo");
        assert_eq!(row.project_path, "/repo");
        assert_eq!(row.total_tokens, 1650);
        assert_eq!(row.dedup_key, "07450000e75f0bb1");
    }

    #[test]
    fn daily_fallback_no_model_breakdowns() {
        let data = CcusageData {
            daily: vec![DailyUsage {
                date: "2025-01-05".to_string(),
                input_tokens: 10,
                output_tokens: 20,
                cache_creation_tokens: 1,
                cache_read_tokens: 2,
                total_cost: 0.5,
                models_used: vec!["model-x".to_string()],
                ..Default::default()
            }],
            session: vec![],
            blocks: vec![],
            projects: StdHashMap::new(),
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_name, "model-x");
        assert_eq!(rows[0].cost, 0.5);
        assert_eq!(rows[0].total_tokens, 33); // 10+20+1+2
    }

    #[test]
    fn distribute_cost_weights_by_output_last_absorbs_rounding() {
        let data = CcusageData {
            daily: vec![DailyUsage {
                date: "2025-02-01".to_string(),
                total_cost: 1.0,
                model_breakdowns: vec![
                    ModelBreakdown { model_name: "a".to_string(), output_tokens: 1, ..Default::default() },
                    ModelBreakdown { model_name: "b".to_string(), output_tokens: 1, ..Default::default() },
                    ModelBreakdown { model_name: "c".to_string(), output_tokens: 1, ..Default::default() },
                ],
                ..Default::default()
            }],
            session: vec![],
            blocks: vec![],
            projects: StdHashMap::new(),
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        assert_eq!(rows.len(), 3);
        let sum: f64 = rows.iter().map(|r| r.cost).sum();
        assert!((sum - 1.0).abs() < 1e-8);
    }

    #[test]
    fn distribute_cost_zero_tokens_all_to_first() {
        let data = CcusageData {
            daily: vec![DailyUsage {
                date: "2025-02-02".to_string(),
                total_cost: 2.0,
                model_breakdowns: vec![
                    ModelBreakdown { model_name: "a".to_string(), ..Default::default() },
                    ModelBreakdown { model_name: "b".to_string(), ..Default::default() },
                ],
                ..Default::default()
            }],
            session: vec![],
            blocks: vec![],
            projects: StdHashMap::new(),
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cost, 2.0);
        assert_eq!(rows[1].cost, 0.0);
    }

    #[test]
    fn distribute_cost_per_model_costs_unchanged() {
        let data = CcusageData {
            daily: vec![DailyUsage {
                date: "2025-02-03".to_string(),
                total_cost: 3.0,
                model_breakdowns: vec![
                    ModelBreakdown { model_name: "a".to_string(), input_tokens: 1, output_tokens: 1, cost: 1.0, ..Default::default() },
                    ModelBreakdown { model_name: "b".to_string(), input_tokens: 1, output_tokens: 1, cost: 2.0, ..Default::default() },
                ],
                ..Default::default()
            }],
            session: vec![],
            blocks: vec![],
            projects: StdHashMap::new(),
        };

        let rows = build_ccusage_event_rows(&data, MACHINE, false, "");
        let costs: Vec<f64> = rows.iter().map(|r| r.cost).collect();
        assert_eq!(costs, vec![1.0, 2.0]);
    }

    #[test]
    fn companion_total_includes_cache_excludes_reasoning() {
        let data = CompanionData {
            daily: vec![CompanionUsageRow {
                date: Some("2026-05-13".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 10,
                cache_read_tokens: 20,
                total_cost: 0.01,
                models_used: vec!["gpt-5".to_string()],
                model_breakdowns: vec![CompanionModelBreakdown {
                    model_name: "gpt-5".to_string(),
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_tokens: 10,
                    cache_read_tokens: 20,
                    reasoning_tokens: 0,
                    extra_total_tokens: 0,
                    cost: 0.01,
                }],
                ..Default::default()
            }],
            monthly: vec![],
            session: vec![],
        };

        let rows = build_companion_event_rows(&data, "machine-1", "codex", false, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_tokens, 180); // 100+50+10+20
        assert_eq!(rows[0].cache_creation_tokens, 10);
        assert_eq!(rows[0].cache_read_tokens, 20);
        assert_eq!(rows[0].reasoning_tokens, 0);
    }

    #[test]
    fn companion_reasoning_not_in_total_2717719() {
        let data = CompanionData {
            daily: vec![CompanionUsageRow {
                date: Some("2026-05-13".to_string()),
                input_tokens: 469867,
                output_tokens: 33580,
                cache_creation_tokens: 0,
                cache_read_tokens: 2214272,
                reasoning_tokens: 17062,
                total_cost: 0.66,
                models_used: vec!["gpt-5.4-mini".to_string()],
                model_breakdowns: vec![CompanionModelBreakdown {
                    model_name: "gpt-5.4-mini".to_string(),
                    input_tokens: 469867,
                    output_tokens: 33580,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 2214272,
                    reasoning_tokens: 17062,
                    extra_total_tokens: 0,
                    cost: 0.66,
                }],
                ..Default::default()
            }],
            monthly: vec![],
            session: vec![],
        };

        let rows = build_companion_event_rows(&data, "machine-1", "codex", false, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_tokens, 2717719); // 469867+33580+0+2214272
        assert_eq!(rows[0].reasoning_tokens, 17062);
    }

    #[test]
    fn companion_preserves_parent_total_when_model_breakdown_omits_extra_tokens() {
        let data = CompanionData {
            daily: vec![CompanionUsageRow {
                date: Some("2026-09-24".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 10,
                cache_read_tokens: 20,
                total_tokens: 200,
                total_cost: 0.01,
                models_used: vec!["opencode-model".to_string()],
                model_breakdowns: vec![CompanionModelBreakdown {
                    model_name: "opencode-model".to_string(),
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_tokens: 10,
                    cache_read_tokens: 20,
                    reasoning_tokens: 0,
                    extra_total_tokens: 0,
                    cost: 0.01,
                }],
                ..Default::default()
            }],
            monthly: vec![],
            session: vec![],
        };

        let rows = build_companion_event_rows(&data, "machine-1", "opencode", false, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_tokens, 200);
        assert_eq!(rows[0].output_tokens, 50);
    }

    #[test]
    fn companion_explodes_per_model_breakdowns() {
        let data = CompanionData {
            daily: vec![CompanionUsageRow {
                date: Some("2026-05-13".to_string()),
                input_tokens: 300,
                output_tokens: 120,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_cost: 0.05,
                models_used: vec!["model-a".to_string(), "model-b".to_string()],
                model_breakdowns: vec![
                    CompanionModelBreakdown { model_name: "model-a".to_string(), input_tokens: 200, output_tokens: 80, cost: 0.03, ..Default::default() },
                    CompanionModelBreakdown { model_name: "model-b".to_string(), input_tokens: 100, output_tokens: 40, cost: 0.02, ..Default::default() },
                ],
                ..Default::default()
            }],
            monthly: vec![],
            session: vec![],
        };

        let rows = build_companion_event_rows(&data, "machine-1", "gemini", false, "");
        assert_eq!(rows.len(), 2);
        let models: Vec<&str> = rows.iter().map(|r| r.model_name.as_str()).collect();
        assert!(models.contains(&"model-a"));
        assert!(models.contains(&"model-b"));
        assert!(rows.iter().all(|r| r.source == "gemini"));
    }
}
