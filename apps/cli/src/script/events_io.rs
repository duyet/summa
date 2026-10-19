//! Shared DuckDB read used by publish (ClickHouse) and d1-sync (cloud hub).
use crate::model::EventRow;

pub fn read_event_rows(
    conn: &duckdb::Connection,
    where_clause: &str,
) -> anyhow::Result<Vec<EventRow>> {
    let sql = format!(
        "SELECT date, record_type, record_key, source, machine_name, model_name, \
         session_id, project_path, input_tokens, output_tokens, \
         cache_creation_tokens, cache_read_tokens, reasoning_tokens, total_tokens, cost, \
         dedup_key, import_id, block_id, start_time, end_time, actual_end_time, \
         is_active, is_gap, entries, burn_rate, projection, usage_limit_reset_time, \
         created_at, updated_at, account_id, api_key_id \
         FROM ccusage_events {where_clause}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;

    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(EventRow {
            date: row.get(0)?,
            record_type: row.get(1)?,
            record_key: row.get(2)?,
            source: row.get(3)?,
            machine_name: row.get(4)?,
            account_id: row.get(29).unwrap_or_default(),
            api_key_id: row.get(30).unwrap_or_default(),
            model_name: row.get(5)?,
            session_id: row.get(6)?,
            project_path: row.get(7)?,
            input_tokens: row.get(8)?,
            output_tokens: row.get(9)?,
            cache_creation_tokens: row.get(10)?,
            cache_read_tokens: row.get(11)?,
            reasoning_tokens: row.get(12)?,
            total_tokens: row.get(13)?,
            cost: row.get(14)?,
            dedup_key: row.get(15)?,
            import_id: row.get(16)?,
            block_id: row.get(17)?,
            start_time: row.get(18)?,
            end_time: row.get(19)?,
            actual_end_time: row.get(20)?,
            is_active: row.get(21)?,
            is_gap: row.get(22)?,
            entries: row.get(23)?,
            burn_rate: row.get(24)?,
            projection: row.get(25)?,
            usage_limit_reset_time: row.get(26)?,
            created_at: row.get(27)?,
            updated_at: row.get(28)?,
        });
    }
    Ok(events)
}

/// MotherDuck needs its token on the connection string (`md:path?motherduck_token=…`).
pub fn open_connection(
    duckdb_path: &str,
    motherduck_token: &str,
) -> anyhow::Result<duckdb::Connection> {
    if duckdb_path.starts_with("md:") && !motherduck_token.is_empty() {
        let sep = if duckdb_path.contains('?') { '&' } else { '?' };
        let path = format!("{duckdb_path}{sep}motherduck_token={motherduck_token}");
        return Ok(duckdb::Connection::open(&path)?);
    }
    Ok(duckdb::Connection::open(duckdb_path)?)
}
