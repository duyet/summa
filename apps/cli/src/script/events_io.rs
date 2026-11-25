//! Shared DuckDB read used by publish (ClickHouse) and d1-sync (cloud hub).
use crate::model::EventRow;
use crate::sink::duckdb::DuckDbSink;

/// Read column `idx`, mapping SQL NULL to the type's default. Type or index
/// errors still propagate, so a schema mismatch fails loudly instead of
/// shipping zeroed rows.
fn col<T: duckdb::types::FromSql + Default>(
    row: &duckdb::Row<'_>,
    idx: usize,
) -> duckdb::Result<T> {
    Ok(row.get::<_, Option<T>>(idx)?.unwrap_or_default())
}

pub fn read_event_rows(
    conn: &duckdb::Connection,
    where_clause: &str,
) -> anyhow::Result<Vec<EventRow>> {
    // Timestamp and date columns are cast to VARCHAR: EventRow carries them as
    // strings, and duckdb-rs does not convert TIMESTAMP/DATE to String.
    let sql = format!(
        "SELECT CAST(date AS VARCHAR), record_type, record_key, source, machine_name, model_name, \
         session_id, project_path, input_tokens, output_tokens, \
         cache_creation_tokens, cache_read_tokens, reasoning_tokens, total_tokens, cost, \
         dedup_key, import_id, block_id, \
         CAST(start_time AS VARCHAR), CAST(end_time AS VARCHAR), CAST(actual_end_time AS VARCHAR), \
         is_active, is_gap, entries, burn_rate, projection, CAST(usage_limit_reset_time AS VARCHAR), \
         CAST(created_at AS VARCHAR), CAST(updated_at AS VARCHAR), account_id, api_key_id \
         FROM ccusage_events {where_clause}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;

    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(EventRow {
            date: col(row, 0)?,
            record_type: col(row, 1)?,
            record_key: col(row, 2)?,
            source: col(row, 3)?,
            machine_name: col(row, 4)?,
            account_id: col(row, 29)?,
            api_key_id: col(row, 30)?,
            model_name: col(row, 5)?,
            session_id: col(row, 6)?,
            project_path: col(row, 7)?,
            input_tokens: col(row, 8)?,
            output_tokens: col(row, 9)?,
            cache_creation_tokens: col(row, 10)?,
            cache_read_tokens: col(row, 11)?,
            reasoning_tokens: col(row, 12)?,
            total_tokens: col(row, 13)?,
            cost: col(row, 14)?,
            dedup_key: col(row, 15)?,
            import_id: col(row, 16)?,
            block_id: col(row, 17)?,
            start_time: col(row, 18)?,
            end_time: col(row, 19)?,
            actual_end_time: col(row, 20)?,
            is_active: col(row, 21)?,
            is_gap: col(row, 22)?,
            entries: col(row, 23)?,
            burn_rate: col(row, 24)?,
            projection: col(row, 25)?,
            usage_limit_reset_time: col(row, 26)?,
            created_at: col(row, 27)?,
            updated_at: col(row, 28)?,
        });
    }
    Ok(events)
}

/// Open the local DuckDB file or the `md:` MotherDuck database. Delegates to
/// the sink so the extension load and the token redaction in connection errors
/// stay in one place.
pub fn open_connection(duckdb_path: &str) -> anyhow::Result<duckdb::Connection> {
    DuckDbSink::new(duckdb_path).open_connection()
}
