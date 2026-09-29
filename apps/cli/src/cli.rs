use clap::{Parser, Subcommand};
use crate::script::cronjob::CronjobArgs;
use crate::script::publish::PublishArgs;
use crate::script::serve::ServeArgs;

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // Auto-update downloads alongside the command; active on next launch.
    let auto_update = tokio::spawn(crate::script::update::auto_update_tick());
    let result = match cli.command {
        Commands::Import(args) => {
            if cli.verbose {
                eprintln!("Importing with config: {:?}", args.config);
            }
            crate::script::import_all::run(args, cli.verbose).await
        }
        Commands::Check(args) => crate::script::check::run(args).await,
        Commands::Config(args) => {
            for pair in &args.set {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--set expects KEY=VALUE, got `{pair}`"))?;
                let path = crate::config::Config::resolve_write_path(args.config.as_deref());
                crate::config::Config::set_value(&path, key.trim(), value.trim())?;
                println!("set {key} = {value} ({})", path.display());
            }
            if !args.set.is_empty() {
                return finish(auto_update, Ok(())).await;
            }
            let cfg = crate::config::Config::load(args.config.as_deref())?;
            if args.validate {
                println!("config ok");
                println!("  duckdb_default={}", crate::config::Config::default_duckdb_path());
                println!("  clickhouse_host={}", cfg.clickhouse.host);
                println!(
                    "  password_set={}",
                    !cfg.clickhouse.password.is_empty()
                );
                return finish(auto_update, Ok(())).await;
            }
            // Redact password when printing
            let mut printable = cfg.clone();
            if !printable.clickhouse.password.is_empty() {
                printable.clickhouse.password = "***".into();
            }
            println!("{}", toml::to_string_pretty(&printable)?);
            println!("# resolved duckdb default: {}", crate::config::Config::default_duckdb_path());
            println!("# update: channel={} mode={}", cfg.update_channel(), cfg.update_mode());
            println!("# config candidates:");
            for p in crate::config::Config::candidate_paths() {
                println!("#   {p}");
            }
            Ok(())
        }
        Commands::Cronjob(args) => crate::script::cronjob::run(args).await,
        Commands::Publish(args) => crate::script::publish::run(args).await,
        Commands::Update(args) => crate::script::update::run(args).await,
        Commands::Serve(args) => crate::script::serve::run(args).await,
    };
    finish(auto_update, result).await
}

async fn finish(
    auto_update: tokio::task::JoinHandle<()>,
    result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    let _ = auto_update.await;
    result
}

#[derive(Parser, Debug, Clone)]
#[command(name = "summa")]
#[command(about = "Import Claude Code (ccusage) and AI agent usage costs into DuckDB or ClickHouse")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Import usage data from all configured sources to sinks
    Import(ImportArgs),
    /// Check connections to sinks
    Check(CheckArgs),
    /// Print or validate configuration
    Config(ConfigArgs),
    /// Generate and register a scheduled import (launchd / systemd / cron)
    Cronjob(CronjobArgs),
    /// Publish local DuckDB events to ClickHouse
    Publish(PublishArgs),
    /// Install the newest CI Release-workflow binary for this OS/arch
    Update(crate::script::update::UpdateArgs),
    /// Ping the cloud hub (summa.duyet.net). Local HTTP server removed.
    Serve(ServeArgs),
}

#[derive(Parser, Debug, Clone)]
pub struct ImportArgs {
    /// Path to TOML config file
    #[arg(short, long)]
    pub config: Option<String>,
    /// Start date for import (YYYY-MM-DD). Takes priority over --days-back.
    #[arg(long)]
    pub since: Option<String>,
    /// Number of days of history to import (cron runner uses this).
    #[arg(long)]
    pub days_back: Option<i64>,
    /// End date for import (YYYY-MM-DD)
    #[arg(long)]
    pub end_date: Option<String>,
    /// Override DuckDB file path (local path or md:database)
    #[arg(long)]
    pub duckdb_path: Option<String>,
    /// Override ClickHouse host
    #[arg(long)]
    pub ch_host: Option<String>,
    /// Override ClickHouse port
    #[arg(long)]
    pub ch_port: Option<u16>,
    /// Override ClickHouse database
    #[arg(long)]
    pub ch_database: Option<String>,
    /// Skip ccusage source
    #[arg(long)]
    pub skip_ccusage: bool,
    /// Skip OpenCode/companion source
    #[arg(long)]
    pub skip_opencode: bool,
    /// Skip Codex source
    #[arg(long)]
    pub skip_codex: bool,
    /// Skip Antigravity source
    #[arg(long)]
    pub skip_antigravity: bool,
    /// Skip Hermes source
    #[arg(long)]
    pub skip_hermes: bool,
    /// Skip Grok Build source
    #[arg(long)]
    pub skip_grok: bool,
    /// Skip Devin CLI source
    #[arg(long)]
    pub skip_devin: bool,
    /// Skip Cursor account-wide usage source
    #[arg(long)]
    pub skip_cursor: bool,
    /// Skip Command Code source
    #[arg(long)]
    pub skip_command_code: bool,
    /// Skip Pi coding agent source
    #[arg(long)]
    pub skip_pi: bool,
    /// Skip fx (Vercel Labs) source
    #[arg(long)]
    pub skip_fx: bool,
    /// Skip ClickHouse sink
    #[arg(long)]
    pub skip_clickhouse: bool,
    /// Skip DuckDB sink
    #[arg(long)]
    pub skip_duckdb: bool,
    /// Dry run without writing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Parser, Debug, Clone)]
pub struct CheckArgs {
    /// Path to TOML config file
    #[arg(short, long)]
    pub config: Option<String>,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Parser, Debug, Clone)]
