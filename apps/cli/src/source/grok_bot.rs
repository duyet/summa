/**
 * Grok Bot (x.ai) chat token source.
 *
 * Grok Bot chat usage is served by the **same** usage-events API the Cursor
 * source reads: `usageEvents` with a `tokenUsage` block
 * (`inputTokens` / `outputTokens` / `cacheWriteTokens` / `cacheReadTokens`,
 * strings tolerated) and `chargedCents` / `tokenUsage.totalCents` for cost. There
 * is no separate Grok Bot ledger on disk or a standalone Grok Bot token API that
 * this repo can reach without the Cursor account auth, so this source reuses
 * Cursor's auth + paging and maps **only** the events Cursor classifies as the
 * Grok Bot surface.
 *
 * Until this source existed those rows were written by `source::cursor` as
 * `source = "cursor-grok-bot"`, which made Grok Bot chat tokens invisible to
 * `--skip-grok` and to any per-source query that looked for `grok`. They are now
 * their own source id (`grok-bot`), on their own `--skip-grok-bot` flag that is
 * **independent** of `--skip-grok` (Grok Build local logs + `grok-api` billing)
 * and of `--skip-cursor`.
 *
 * Exactly one owner per event: `cursor.rs` skips everything this source claims.
 * Two labels for one event would mean two dedup keys, so ReplacingMergeTree
 * would keep both and count the same chat twice.
 *
 * `machine_name` is `account` (never the importer hostname) because the feed is
 * account-wide, matching every other Cursor-derived row.
 *
 * Cost is the reported cents only. No rate is invented: an event with no cents
 * field imports its tokens at cost 0 rather than a fabricated price.
 *
 * Missing auth, a dead API or an empty feed is an empty result, not an import
 * error, so the rest of the import continues.
 */

use std::path::PathBuf;

use async_trait::async_trait;

use crate::model::{DataSource, EventsSnapshotData, SourceResult};
use crate::source::cursor::{
    fetch_cursor_usage_events, map_grok_bot_events, CursorSourceOptions, SOURCE_GROK_BOT,
};
use crate::util::date::ch_now;

#[derive(Debug, Clone, Default)]
pub struct GrokBotSourceOptions {
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Cookie header or raw WorkosCursorSessionToken value (Grok Bot uses the
    /// Cursor account session).
    pub session: Option<String>,
    /// Cursor team Admin API key.
    pub api_key: Option<String>,
    /// Override Cursor.app `state.vscdb` path (tests).
    pub state_db_path: Option<PathBuf>,
    /// Skip the Cursor.app local JWT lookup (tests / missing-auth cases).
    pub disable_local_auth: bool,
}

impl From<&GrokBotSourceOptions> for CursorSourceOptions {
    fn from(o: &GrokBotSourceOptions) -> Self {
        CursorSourceOptions {
            verbose: o.verbose,
            days_back: o.days_back,
            since: o.since.clone(),
            end_date: o.end_date.clone(),
            import_id: o.import_id.clone(),
            session: o.session.clone(),
            api_key: o.api_key.clone(),
            state_db_path: o.state_db_path.clone(),
            disable_local_auth: o.disable_local_auth,
        }
    }
}

pub struct GrokBotSource {
    opts: GrokBotSourceOptions,
}

impl GrokBotSource {
    pub fn new(opts: GrokBotSourceOptions) -> Self {
        Self { opts }
    }
}

#[async_trait]
impl DataSource for GrokBotSource {
    fn name(&self) -> &'static str {
        SOURCE_GROK_BOT
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let now = ch_now();
        let cursor_opts: CursorSourceOptions = (&self.opts).into();

        match fetch_cursor_usage_events(&cursor_opts).await {
            Ok(events) => {
                let rows = map_grok_bot_events(
                    &events,
                    &self.opts.import_id,
                    self.opts.since.as_deref(),
                    self.opts.end_date.as_deref(),
                    &now,
                );
                if self.opts.verbose {
                    eprintln!(
                        "Grok Bot Source: {} rows from {} usage event(s).",
                        rows.len(),
                        events.len()
                    );
                }
                Ok(SourceResult {
                    source_name: self.name().to_string(),
                    data: EventsSnapshotData { events: rows },
                    fetched_at: now,
                    error: None,
                })
            }
            Err(e) => {
                if self.opts.verbose {
                    eprintln!("Grok Bot Source skipped/failed: {e}");
                }
                Ok(SourceResult {
                    source_name: self.name().to_string(),
                    data: EventsSnapshotData { events: Vec::new() },
                    fetched_at: now,
                    error: Some(e.to_string()),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::cursor::{map_cursor_events, CURSOR_ACCOUNT_MACHINE};
    use futures::executor::block_on;

    /// One Grok Bot chat event plus one plain Cursor event, in Cursor's real
    /// dashboard shape (both `timestamp` and numbers serialized as strings).
    const MIXED: &str = r#"{
      "totalUsageEventsCount": 2,
      "usageEventsDisplay": [
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
          "timestamp": "1750979225854",
          "userEmail": "dev@example.com",
          "conversationId": "conv-editor",
          "model": "claude-4.5-sonnet",
          "kind": "Usage-based",
          "isHeadless": false,
          "tokenUsage": {
            "inputTokens": 126,
            "outputTokens": 450,
            "cacheWriteTokens": 6112,
            "cacheReadTokens": 11964
          },
          "chargedCents": 21.36232
        }
      ]
    }"#;

    const NOW: &str = "2026-08-19 00:00:00";

    fn opts() -> GrokBotSourceOptions {
        GrokBotSourceOptions {
            import_id: "import-1".into(),
            disable_local_auth: true,
            ..Default::default()
        }
    }

    fn grok_bot_rows(json: &str) -> Vec<crate::model::EventRow> {
        let events = crate::source::cursor::events_from_page_json(json).unwrap();
        map_grok_bot_events(&events, "import-1", None, None, NOW)
    }

    fn sessions<'a>(rows: &'a [crate::model::EventRow]) -> Vec<&'a crate::model::EventRow> {
        rows.iter().filter(|e| e.record_type == "session").collect()
    }

