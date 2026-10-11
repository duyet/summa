/**
 * Hermes Source
 *
 * Fetches usage data from Hermes agent SQLite database (~/.hermes/state.db).
 * Extracts exact token counts, cache read/write, reasoning tokens, and costs.
 */

use rusqlite::Connection;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use crate::model::{DataSource, EventRow, EventsSnapshotData, SourceResult};
use crate::util::date::ch_now;
use crate::util::hash::{hash_project_name_sync, make_dedup_key};
use crate::util::pricing::resolve_reported_cost;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HermesSourceOptions {
    pub machine_name: String,
    pub hash_projects: bool,
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Override Hermes home (tests). When None, uses `HERMES_HOME` or `~/.hermes`.
    pub base_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

pub struct HermesSource {
    opts: HermesSourceOptions,
}

impl HermesSource {
    pub fn new(opts: HermesSourceOptions) -> Self {
        Self { opts }
    }

    pub fn name(&self) -> &'static str {
        "hermes"
    }
}

#[async_trait]
impl DataSource for HermesSource {
    fn name(&self) -> &'static str {
        "hermes"
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let HermesSourceOptions {
            machine_name,
            hash_projects,
            verbose,
            days_back,
            since,
            end_date,
            import_id,
            base_dir,
        } = &self.opts;

        let effective_since = if let Some(s) = since {
            Some(s.clone())
        } else if let Some(days) = days_back {
            if *days > 0 {
                let d = chrono::Utc::now() - chrono::Duration::days(*days);
                Some(d.format("%Y-%m-%d").to_string())
            } else {
                None
            }
        } else {
            None
        };

