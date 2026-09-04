//! Full import: register sources/sinks, run pipeline, print summary.
//!
//! Exit policy (cron-friendly): exit 0 when at least one sink completes
//! without error. ClickHouse may be down while MotherDuck/local DuckDB
//! still succeeds — that is a successful run for the hourly job.

use crate::cli::ImportArgs;
use crate::config::{Config, CREDENTIALS_ENV};
use crate::fetcher::companion::CompanionSource;
use crate::pipeline::ImportRunner;
use crate::sink::clickhouse::ClickHouseSink;
use crate::sink::duckdb::DuckDbSink;
use crate::source::antigravity::{AntigravitySource, AntigravitySourceOptions};
use crate::source::ccusage::{CcusageSource, CcusageSourceOptions};
use crate::source::companion::{CompanionSource as CompanionDataSource, CompanionSourceOptions};
use crate::source::cursor::{CursorSource, CursorSourceOptions};
use crate::source::grok::{GrokSource, GrokSourceOptions};
use crate::source::grok_api::{GrokApiSource, GrokApiSourceOptions};
use crate::source::hermes::{HermesSource, HermesSourceOptions};
use crate::util::date::resolve_effective_since;
use std::env;

/// Companion agents registered by default (mirrors TS `CCUSAGE_AGENT_SOURCES`).
const COMPANION_AGENTS: &[CompanionSource] = &[
    CompanionSource::Codex,
    CompanionSource::OpenCode,
    CompanionSource::Gemini,
    CompanionSource::OpenClaw,
    CompanionSource::Amp,
    CompanionSource::Droid,
    CompanionSource::Codebuff,
    CompanionSource::Pi,
    CompanionSource::Goose,
    CompanionSource::Kilo,
    CompanionSource::Copilot,
    CompanionSource::Kimi,
    CompanionSource::Qwen,
];

/// Resolved import settings after loading config + credentials + CLI overrides.
/// This is the single source of truth for the import path (and tests).
#[derive(Debug, Clone)]
pub struct PreparedImport {
    pub config: Config,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub duckdb_path: String,
    pub skip_clickhouse: bool,
    pub skip_duckdb: bool,
    pub machine_name: String,
    pub hash_projects: bool,
}

/// Load config (local / XDG / `--config`), merge separate credentials, apply
/// CLI overrides, and export ClickHouse/DuckDB settings into the process env
/// so sinks that read env (ClickHouse) see the same values.
///
/// Precedence (high → low): CLI flags → config/credentials files → env fallbacks → defaults.
pub fn prepare_import(args: &ImportArgs) -> anyhow::Result<PreparedImport> {
    prepare_import_with_credentials(args, None)
}

/// Same as [`prepare_import`] but allows an explicit credentials path (tests
/// and `SUMMA_CREDENTIALS` callers). When `credentials_path` is `None`,
/// discovery uses `$SUMMA_CREDENTIALS` then XDG / local candidates.
pub fn prepare_import_with_credentials(
    args: &ImportArgs,
    credentials_path: Option<&str>,
) -> anyhow::Result<PreparedImport> {
    let creds_path = credentials_path
        .map(str::to_string)
        .or_else(|| env::var(CREDENTIALS_ENV).ok());

    let mut cfg = Config::load_with_credentials(args.config.as_deref(), creds_path.as_deref())?;

    // CLI overrides win over file config.
    if let Some(ref host) = args.ch_host {
        cfg.clickhouse.host = host.clone();
    }
    if let Some(port) = args.ch_port {
        cfg.clickhouse.port = port;
    }
    if let Some(ref db) = args.ch_database {
        cfg.clickhouse.database = db.clone();
    }
    if let Some(ref path) = args.duckdb_path {
        cfg.importer.duckdb_path = Some(path.clone());
    }
    if let Some(days) = args.days_back {
        cfg.importer.days_back = Some(days);
    }
    if let Some(ref since) = args.since {
        cfg.importer.since = Some(since.clone());
    }
    if let Some(ref end) = args.end_date {
        cfg.importer.end_date = Some(end.clone());
    }

    // Export resolved config so ClickHouseSink::from_env and other env readers match files.
    apply_config_to_env(&cfg);

    let days_back = cfg.importer.days_back.or_else(|| {
        env::var("IMPORT_DAYS_BACK")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
    });
    let since = cfg
        .importer
        .since
        .clone()
        .or_else(|| env::var("IMPORT_SINCE").ok())
        .or_else(|| env::var("IMPORT_SINCE_DATE").ok());
    let end_date = cfg
        .importer
        .end_date
        .clone()
        .or_else(|| env::var("IMPORT_END_DATE").ok());

    // CLI/config path → env (now set) → local default.
    let duckdb_path = Config::resolve_duckdb_path(cfg.importer.duckdb_path.as_deref());

    let skip_clickhouse = args.skip_clickhouse
        || cfg.importer.skip_clickhouse.unwrap_or(false);
    let skip_duckdb = args.skip_duckdb;

    let machine_name = cfg
        .importer
        .machine_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(hostname);

    let hash_projects = cfg
        .importer
        .hash_project_names
        .unwrap_or_else(|| {
            env::var("HASH_PROJECT_NAMES")
                .map(|v| v != "false")
                .unwrap_or(true)
        });

    Ok(PreparedImport {
        config: cfg,
        days_back,
        since,
        end_date,
        duckdb_path,
        skip_clickhouse,
        skip_duckdb,
        machine_name,
        hash_projects,
    })
}