    #[test]
    fn name_is_grok_bot() {
        assert_eq!(GrokBotSource::new(opts()).name(), SOURCE_GROK_BOT);
        assert_eq!(SOURCE_GROK_BOT, "grok-bot");
    }

    #[test]
    fn only_grok_bot_events_become_rows() {
        let rows = grok_bot_rows(MIXED);
        let sess = sessions(&rows);
        assert_eq!(sess.len(), 1, "cursor events must not leak in: {sess:?}");
        let g = sess[0];
        assert_eq!(g.source, SOURCE_GROK_BOT);
        assert_eq!(g.session_id, "conv-grok");
        assert_eq!(g.model_name, "grok-4.5");
        assert_eq!(g.input_tokens, 40);
        assert_eq!(g.output_tokens, 10);
        assert_eq!(g.cache_creation_tokens, 0);
        assert_eq!(g.cache_read_tokens, 8);
        assert_eq!(g.total_tokens, 40 + 10 + 8);
        assert!((g.cost - 0.01).abs() < 1e-9, "chargedCents 1.0 → USD, got {}", g.cost);
        // Account-wide identity, like every other Cursor-derived row.
        assert_eq!(g.machine_name, CURSOR_ACCOUNT_MACHINE);
        assert_eq!(g.machine_name, "account");
    }

    /// The invariant that keeps this source honest: a Grok Bot chat must not be
    /// counted twice under two source labels.
    #[test]
    fn cursor_and_grok_bot_never_both_claim_an_event() {
        let events = crate::source::cursor::events_from_page_json(MIXED).unwrap();
        let cursor_rows = map_cursor_events(&events, "imp", None, None, NOW);
        let bot_rows = map_grok_bot_events(&events, "imp", None, None, NOW);

        let cursor_sessions = sessions(&cursor_rows);
        assert_eq!(cursor_sessions.len(), 1);
        assert_eq!(cursor_sessions[0].session_id, "conv-editor");
        assert!(
            !cursor_rows.iter().any(|r| r.source == SOURCE_GROK_BOT),
            "grok-bot rows must come only from source::grok_bot"
        );

        let keys: std::collections::HashSet<&str> = cursor_rows
            .iter()
            .chain(bot_rows.iter())
            .map(|r| r.dedup_key.as_str())
            .collect();
        let total = cursor_rows.len() + bot_rows.len();
        assert_eq!(keys.len(), total, "duplicate dedup keys mean double counting");
    }

    #[test]
    fn grok_bot_source_is_separate_from_cursor_source_rows() {
        let events = crate::source::cursor::events_from_page_json(MIXED).unwrap();
        let bot_rows = map_grok_bot_events(&events, "imp", None, None, NOW);
        assert!(bot_rows.iter().all(|r| r.source == SOURCE_GROK_BOT));
        assert!(bot_rows.iter().all(|r| r.dedup_key.len() == 16));
        // Session + daily per (date, model).
        assert_eq!(bot_rows.len(), 2, "{bot_rows:?}");
    }

    /// No rate is invented when the feed reports no cents: tokens still import.
    #[test]
    fn missing_cost_keeps_tokens_at_zero_cost() {
        let json = r#"{"usageEventsDisplay":[
          {"timestamp":"1750979500000","model":"grok-4.5","kind":"Grok Bot",
           "tokenUsage":{"inputTokens":"40","outputTokens":"10","cacheReadTokens":"8"}}
        ]}"#;
        let rows = grok_bot_rows(json);
        let sess = sessions(&rows);
        assert_eq!(sess.len(), 1);
        assert_eq!(sess[0].input_tokens, 40);
        assert_eq!(sess[0].total_tokens, 58);
        assert_eq!(sess[0].cost, 0.0);
    }

    #[test]
    fn since_filters_out_grok_bot_days() {
        let events = crate::source::cursor::events_from_page_json(MIXED).unwrap();
        let rows = map_grok_bot_events(&events, "imp", Some("2026-08-20"), None, NOW);
        assert!(rows.is_empty(), "future window must exclude the rows");
    }

    #[test]
    fn non_grok_model_without_signal_is_not_a_grok_bot_event() {
        let json = r#"{"usageEventsDisplay":[
          {"timestamp":"1750979500000","model":"claude-4.5-sonnet","kind":"Usage-based",
           "tokenUsage":{"inputTokens":5,"outputTokens":5}}
        ]}"#;
        assert!(grok_bot_rows(json).is_empty());
    }

    #[test]
    fn missing_auth_returns_empty_not_error() {
        let src = GrokBotSource::new(opts());
        let r = block_on(src.fetch()).unwrap();
        assert_eq!(r.source_name, SOURCE_GROK_BOT);
        assert!(r.data.events.is_empty());
        assert!(r.error.is_none(), "no credentials is a skip, not a failure");
    }
}