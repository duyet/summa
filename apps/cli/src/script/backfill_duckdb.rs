use crate::config::ClickHouseConfig;
use crate::model::EventsSnapshotData;
use crate::sink::duckdb::DuckDbSink;
use crate::sink::DataSink;
use crate::util::timer::CommandTimeout;
use reqwest::Client;
use std::env;
use std::time::Duration;

/// Backfill DuckDB/MotherDuck from ClickHouse.
///
/// Reads all rows from ClickHouse `ccusage_events` and writes them
/// to the configured DuckDB path.
pub fn run() -> anyhow::Result<()> {
    crate::util::load_dotenv();

    let args: Vec<String> = env::args().collect();
    let verbose = args.contains(&"--verbose".into()) || args.contains(&"-v".into());

    // Load XDG/local config + credentials so CH_* match import path.
    let file_cfg = crate::config::Config::load(None).unwrap_or_default();
    crate::script::import_all::apply_config_to_env(&file_cfg);

    let explicit = args
        .iter()
        .find(|a| a.starts_with("--path="))
        .map(|a| a.split('=').nth(1).unwrap_or(""))
        .filter(|s| !s.is_empty())
        .or(file_cfg.importer.duckdb_path.as_deref().filter(|s| !s.is_empty()));
    let path = crate::config::Config::resolve_duckdb_path(explicit);

    println!("Backfill: ClickHouse ccusage_events → {}", path);

    let cfg = ClickHouseConfig::from_env();
    let timeout = CommandTimeout::from_ms(
        env::var("IMPORT_COMMAND_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok()),
    )
    .duration()
    .unwrap_or_else(|| Duration::from_secs(30));

    let client = Client::builder()
        .timeout(timeout)
        .build()
        .expect("build reqwest client");

    let url = format!(
        "{}://{}:{}/?query={}",
        cfg.protocol,
        cfg.host,
        cfg.port,
        crate::sink::clickhouse::percent_encode("SELECT * FROM ccusage_events")
    );

    let rows: Vec<serde_json::Value> = client
        .get(&url)
        .basic_auth(
            cfg.user.clone(),
            if cfg.password.is_empty() {
                None
            } else {
                Some(cfg.password.clone())
            },
        )
        .send()
        .and_then(|r| r.error_for_status())
        .expect("fetch from ClickHouse")
        .json()
        .expect("parse ClickHouse response");

    println!("  ccusage_events: {} rows", rows.len());

    let events = rows
        .into_iter()
        .map(|v| v)
        .collect::<Vec<serde_json::Value>>();

    println!("\nWriting {} rows to {}...", events.len(), path);

    let mut sink = DuckDbSink::new(path);
    sink.connect().expect("connect duckdb");

    let start = std::time::Instant::now();
    let result = sink
        .write(EventsSnapshotData { events })
        .expect("write to duckdb");
    sink.close().expect("close duckdb");

    let total = result.rows_written.values().copied::<u64>().sum::<u64>();
    println!(
        "Done: {} rows in {}ms",
        total,
        start.elapsed().as_millis()
    );
    for (table, count) in result.rows_written {
        println!("  {}: {}", table, count);
    }

    Ok(())
}
