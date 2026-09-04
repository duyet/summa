use chrono::{Duration, Utc};
use worker::{Env, Result};

use crate::sinks::{
    clickhouse_configured, clickhouse_query, motherduck_configured, motherduck_query,
    parse_analytics_payload,
};
use crate::types::{AnalyticsPoint, MAX_ANALYTICS_DAYS};

pub fn analytics_window(
    since: Option<&str>,
    until: Option<&str>,
    days: Option<i64>,
) -> std::result::Result<(String, String), String> {
    let today = Utc::now().date_naive();
    let until_d = match until.filter(|s| !s.is_empty()) {
        Some(s) => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| format!("invalid date `{s}`"))?,
        None => today,
    };
    let since_d = match since.filter(|s| !s.is_empty()) {
        Some(s) => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| format!("invalid date `{s}`"))?,
        None => {
            let n = days.unwrap_or(30).clamp(1, MAX_ANALYTICS_DAYS);
            until_d - Duration::days(n - 1)
        }
    };
    if since_d > until_d {
        return Err("since is after until".into());
    }
    if (until_d - since_d).num_days() + 1 > MAX_ANALYTICS_DAYS {
        return Err("range exceeds 366 days".into());
    }
    Ok((
        since_d.format("%Y-%m-%d").to_string(),
        until_d.format("%Y-%m-%d").to_string(),
    ))
}

pub fn inclusive_days(since: &str, until: &str) -> i64 {
    match (
        chrono::NaiveDate::parse_from_str(since, "%Y-%m-%d"),
        chrono::NaiveDate::parse_from_str(until, "%Y-%m-%d"),
    ) {
        (Ok(a), Ok(b)) => (b - a).num_days().max(0) + 1,
        _ => 1,
    }
}

pub fn summarize(since: &str, until: &str, points: &[AnalyticsPoint]) -> serde_json::Value {
    let days = inclusive_days(since, until);
    let mut cost = 0.0;
    let mut total_tokens: u64 = 0;
    let mut entries: u64 = 0;
    let mut by: Vec<(String, f64, u64, u64)> = Vec::new();
    for p in points {
        cost += p.cost;
        total_tokens = total_tokens.saturating_add(p.total_tokens);
        entries = entries.saturating_add(p.entries);
        if let Some(row) = by.iter_mut().find(|s| s.0 == p.source) {
            row.1 += p.cost;
            row.2 = row.2.saturating_add(p.total_tokens);
            row.3 = row.3.saturating_add(p.entries);
        } else {
            by.push((p.source.clone(), p.cost, p.total_tokens, p.entries));
        }
    }
    by.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    serde_json::json!({
        "since": since,
        "until": until,
        "days": days,
        "cost": cost,
        "total_tokens": total_tokens,
        "entries": entries,
        "cost_per_day": if days > 0 { cost / days as f64 } else { 0.0 },
        "by_source": by.into_iter().map(|s| serde_json::json!({
            "source": s.0, "cost": s.1, "total_tokens": s.2, "entries": s.3
        })).collect::<Vec<_>>(),
    })
}

/// Usage totals come from warehouse `ccusage_events`, never D1.
pub fn usage_select_sql(
    group: &str,
    account_id: &str,
    include_legacy: bool,
    since: &str,
    until: &str,
    use_final: bool,
) -> String {
    let extra = if group == "model" {
        "source, model_name"
    } else {
        "source, '' AS model_name"
    };
    let group_by = if group == "model" {
        "date, source, model_name"
    } else {
        "date, source"
    };
    let tenant = if include_legacy {
        format!(
            "(account_id = {} OR account_id = '')",
            crate::types::sql_literal(account_id)
        )
    } else {
        format!("account_id = {}", crate::types::sql_literal(account_id))
    };
    let where_sql =
        format!("record_type = 'daily' AND date >= '{since}' AND date <= '{until}' AND {tenant}");
    let select = format!(
        "SELECT date, {extra}, sum(cost) AS cost, sum(total_tokens) AS total_tokens, sum(entries) AS entries \
         FROM ccusage_events"
    );
    let tail = format!(" WHERE {where_sql} GROUP BY {group_by} ORDER BY date, source");
    if use_final {
        format!("{select} FINAL{tail} FORMAT JSONEachRow")
    } else {
        format!("{select}{tail}")
    }
}