        let base_dir = if let Some(d) = base_dir {
            d.clone()
        } else if let Ok(h) = env::var("HERMES_HOME") {
            PathBuf::from(h)
        } else {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".hermes")
        };
        let db_path = base_dir.join("state.db");

        let mut events: Vec<EventRow> = Vec::new();
        let now = ch_now();

        if !db_path.exists() {
            if *verbose {
                eprintln!("Hermes state database not found: {}", db_path.display());
            }
            return Ok(SourceResult {
                source_name: self.name().to_string(),
                data: EventsSnapshotData { events },
                fetched_at: chrono::Utc::now().to_rfc3339(),
                error: None,
            });
        }

        // Copy DB to temp dir for safe read-only access
        let temp_dir = std::env::temp_dir();
        let temp_db = temp_dir.join(format!("hermes-{}.db", uuid::Uuid::new_v4()));
        let temp_wal = temp_dir.join(format!("hermes-{}.db-wal", uuid::Uuid::new_v4()));
        let temp_shm = temp_dir.join(format!("hermes-{}.db-shm", uuid::Uuid::new_v4()));

        fs::copy(&db_path, &temp_db)?;
        let wal_path = db_path.with_extension("db-wal");
        let shm_path = db_path.with_extension("db-shm");
        if wal_path.exists() {
            let _ = fs::copy(&wal_path, &temp_wal);
        }
        if shm_path.exists() {
            let _ = fs::copy(&shm_path, &temp_shm);
        }

        let conn = Connection::open(&temp_db)?;

        // Determine date boundaries in seconds since Unix epoch
        let since_seconds: i64 = if let Some(ref eff) = effective_since {
            chrono::DateTime::parse_from_rfc3339(&format!("{}T00:00:00Z", eff))
                .map(|dt| dt.timestamp())
                .unwrap_or(0)
        } else {
            0
        };

        let end_seconds: i64 = if let Some(ref ed) = end_date {
            chrono::DateTime::parse_from_rfc3339(&format!("{}T23:59:59Z", ed))
                .map(|dt| dt.timestamp())
                .unwrap_or(i64::MAX)
        } else {
            i64::MAX
        };

        // Query raw sessions from SQLite
        let mut stmt = conn.prepare(
            "SELECT id, model, started_at, ended_at, input_tokens, output_tokens,
                    cache_write_tokens, cache_read_tokens, reasoning_tokens,
                    cwd, estimated_cost_usd, actual_cost_usd
             FROM sessions
             WHERE started_at >= ? AND started_at <= ?"
        )?;

        let session_rows = stmt.query_map([since_seconds, end_seconds], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "unknown".to_string()),
                row_unix_ts(row, 2)?,
                row_opt_unix_ts(row, 3)?,
                row_i64_flex(row, 4)?,
                row_i64_flex(row, 5)?,
                row_i64_flex(row, 6)?,
                row_i64_flex(row, 7)?,
                row_i64_flex(row, 8)?,
                row.get::<_, String>(9).unwrap_or_default(),
                row.get::<_, Option<f64>>(10)?,
                row.get::<_, Option<f64>>(11)?,
            ))
        })?;

        let mut daily_sums: HashMap<(String, String), DailySum> = HashMap::new();

        for session in session_rows {
            let (
                session_id,
                model,
                started_at,
                ended_at,
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
                reasoning_tokens,
                cwd,
                estimated_cost,
                actual_cost,
            ) = session?;

            let total = input_tokens + output_tokens + cache_read_tokens + cache_write_tokens;
            if total == 0 {
                continue;
            }

            let date = chrono::DateTime::from_timestamp(started_at, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            if date.is_empty() {
                continue;
            }

            // Date filter
            if let Some(ref eff) = effective_since {
                if &date < eff {
                    continue;
                }
            }
            if let Some(ref ed) = end_date {
                if &date > ed {
                    continue;
                }
            }

            let reported = actual_cost.unwrap_or(estimated_cost.unwrap_or(0.0));
            // Hermes estimates are often missing or absurd — fall back to public rates.
            let cost = resolve_reported_cost(
                &model,
                reported,
                input_tokens as u64,
                cache_read_tokens as u64,
                cache_write_tokens as u64,
                output_tokens as u64,
            );

            let entry = daily_sums.entry((date.clone(), model.clone())).or_default();
            entry.input += input_tokens;
            entry.output += output_tokens;
            entry.cache_write += cache_write_tokens;
            entry.cache_read += cache_read_tokens;
            entry.reasoning += reasoning_tokens;
            entry.cost += cost;
            if entry.cwd.is_empty() && !cwd.is_empty() {
                entry.cwd = cwd.clone();
            }

            // Build session event row
            let hashed_session_id = hash_project_name_sync(&session_id, *hash_projects);
            let hashed_proj = hash_project_name_sync(if cwd.is_empty() { &session_id } else { &cwd }, *hash_projects);

            let raw_session_key = format!(
                "hermes|{}|session|{}|{}|{}",
                machine_name, date, model, hashed_session_id
            );
            let session_dedup_key = make_dedup_key(&raw_session_key);

            let start_time = chrono::DateTime::from_timestamp(started_at, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            let end_time = ended_at
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string());

            events.push(EventRow {
                date: date.clone(),
                record_type: "session".to_string(),
                record_key: hashed_session_id.clone(),
                source: "hermes".to_string(),
                machine_name: machine_name.to_string(),
                account_id: String::new(),
                api_key_id: String::new(),
                model_name: model.clone(),
                session_id: hashed_session_id,
                project_path: hashed_proj,
                input_tokens: input_tokens as u64,
                output_tokens: output_tokens as u64,
                cache_creation_tokens: cache_write_tokens as u64,
                cache_read_tokens: cache_read_tokens as u64,
                reasoning_tokens: reasoning_tokens as u64,
                total_tokens: total as u64,
                cost,
                dedup_key: session_dedup_key,
                import_id: import_id.to_string(),
                start_time: Some(start_time),
                end_time,
                actual_end_time: None,
                is_active: if ended_at.is_some() { 0 } else { 1 },
                is_gap: 0,
                entries: 1,
                burn_rate: 0.0,
                projection: 0.0,
                usage_limit_reset_time: None,
                block_id: String::new(),
                created_at: now.clone(),
                updated_at: now.clone(),
            });
        }

        // Build daily event rows
        for ((date, model), sum) in &daily_sums {
            let total = sum.input + sum.output + sum.cache_read + sum.cache_write;
            let hashed_proj = hash_project_name_sync(
                if sum.cwd.is_empty() { "unknown" } else { &sum.cwd },
                *hash_projects,
            );

            let raw_daily_key = format!(
                "hermes|{}|daily|{}|{}|{}",
                machine_name, date, model, date
            );
            let daily_dedup_key = make_dedup_key(&raw_daily_key);

            // Round daily sum to cents for stable display.
            let cost = (sum.cost * 100.0).round() / 100.0;

            events.push(EventRow {
                date: date.clone(),
                record_type: "daily".to_string(),
                record_key: date.clone(),
                source: "hermes".to_string(),
                machine_name: machine_name.to_string(),
                account_id: String::new(),
                api_key_id: String::new(),
                model_name: model.clone(),
                session_id: String::new(),
                project_path: hashed_proj,
                input_tokens: sum.input as u64,
                output_tokens: sum.output as u64,
                cache_creation_tokens: sum.cache_write as u64,
                cache_read_tokens: sum.cache_read as u64,
                reasoning_tokens: sum.reasoning as u64,
                total_tokens: total as u64,
                cost,
                dedup_key: daily_dedup_key,
                import_id: import_id.to_string(),
                start_time: None,
                end_time: None,
                actual_end_time: None,
                is_active: 0,
                is_gap: 0,
                entries: 1,
                burn_rate: 0.0,
                projection: 0.0,
                usage_limit_reset_time: None,
                block_id: String::new(),
                created_at: now.clone(),
                updated_at: now.clone(),
            });
        }

        if *verbose {
            eprintln!("Hermes Source parsed {} rows.", events.len());
        }

        Ok(SourceResult {
            source_name: self.name().to_string(),
            data: EventsSnapshotData { events },
            fetched_at: chrono::Utc::now().to_rfc3339(),
            error: None,
        })
    }
}

