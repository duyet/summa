use crate::config::Config;
use crate::model::DataSink;
use crate::script::events_io::{open_connection, read_event_rows};
use crate::sink::clickhouse::ClickHouseSink;

#[derive(clap::Args, Debug, Clone)]
pub struct PublishArgs {
    /// Path to TOML config file
    #[arg(short, long)]
    pub config: Option<String>,
}

pub async fn run(args: PublishArgs) -> anyhow::Result<()> {
    let cfg = Config::load(args.config.as_deref())?;
    let duckdb_path = Config::resolve_duckdb_path(cfg.importer.duckdb_path.as_deref());

    println!("publish: {} → clickhouse {}", duckdb_path, cfg.clickhouse.host);

    let md_token = std::env::var("MOTHERDUCK_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let conn = open_connection(&duckdb_path, &md_token)?;
    let events = read_event_rows(&conn, "")?;

    if events.is_empty() {
        println!("no data to publish");
        return Ok(());
    }

    let mut sink = ClickHouseSink::new();
    sink.connect().await?;
    let result = sink.write(crate::model::EventsSnapshotData { events }).await?;

    println!("published: {} rows to {}", result.rows_written.values().sum::<u64>(), sink.name());
    Ok(())
}