pub async fn load_points(
    env: &Env,
    account_id: &str,
    include_legacy: bool,
    group: &str,
    since: &str,
    until: &str,
) -> Result<Vec<AnalyticsPoint>> {
    let mut errors = Vec::new();
    if clickhouse_configured(env) {
        let sql = usage_select_sql(group, account_id, include_legacy, since, until, true);
        match clickhouse_query(env, &sql).await {
            Ok(text) => return Ok(parse_analytics_payload(&text)),
            Err(e) => errors.push(format!("clickhouse: {e}")),
        }
    }
    if motherduck_configured(env) {
        let sql = usage_select_sql(group, account_id, include_legacy, since, until, false);
        match motherduck_query(env, &sql).await {
            Ok(text) => return Ok(parse_analytics_payload(&text)),
            Err(e) => errors.push(format!("motherduck: {e}")),
        }
    }
    if errors.is_empty() {
        return Err(worker::Error::RustError(
            "no analytics sink configured".into(),
        ));
    }
    Err(worker::Error::RustError(errors.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AnalyticsPoint;

    #[test]
    fn window_inclusive_range() {
        let (since, until) =
            analytics_window(Some("2026-01-01"), Some("2026-01-10"), None).unwrap();
        assert_eq!(since, "2026-01-01");
        assert_eq!(until, "2026-01-10");
        assert_eq!(inclusive_days(&since, &until), 10);
    }

    #[test]
    fn window_rejects_inverted_range() {
        assert!(analytics_window(Some("2026-02-01"), Some("2026-01-01"), None).is_err());
    }

    #[test]
    fn window_rejects_range_over_a_year() {
        assert!(analytics_window(Some("2024-01-01"), Some("2026-01-02"), None).is_err());
        let (since, until) = analytics_window(None, Some("2026-01-31"), Some(9_999)).unwrap();
        assert_eq!(until, "2026-01-31");
        assert_eq!(since, "2025-01-31");
    }

    #[test]
    fn summarize_cost_per_day_uses_inclusive_days() {
        let points = vec![AnalyticsPoint {
            date: "2026-01-01".into(),
            source: "cursor".into(),
            model_name: "x".into(),
            cost: 10.0,
            total_tokens: 100,
            entries: 2,
        }];
        let v = summarize("2026-01-01", "2026-01-10", &points);
        assert_eq!(v["days"], 10);
        assert_eq!(v["cost"], 10.0);
        assert_eq!(v["cost_per_day"], 1.0);
        assert_eq!(v["entries"], 2);
    }

    #[test]
    fn usage_sql_reads_ccusage_events_not_d1() {
        let ch = usage_select_sql("source", "acc-1", true, "2026-01-01", "2026-01-07", true);
        assert!(ch.contains("FROM ccusage_events FINAL"));
        assert!(ch.contains("FORMAT JSONEachRow"));
        assert!(ch.contains("(account_id = 'acc-1' OR account_id = '')"));
        let lower = ch.to_ascii_lowercase();
        assert!(!lower.contains("from events"));
        assert!(!lower.contains("api_keys"));
        assert!(!lower.contains("from accounts"));

        let md = usage_select_sql("model", "acc-2", false, "2026-02-01", "2026-02-02", false);
        assert!(md.contains("FROM ccusage_events WHERE"));
        assert!(!md.contains(" FINAL"));
        assert!(!md.contains("FORMAT JSONEachRow"));
        assert!(md.contains("account_id = 'acc-2'"));
        assert!(md.contains("GROUP BY date, source, model_name"));
        assert!(!md.contains("api_keys"));
    }

    #[test]
    fn summary_from_store_fixture_does_not_invent_cost() {
        let text = "{\"date\":\"2026-01-01\",\"source\":\"cursor\",\"cost\":2.5,\"total_tokens\":9,\"entries\":1}\n{\"date\":\"2026-01-01\",\"source\":\"grok\",\"cost\":1.25,\"total_tokens\":3,\"entries\":2}\n";
        let points = parse_analytics_payload(text);
        let v = summarize("2026-01-01", "2026-01-01", &points);
        assert_eq!(v["cost"], 3.75);
        assert_eq!(v["total_tokens"], 12);
        assert_eq!(v["entries"], 3);
        assert_eq!(v["cost_per_day"], 3.75);
        assert_eq!(v["days"], 1);
    }
}