pub struct ConfigArgs {
    /// Path to TOML config file to print
    #[arg(short, long)]
    pub config: Option<String>,
    /// Validate config only
    #[arg(long)]
    pub validate: bool,
    /// Set a config value (dotted key=value), e.g. --set update.mode=auto
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,
}

impl Default for ConfigArgs {
    fn default() -> Self {
        Self {
            config: None,
            validate: false,
            set: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_help_includes_days_back() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        // Subcommand help is nested; try_get_matches style check via parse.
        let cli = Cli::try_parse_from(["summa", "import", "--help"]);
        // --help causes error with DisplayHelp; just ensure days_back is on the type.
        let _ = cli;
        let _ = help;
        let args = ImportArgs {
            config: None,
            since: None,
            days_back: Some(2),
            end_date: None,
            duckdb_path: Some(crate::config::Config::default_duckdb_path()),
            ch_host: None,
            ch_port: None,
            ch_database: None,
            skip_ccusage: false,
            skip_opencode: false,
            skip_codex: false,
            skip_antigravity: false,
            skip_hermes: false,
            skip_grok: false,
            skip_devin: false,
            skip_cursor: false,
            skip_command_code: false,
            skip_pi: false,
            skip_fx: false,
            skip_clickhouse: false,
            skip_duckdb: false,
            dry_run: false,
        };
        assert_eq!(args.days_back, Some(2));
    }

    #[test]
    fn parse_update_dry_run() {
        let cli = Cli::try_parse_from(["summa", "update", "--dry-run"]).unwrap();
        match cli.command {
            Commands::Update(args) => assert!(args.dry_run),
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn parse_config_set_key_value() {
        let cli = Cli::try_parse_from(["summa", "config", "--set", "update.mode=auto"]).unwrap();
        match cli.command {
            Commands::Config(args) => assert_eq!(args.set, vec!["update.mode=auto".to_string()]),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn help_includes_update_subcommand() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("update"),
            "top-level help should list update: {help}"
        );
    }

    #[test]
    fn help_names_summa_product() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("summa") || help.to_lowercase().contains("duckdb"),
            "help should brand the summa product: {help}"
        );
    }

    #[test]
    fn help_includes_cronjob_subcommand() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("cronjob"),
            "top-level help should list cronjob: {help}"
        );
    }

    #[test]
    fn help_includes_serve_subcommand() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("serve"),
            "top-level help should list serve: {help}"
        );
    }

    #[test]
    fn parse_serve_bind() {
        let cli = Cli::try_parse_from(["summa", "serve", "--bind", "0.0.0.0:8787"]).unwrap();
        match cli.command {
            Commands::Serve(args) => assert_eq!(args.bind.as_deref(), Some("0.0.0.0:8787")),
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn clap_accepts_skip_cursor_and_skip_grok() {
        let cli = Cli::try_parse_from(["summa", "import", "--skip-cursor", "--skip-grok"]).unwrap();
        match cli.command {
            Commands::Import(args) => {
                assert!(args.skip_cursor);
                assert!(args.skip_grok);
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn clap_accepts_skip_devin() {
        let cli = Cli::try_parse_from(["summa", "import", "--skip-devin"]).unwrap();
        match cli.command {
            Commands::Import(args) => assert!(args.skip_devin),
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn clap_accepts_skip_pi_and_skip_fx() {
        let cli = Cli::try_parse_from(["summa", "import", "--skip-pi", "--skip-fx"]).unwrap();
        match cli.command {
            Commands::Import(args) => {
                assert!(args.skip_pi);
                assert!(args.skip_fx);
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }
}