#[derive(Default)]
struct DailySum {
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    reasoning: i64,
    cost: f64,
    cwd: String,
}

/// Hermes `state.db` stores unix timestamps as REAL (float seconds).
fn row_unix_ts(row: &rusqlite::Row, idx: usize) -> rusqlite::Result<i64> {
    if let Ok(v) = row.get::<_, i64>(idx) {
        return Ok(v);
    }
    let f: f64 = row.get(idx)?;
    Ok(f as i64)
}

fn row_opt_unix_ts(row: &rusqlite::Row, idx: usize) -> rusqlite::Result<Option<i64>> {
    match row.get_ref(idx)?.data_type() {
        rusqlite::types::Type::Null => Ok(None),
        _ => Ok(Some(row_unix_ts(row, idx)?)),
    }
}

fn row_i64_flex(row: &rusqlite::Row, idx: usize) -> rusqlite::Result<i64> {
    if row.get_ref(idx)?.data_type() == rusqlite::types::Type::Null {
        return Ok(0);
    }
    if let Ok(v) = row.get::<_, i64>(idx) {
        return Ok(v);
    }
    let f: f64 = row.get(idx)?;
    Ok(f as i64)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_name() {
        let src = HermesSource::new(HermesSourceOptions {
            machine_name: "m1".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "".into(),
            base_dir: None,
        });
        assert_eq!(src.name(), "hermes");
    }

    #[test]
    fn fetch_reads_real_started_at_like_k3s_state_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT, model TEXT, started_at REAL, ended_at REAL,
                input_tokens REAL, output_tokens REAL,
                cache_write_tokens REAL, cache_read_tokens REAL,
                reasoning_tokens REAL, cwd TEXT,
                estimated_cost_usd REAL, actual_cost_usd REAL
            );
            INSERT INTO sessions VALUES (
                'sess-1', 'google/gemini-2.5-flash',
                1754784000.25, 1754784060.9,
                100.0, 20.0, 0.0, 10.0, 5.0,
                '/opt/workspace', 0.001, NULL
            );",
        )
        .unwrap();
        drop(conn);

        let src = HermesSource::new(HermesSourceOptions {
            machine_name: "duet-ubuntu-hermes".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "test-import".into(),
            base_dir: Some(tmp.path().to_path_buf()),
        });
        let result = futures::executor::block_on(async move { src.fetch().await }).unwrap();

        assert!(result.error.is_none(), "{:?}", result.error);
        let sessions: Vec<_> = result
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "session")
            .collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source, "hermes");
        assert_eq!(sessions[0].machine_name, "duet-ubuntu-hermes");
        assert_eq!(sessions[0].input_tokens, 100);
        assert_eq!(sessions[0].output_tokens, 20);
        assert_eq!(sessions[0].cache_read_tokens, 10);
        assert_eq!(sessions[0].cache_creation_tokens, 0);
        assert_eq!(sessions[0].total_tokens, 130);
        assert_eq!(sessions[0].date, "2025-08-10");
    }

    #[test]
    fn fetch_tolerates_null_model_and_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(tmp.path().join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT, model TEXT, started_at REAL, ended_at REAL,
                input_tokens REAL, output_tokens REAL,
                cache_write_tokens REAL, cache_read_tokens REAL,
                reasoning_tokens REAL, cwd TEXT,
                estimated_cost_usd REAL, actual_cost_usd REAL
            );
            INSERT INTO sessions VALUES (
                'empty', NULL, 1754784000, NULL,
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
            );
            INSERT INTO sessions VALUES (
                'no-model', NULL, 1754784000, 1754784060,
                50, 5, NULL, NULL, NULL, '/w', NULL, NULL
            );
            INSERT INTO sessions VALUES (
                'ok', 'google/gemini-2.5-flash', 1754784000, 1754784060,
                100, 20, 0, 10, 5, '/w', 0.001, NULL
            );",
        )
        .unwrap();
        drop(conn);

        let src = HermesSource::new(HermesSourceOptions {
            machine_name: "m1".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "t".into(),
            base_dir: Some(tmp.path().to_path_buf()),
        });
        let result = futures::executor::block_on(async move { src.fetch().await }).unwrap();

        let mut models: Vec<_> = result
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "session")
            .map(|e| (e.model_name.as_str(), e.total_tokens))
            .collect();
        models.sort();
        assert_eq!(models, vec![("google/gemini-2.5-flash", 130), ("unknown", 55)]);
    }

    #[test]
    fn test_fetch_returns_ok_when_missing_db() {
        let tmp = tempfile::tempdir().unwrap();
        let src = HermesSource::new(HermesSourceOptions {
            machine_name: "m1".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "".into(),
            base_dir: Some(tmp.path().join("missing-hermes")),
        });
        let result = futures::executor::block_on(async move { src.fetch().await });

        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.source_name, "hermes");
        assert!(r.data.events.is_empty());
    }
}
