use crate::config::Config;

pub async fn run(args: crate::cli::CheckArgs) -> anyhow::Result<()> {
    crate::util::load_dotenv();
    let cfg = Config::load(args.config.as_deref())?;
    let duckdb_path = Config::resolve_duckdb_path(cfg.importer.duckdb_path.as_deref());

    let db = duckdb_path.clone();
    let summary = tokio::task::spawn_blocking(move || summary_blocking(&db))
        .await??;
    let db = duckdb_path.clone();
    let model_rows = tokio::task::spawn_blocking(move || model_rows_blocking(&db))
        .await??;
    let db = duckdb_path.clone();
    let source_rows = tokio::task::spawn_blocking(move || source_rows_blocking(&db))
        .await??;

    if args.json {
        let out = serde_json::json!({
            "duckdb": duckdb_path,
            "date_range": [summary.0, summary.1],
            "records": summary.2,
            "tokens": {
                "input": summary.3,
                "output": summary.4,
                "total": summary.5,
            },
            "cost_usd": summary.6,
            "models": model_rows.into_iter().map(|(model, count, total, cost)| {
                serde_json::json!({"model": model, "records": count, "tokens": total, "cost_usd": cost})
            }).collect::<Vec<_>>(),
            "sources": source_rows.iter().map(|(source, record_type, count, total, cost)| {
                serde_json::json!({
                    "source": source,
                    "record_type": record_type,
                    "records": count,
                    "tokens": total,
                    "cost_usd": cost
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("duckdb: {}", duckdb_path);
    println!(
        "  date range: {} → {}",
        summary.0.unwrap_or_default(),
        summary.1.unwrap_or_default()
    );
    println!("  records: {}", summary.2);
    println!("  tokens: input={} output={} total={}", summary.3, summary.4, summary.5);
    println!("  cost: ${:.2}", summary.6);

    println!("\nmodels:");
    for (model, count, total, cost) in model_rows {
        println!(
            "  {}: {} records, tokens={}, cost=${:.2}",
            model, count, total, cost
        );
    }

    println!("\nsources:");
    for (source, record_type, count, total, cost) in source_rows {
        println!(
            "  {}/{}: {} records, tokens={}, cost=${:.2}",
            source, record_type, count, total, cost
        );
    }

    Ok(())
}

fn summary_blocking(
    db_path: &str,
) -> anyhow::Result<(Option<String>, Option<String>, i64, u64, u64, u64, f64)> {
    let conn = open_db(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT CAST(min(date) AS VARCHAR), CAST(max(date) AS VARCHAR), count(*), \
         sum(input_tokens), sum(output_tokens), sum(total_tokens), sum(cost) \
         FROM ccusage_events",
    )?;
    let mut rows = stmt.query([])?;
    if let Some(row) = rows.next()? {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ))
    } else {
        Ok((None, None, 0, 0, 0, 0, 0.0))
    }
}

fn model_rows_blocking(
    db_path: &str,
) -> anyhow::Result<Vec<(String, i64, u64, f64)>> {
    let conn = open_db(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(model_name, ''), count(*), \
         COALESCE(sum(total_tokens), 0), COALESCE(sum(cost), 0) \
         FROM ccusage_events \
         GROUP BY 1 \
         ORDER BY 4 DESC",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?));
    }
    Ok(out)
}

fn source_rows_blocking(
    db_path: &str,
) -> anyhow::Result<Vec<(String, String, i64, u64, f64)>> {
    let conn = open_db(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(source, ''), COALESCE(record_type, ''), count(*), \
         COALESCE(sum(total_tokens), 0), COALESCE(sum(cost), 0) \
         FROM ccusage_events \
         GROUP BY 1, 2 \
         ORDER BY 1, 2",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ));
    }
    Ok(out)
}

fn open_db(db_path: &str) -> anyhow::Result<duckdb::Connection> {
    if db_path.starts_with("md:") {
        crate::sink::duckdb::DuckDbSink::new(db_path).open_for_query()
    } else {
        Ok(duckdb::Connection::open(db_path)?)
    }
}