/// Write non-empty config fields into process env (ClickHouse sink + cron compatibility).
pub fn apply_config_to_env(cfg: &Config) {
    for (key, value) in cfg.to_env_map() {
        if !value.is_empty() {
            env::set_var(key, value);
        }
    }
}

/// Run the full import pipeline from clap-parsed args.
pub async fn run(args: ImportArgs, verbose: bool) -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let mut args = args;
    let prepared = prepare_import(&args)?;
    apply_importer_skips(&mut args, &prepared.config.importer);
    let days_back = prepared.days_back;
    let since = prepared.since.clone();
    let end_date = prepared.end_date.clone();
    let effective_since = resolve_effective_since(since.as_deref(), days_back);

    let import_id = uuid::Uuid::new_v4().to_string();
    let machine_name = prepared.machine_name.clone();
    let hash_projects = prepared.hash_projects;

    println!(
        "summa — machine: {}{}{}, import: {}",
        machine_name,
        effective_since
            .as_ref()
            .map(|s| format!(", since: {}", s))
            .unwrap_or_default(),
        end_date
            .as_ref()
            .map(|e| format!(", until: {}", e))
            .unwrap_or_default(),
        import_id
    );
    if verbose {
        eprintln!(
            "  config: ch_host={} duckdb={} password_set={}",
            prepared.config.clickhouse.host,
            prepared.duckdb_path,
            !prepared.config.clickhouse.password.is_empty()
        );
    }

    if args.dry_run {
        println!("dry-run: skipping source fetch and sink writes");
        println!(
            "  would use duckdb={} skip_ch={} skip_duckdb={}",
            prepared.duckdb_path, prepared.skip_clickhouse, prepared.skip_duckdb
        );
        println!("\n=== Summary ===");
        println!("  source (dry-run): 0 rows");
        println!("  sink (dry-run): 0 rows, 0ms");
        println!("  total: 0ms");
        return Ok(());
    }

    let mut sources: Vec<Box<dyn crate::model::DataSource>> = Vec::new();

    if !args.skip_ccusage {
        sources.push(Box::new(CcusageSource::new(CcusageSourceOptions {
            machine_name: machine_name.clone(),
            hash_projects: Some(hash_projects),
            timeout: prepared
                .config
                .importer
                .command_timeout
                .or_else(|| {
                    env::var("IMPORT_COMMAND_TIMEOUT_MS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                }),
            verbose: Some(verbose),
            days_back,
            since: effective_since.clone(),
            end_date: end_date.clone(),
            import_id: Some(import_id.clone()),
        })));
    }

    if !args.skip_antigravity {
        sources.push(Box::new(AntigravitySource::new(AntigravitySourceOptions {
            machine_name: machine_name.clone(),
            hash_projects,
            verbose,
            days_back,
            since: effective_since.clone(),
            end_date: end_date.clone(),
            import_id: import_id.clone(),
            cli_dir: None,
        })));
    }

    if !args.skip_hermes {
        sources.push(Box::new(HermesSource::new(HermesSourceOptions {
            machine_name: machine_name.clone(),
            hash_projects,
            verbose,
            days_back,
            since: effective_since.clone(),
            end_date: end_date.clone(),
            import_id: import_id.clone(),
            base_dir: None,
        })));
    }

    if !args.skip_grok {
        sources.push(Box::new(GrokSource::new(GrokSourceOptions {
            machine_name: machine_name.clone(),
            hash_projects,
            verbose,
            days_back,
            since: effective_since.clone(),
            end_date: end_date.clone(),
            import_id: import_id.clone(),
            base_dir: None,
        })));
        sources.push(Box::new(GrokApiSource::new(GrokApiSourceOptions {
            verbose,
            import_id: import_id.clone(),
            auth_path: None,
            disable_network: false,
        })));
    }

    if !args.skip_cursor {
        sources.push(Box::new(CursorSource::new(CursorSourceOptions {
            verbose,
            days_back,
            since: effective_since.clone(),
            end_date: end_date.clone(),
            import_id: import_id.clone(),
            session: None,
            api_key: None,
            state_db_path: None,
            disable_local_auth: false,
        })));
    }

    for agent in COMPANION_AGENTS {
        let id = agent.as_str();
        if should_skip_companion(&args, id) {
            continue;
        }
        sources.push(Box::new(CompanionDataSource::new(CompanionSourceOptions {
            source: *agent,
            machine_name: machine_name.clone(),
            hash_projects,
            timeout_ms: prepared
                .config
                .importer
                .command_timeout
                .or_else(|| {
                    env::var("IMPORT_COMMAND_TIMEOUT_MS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                }),
            verbose: Some(verbose),
            data_path: None,
            since: effective_since.clone(),
            end_date: end_date.clone(),
            import_id: import_id.clone(),
        })));
    }

    let mut sinks: Vec<Box<dyn crate::model::DataSink>> = Vec::new();
    let routes = prepared.config.sink_routes();

    if routes.clickhouse {
        sinks.push(Box::new(ClickHouseSink::new()));
    }

    if routes.local {
        sinks.push(Box::new(DuckDbSink::new(prepared.duckdb_path.clone())));
    } else if routes.motherduck {
        sinks.push(Box::new(DuckDbSink::new(prepared.duckdb_path.clone())));
    }

    if let Some(cloud) = crate::telemetry::TelemetrySink::from_parts(
        prepared.config.telemetry.endpoint.as_deref(),
        prepared.config.telemetry.token.as_deref(),
    ) {
        sinks.push(Box::new(cloud));
    }

    // Snapshot source: drop stale antigravity rows (including leftover estimates)
    // even when the new fetch emits nothing.
    if !args.skip_antigravity && (routes.local || routes.motherduck) {
        let mut sink = DuckDbSink::new(prepared.duckdb_path.clone());
        match sink.purge_source("antigravity") {
            Ok(n) => {
                if verbose {
                    eprintln!("purged {n} stale antigravity row(s) from {}", prepared.duckdb_path);
                }
            }
            Err(e) => {
                eprintln!("warning: could not purge antigravity from duckdb: {e}");
            }
        }
    }

    let mut runner = ImportRunner { sources, sinks };
    let result = runner.run().await?;

    println!("\n=== Summary ===");
    for source in &result.sources {
        println!(
            "  source {}: {} rows{}",
            source.name,
            source.rows,
            source
                .error
                .as_deref()
                .map(|e| format!(" (error: {})", e))
                .unwrap_or_default()
        );
    }
    for sink in &result.sinks {
        let total = sink.rows_written.values().copied().sum::<u64>();
        println!(
            "  sink {}: {} rows, {}ms{}",
            sink.sink_name,
            total,
            sink.duration_ms,
            sink
                .error
                .as_deref()
                .map(|e| format!(" (error: {})", e))
                .unwrap_or_default()
        );
    }
    println!("  total: {}ms", result.total_duration_ms);

    // Partial-success: at least one healthy sink is enough for cron exit 0.
    let any_sink_ok = result.sinks.iter().any(|s| s.error.is_none());
    if result.sinks.is_empty() || !any_sink_ok {
        anyhow::bail!("all sinks failed (or no sinks configured)");
    }
    Ok(())
}

pub fn apply_importer_skips(args: &mut ImportArgs, cfg: &crate::config::ImporterConfig) {
    args.skip_ccusage |= cfg.skip_ccusage.unwrap_or(false);
    args.skip_opencode |= cfg.skip_opencode.unwrap_or(false);
    args.skip_codex |= cfg.skip_codex.unwrap_or(false);
    args.skip_antigravity |= cfg.skip_antigravity.unwrap_or(false);
    args.skip_hermes |= cfg.skip_hermes.unwrap_or(false);
    args.skip_grok |= cfg.skip_grok.unwrap_or(false);
    args.skip_cursor |= cfg.skip_cursor.unwrap_or(false);
}

fn should_skip_companion(args: &ImportArgs, id: &str) -> bool {
    match id {
        "codex" => args.skip_codex,
        "opencode" => args.skip_opencode,
        _ => false,
    }
}

/// Source ids `summa import` would register for these skip flags (no I/O).
pub fn enabled_source_ids(args: &ImportArgs) -> Vec<&'static str> {
    let mut ids = Vec::new();
    if !args.skip_ccusage {
        ids.push("ccusage");
    }
    if !args.skip_antigravity {
        ids.push("antigravity");
    }
    if !args.skip_hermes {
        ids.push("hermes");
    }
    if !args.skip_grok {
        ids.push("grok");
        ids.push("grok-api");
    }
    if !args.skip_cursor {
        ids.push("cursor");
    }
    for agent in COMPANION_AGENTS {
        if !should_skip_companion(args, agent.as_str()) {
            ids.push(agent.as_str());
        }
    }
    ids
}

fn hostname() -> String {
    env::var("HOSTNAME")
        .or_else(|_| env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                })
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    #[test]
    fn clap_accepts_days_back_and_duckdb_path() {
        let cli = Cli::try_parse_from([
            "summa",
            "import",
            "--days-back",
            "2",
            "--duckdb-path",
            "md:ccusage",
        ])
        .expect("cron argv must parse");
        match cli.command {
            Commands::Import(a) => {
                assert_eq!(a.days_back, Some(2));
                assert_eq!(a.duckdb_path.as_deref(), Some("md:ccusage"));
            }
            _ => panic!("expected Import"),
        }
    }

    #[test]
    fn clap_accepts_since_and_days_back_together() {
        let cli = Cli::try_parse_from([
            "summa",
            "import",
            "--since",
            "2026-08-01",
            "--days-back",
            "7",
        ])
        .expect("both flags must parse");
        match cli.command {
            Commands::Import(a) => {
                assert_eq!(a.since.as_deref(), Some("2026-08-01"));
                assert_eq!(a.days_back, Some(7));
                let eff = resolve_effective_since(a.since.as_deref(), a.days_back);
                assert_eq!(eff.as_deref(), Some("2026-08-01"));
            }
            _ => panic!("expected Import"),
        }
    }

    #[test]
    fn default_duckdb_is_local_not_motherduck() {
        let _env = crate::test_env::EnvLock::isolate_summa();
        let path = crate::config::Config::resolve_duckdb_path(None);
        assert!(
            !path.starts_with("md:"),
            "default must be local file, got {path}"
        );
        assert!(path.ends_with("summa.duckdb") || path.contains("summa"));
    }

    /// End-to-end: the same prepare_import path used by `summa import` must
    /// load main config + separate credentials without CH_PASSWORD/DUCKDB_PATH
    /// in the environment.
    #[test]
    fn prepare_import_applies_config_and_credentials_files() {
        use std::io::Write;

        let _env = crate::test_env::EnvLock::isolate_summa();

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("summa.toml");
        let creds_path = dir.path().join("credentials.toml");
        let duck_path = dir.path().join("data").join("from-config.duckdb");

        {
            let mut f = std::fs::File::create(&config_path).unwrap();
            writeln!(
                f,
                r#"[clickhouse]
host = "ch.from-config.example"
port = 9440
user = "importer"
password = ""
database = "usage_db"
protocol = "https"

[importer]
duckdb_path = "{duck}"
days_back = 5
machine_name = "config-machine"
skip_clickhouse = false
"#,
                duck = duck_path.display()
            )
            .unwrap();
        }
        {
            let mut f = std::fs::File::create(&creds_path).unwrap();
            writeln!(
                f,
                r#"clickhouse_password = "secret-from-credentials-file"
motherduck_token = "md-from-credentials"
"#
            )
            .unwrap();
        }

        // Main TOML must not contain the password.
        let main_raw = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !main_raw.contains("secret-from-credentials-file"),
            "password must live only in credentials file"
        );

        let args = ImportArgs {
            config: Some(config_path.to_string_lossy().into()),
            since: None,
            days_back: None,
            end_date: None,
            duckdb_path: None,
            ch_host: None,
            ch_port: None,
            ch_database: None,
            skip_ccusage: true,
            skip_opencode: true,
            skip_codex: true,
            skip_antigravity: true,
            skip_hermes: true,
            skip_grok: true,
            skip_cursor: true,
            skip_clickhouse: false,
            skip_duckdb: false,
            dry_run: true,
        };

        let prepared = prepare_import_with_credentials(
            &args,
            Some(creds_path.to_str().unwrap()),
        )
        .expect("prepare_import must load config+credentials");

        assert_eq!(prepared.config.clickhouse.host, "ch.from-config.example");
        assert_eq!(prepared.config.clickhouse.port, 9440);
        assert_eq!(prepared.config.clickhouse.database, "usage_db");
        assert_eq!(
            prepared.config.clickhouse.password, "secret-from-credentials-file",
            "password must come from credentials.toml, not main config"
        );
        assert_eq!(
            prepared.duckdb_path,
            duck_path.to_string_lossy().as_ref(),
            "duckdb_path must come from importer config"
        );
        assert_eq!(prepared.days_back, Some(5));
        assert_eq!(prepared.machine_name, "config-machine");

        // Env export so ClickHouseSink::from_env sees the same values.
        assert_eq!(
            env::var("CH_PASSWORD").ok().as_deref(),
            Some("secret-from-credentials-file")
        );
        assert_eq!(
            env::var("CH_HOST").ok().as_deref(),
            Some("ch.from-config.example")
        );
        assert_eq!(
            env::var("DUCKDB_PATH").ok().as_deref(),
            Some(duck_path.to_string_lossy().as_ref())
        );
        assert_eq!(
            env::var("MOTHERDUCK_TOKEN").ok().as_deref(),
            Some("md-from-credentials")
        );
    }

    #[test]
    fn prepare_import_cli_overrides_config_duckdb_and_days() {
        use std::io::Write;

        let _env = crate::test_env::EnvLock::isolate_summa();

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("summa.toml");
        {
            let mut f = std::fs::File::create(&config_path).unwrap();
            writeln!(
                f,
                r#"[importer]
duckdb_path = "/from/config.duckdb"
days_back = 30
"#
            )
            .unwrap();
        }

        let args = ImportArgs {
            config: Some(config_path.to_string_lossy().into()),
            since: None,
            days_back: Some(2),
            end_date: None,
            duckdb_path: Some("/from/cli.duckdb".into()),
            ch_host: None,
            ch_port: None,
            ch_database: None,
            skip_ccusage: true,
            skip_opencode: true,
            skip_codex: true,
            skip_antigravity: true,
            skip_hermes: true,
            skip_grok: true,
            skip_cursor: true,
            skip_clickhouse: true,
            skip_duckdb: false,
            dry_run: true,
        };

        let prepared = prepare_import_with_credentials(&args, None).unwrap();
        assert_eq!(prepared.duckdb_path, "/from/cli.duckdb");
        assert_eq!(prepared.days_back, Some(2));
        assert!(prepared.skip_clickhouse);
    }

    fn skip_all_import_args() -> ImportArgs {
        ImportArgs {
            config: None,
            since: None,
            days_back: None,
            end_date: None,
            duckdb_path: None,
            ch_host: None,
            ch_port: None,
            ch_database: None,
            skip_ccusage: true,
            skip_opencode: true,
            skip_codex: true,
            skip_antigravity: true,
            skip_hermes: true,
            skip_grok: true,
            skip_cursor: true,
            skip_clickhouse: true,
            skip_duckdb: true,
            dry_run: true,
        }
    }

    #[test]
    fn skip_cursor_omits_cursor_source() {
        let mut args = skip_all_import_args();
        args.skip_cursor = true;
        args.skip_grok = false;
        let ids = enabled_source_ids(&args);
        assert!(!ids.contains(&"cursor"));
        assert!(ids.contains(&"grok"));
        assert!(ids.contains(&"grok-api"));
    }

    #[test]
    fn default_import_registers_cursor_and_grok() {
        let cli = Cli::try_parse_from(["summa", "import"]).expect("default import must parse");
        match cli.command {
            Commands::Import(args) => {
                assert!(!args.skip_cursor);
                assert!(!args.skip_grok);
                let ids = enabled_source_ids(&args);
                assert!(ids.contains(&"cursor"), "cursor must be on by default: {ids:?}");
                assert!(ids.contains(&"grok"));
                assert!(ids.contains(&"grok-api"));
            }
            _ => panic!("expected Import"),
        }
    }

    #[test]
    fn config_skip_cursor_omits_cursor_source() {
        let mut args = skip_all_import_args();
        args.skip_cursor = false;
        args.skip_grok = false;
        let cfg = crate::config::ImporterConfig {
            skip_cursor: Some(true),
            ..Default::default()
        };
        apply_importer_skips(&mut args, &cfg);
        let ids = enabled_source_ids(&args);
        assert!(!ids.contains(&"cursor"));
        assert!(ids.contains(&"grok"));
    }

    #[test]
    fn clap_skip_cursor_flag() {
        let cli = Cli::try_parse_from(["summa", "import", "--skip-cursor"]).unwrap();
        match cli.command {
            Commands::Import(args) => {
                assert!(args.skip_cursor);
                let ids = enabled_source_ids(&args);
                assert!(!ids.contains(&"cursor"));
            }
            _ => panic!("expected Import"),
        }
    }
}
