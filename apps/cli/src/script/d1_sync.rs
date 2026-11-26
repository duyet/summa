//! Mirror daily rows from DuckDB/MotherDuck to the cloud hub.
//!
//! The hub fans out every ingested `daily` row into its D1 read mirror
//! (migration `0004_d1_analytics.sql`), which serves `/v1/analytics` when
//! MotherDuck and ClickHouse are both unreachable. Ongoing coverage is
//! automatic via `summa import`; this command is the official one-off backfill.
use crate::config::Config;
use crate::model::{DataSink, EventsSnapshotData};
use crate::script::events_io::{open_connection, read_event_rows};
use crate::telemetry::TelemetrySink;
use chrono::{Duration, Utc};

#[derive(clap::Args, Debug, Clone)]
pub struct D1SyncArgs {
    /// Path to TOML config file
    #[arg(short, long)]
    pub config: Option<String>,
    /// Days of daily-row history to mirror
    #[arg(long, default_value_t = 30)]
    pub days_back: i64,
    /// Mirror the entire known history instead of --days-back
    #[arg(long)]
    pub full: bool,
}

pub async fn run(args: D1SyncArgs) -> anyhow::Result<()> {
    crate::util::load_dotenv();

    let cfg = Config::load(args.config.as_deref())?;
    let duckdb_path = Config::resolve_duckdb_path(cfg.importer.duckdb_path.as_deref());
    let token = match cfg.telemetry.token.as_deref().filter(|s| !s.is_empty()) {
        Some(t) => t.to_string(),
        None => anyhow::bail!(
            "no telemetry token: set telemetry_token in credentials.toml or config [telemetry].token"
        ),
    };
    let endpoint = cfg.telemetry.endpoint.as_deref();

    // Config::load exports MOTHERDUCK_TOKEN from credentials.toml when set.
    let conn = open_connection(&duckdb_path)?;

    let where_clause = if args.full {
        "WHERE record_type = 'daily'".to_string()
    } else {
        let since = Utc::now().date_naive() - Duration::days(args.days_back.max(0));
        format!("WHERE record_type = 'daily' AND date >= '{}'", since.format("%Y-%m-%d"))
    };

    let events = read_event_rows(&conn, &where_clause)?;
    println!(
        "d1-sync: {} daily rows → {}/v1/ingest (D1 mirror)",
        events.len(),
        endpoint.unwrap_or("https://summa.duyet.net")
    );
    if events.is_empty() {
        println!("nothing to mirror");
        return Ok(());
    }

    let mut sink = TelemetrySink::from_parts(endpoint, Some(&token))
        .ok_or_else(|| anyhow::anyhow!("no telemetry token"))?;
    sink.connect().await?;
    let result = sink.write(EventsSnapshotData { events }).await?;

    let accepted = result.rows_written.get("ccusage_events").copied().unwrap_or(0);
    println!("mirrored: {accepted} rows through the hub");
    if let Some(e) = &result.error {
        anyhow::bail!("hub could not persist the batch: {e}");
    }
    Ok(())
}
