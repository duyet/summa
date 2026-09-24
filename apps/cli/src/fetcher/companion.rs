/**
 * Companion fetcher — spawns `ccusage@latest <source> <command> --breakdown --json`
 * subprocesses and normalizes the JSON into `CompanionUsageRow` structs.
 *
 * The TS equivalent is `src/fetchers/companion.ts`. We reuse the normalization
 * logic already ported to `parser::companion`.
 */

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout as tokio_timeout;

use crate::parser::companion::{normalize_companion_rows, normalize_usage_row, CompanionCommand, CompanionData, CompanionUsageRow};

// ---------------------------------------------------------------------------
// Source enum — mirrors TS `CompanionSource`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionSource {
    Codex,
    OpenCode,
    Gemini,
    OpenClaw,
    Amp,
    Droid,
    Codebuff,
    Pi,
    Goose,
    Kilo,
    Copilot,
    Kimi,
    Qwen,
}

impl CompanionSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompanionSource::Codex => "codex",
            CompanionSource::OpenCode => "opencode",
            CompanionSource::Gemini => "gemini",
            CompanionSource::OpenClaw => "openclaw",
            CompanionSource::Amp => "amp",
            CompanionSource::Droid => "droid",
            CompanionSource::Codebuff => "codebuff",
            CompanionSource::Pi => "pi",
            CompanionSource::Goose => "goose",
            CompanionSource::Kilo => "kilo",
            CompanionSource::Copilot => "copilot",
            CompanionSource::Kimi => "kimi",
            CompanionSource::Qwen => "qwen",
        }
    }

    /// Env var that points this source at a custom data dir, when supported.
    pub fn path_env(&self) -> Option<&'static str> {
        match self {
            CompanionSource::Codex => Some("CODEX_HOME"),
            CompanionSource::OpenCode => Some("OPENCODE_DATA_DIR"),
            CompanionSource::Gemini => Some("GEMINI_DATA_DIR"),
            CompanionSource::OpenClaw => Some("OPENCLAW_DIR"),
            _ => None,
        }
    }
}

impl std::fmt::Display for CompanionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for CompanionSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "codex" => Ok(CompanionSource::Codex),
            "opencode" => Ok(CompanionSource::OpenCode),
            "gemini" => Ok(CompanionSource::Gemini),
            "openclaw" => Ok(CompanionSource::OpenClaw),
            "amp" => Ok(CompanionSource::Amp),
            "droid" => Ok(CompanionSource::Droid),
            "codebuff" => Ok(CompanionSource::Codebuff),
            "pi" => Ok(CompanionSource::Pi),
            "goose" => Ok(CompanionSource::Goose),
            "kilo" => Ok(CompanionSource::Kilo),
            "copilot" => Ok(CompanionSource::Copilot),
            "kimi" => Ok(CompanionSource::Kimi),
            "qwen" => Ok(CompanionSource::Qwen),
            _ => Err(format!("unknown companion source: {}", s)),
        }
    }
}

// ---------------------------------------------------------------------------
// Fetch options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CompanionFetchOptions {
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub verbose: Option<bool>,
    pub data_path: Option<String>,
    pub since: Option<String>,
    pub end_date: Option<String>,
}

// ---------------------------------------------------------------------------
// Main fetch entrypoint
// ---------------------------------------------------------------------------

/// Fetch daily + session rows from a companion source.
///
/// Monthly is not fetched — derivable from daily via `toYYYYMM(date)` in SQL.
pub async fn fetch_all_companion_data(
    source: CompanionSource,
    opts: CompanionFetchOptions,
) -> anyhow::Result<CompanionData> {
    let timeout_dur = Duration::from_millis(opts.timeout_ms.unwrap_or(120_000));
    let max_retries = opts.max_retries.unwrap_or(2);
    let verbose = opts.verbose.unwrap_or(false);
    let since = opts.since.clone();
    let end_date = opts.end_date.clone();

    let date_flags = build_date_flags(&since, &end_date);

    let runner = detect_runner().await;

    // Build per-source env (e.g. CODEX_HOME=/path/to/.codex)
    let mut env = HashMap::new();
    if let Some(path) = &opts.data_path {
        if let Some(path_env) = source.path_env() {
            env.insert(path_env.to_string(), path.clone());
        }
    }

    let daily = fetch_command(source, CompanionCommand::Daily, &runner, &env, &date_flags, timeout_dur, max_retries, verbose).await;
    let session = fetch_command(source, CompanionCommand::Session, &runner, &env, &date_flags, timeout_dur, max_retries, verbose).await;

    Ok(CompanionData {
        daily,
        monthly: vec![], // derived from daily in SQL
        session,
    })
}

// ---------------------------------------------------------------------------
// Subprocess runner
// ---------------------------------------------------------------------------

async fn fetch_command(
    source: CompanionSource,
    command: CompanionCommand,
    runner: &str,
    env: &HashMap<String, String>,
    date_flags: &str,
    timeout_dur: Duration,
    max_retries: u32,
    verbose: bool,
) -> Vec<CompanionUsageRow> {
    let mut last_err = None;

    for attempt in 0..max_retries {
        match run_once(source, command, runner, env, date_flags, timeout_dur).await {
            Ok(raw) => {
                let raw_rows = normalize_companion_rows(command, &raw);
                if !raw_rows.is_empty() {
                    return raw_rows
                        .into_iter()
                        .map(|v| normalize_usage_row(command, &v))
                        .collect();
                }
                last_err = Some("empty normalized rows".to_string());
                if attempt < max_retries - 1 {
                    sleep_backoff(attempt).await;
                }
            }
            Err(e) => {
                last_err = Some(e.to_string());
                if attempt < max_retries - 1 {
                    sleep_backoff(attempt).await;
                }
            }
        }
    }

    if verbose {
        if let Some(ref e) = last_err {
            eprintln!("{} {} failed: {}", source, command.as_str(), e);
        }
    }

    vec![]
}

async fn run_once(
    source: CompanionSource,
    command: CompanionCommand,
    runner: &str,
    env: &HashMap<String, String>,
    date_flags: &str,
    timeout_dur: Duration,
) -> anyhow::Result<serde_json::Value> {
    let mut args: Vec<&str> = vec![runner, "-y", "ccusage@latest"];
    args.push(source.as_str());
    args.push(command.as_str());
    args.push("--breakdown");
    args.push("--json");
    if !date_flags.is_empty() {
        args.extend(date_flags.split(' ').filter(|s| !s.is_empty()));
    }

    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Merge env: current process env + source-specific overrides
    for (key, value) in env {
        cmd.env(key, value);
    }

    let output = tokio_timeout(timeout_dur, cmd.output()).await??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "{} {} exited with {}: {}",
            source.as_str(),
            command.as_str(),
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_str = extract_json(&stdout);
    if json_str.is_empty() || json_str == "null" {
        anyhow::bail!("no JSON in {} {} output", source.as_str(), command.as_str());
    }

    serde_json::from_str(json_str).map_err(|e| anyhow::anyhow!("parse JSON: {}", e))
}

/// Strip log lines before the first JSON object/array.
///
/// Companion packages may print log lines to stdout before JSON
/// (e.g. `[@ccusage/opencode] ℹ ...`). We skip those by validating
/// the candidate line parses as JSON.
fn extract_json(stdout: &str) -> &str {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            // Validate it's actually JSON, not a log prefix like `[@ccusage/...]`.
            if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
                return trimmed;
            }
        }
    }
    stdout
}

fn build_date_flags(since: &Option<String>, end_date: &Option<String>) -> String {
    let mut parts = Vec::new();
    if let Some(ref s) = since {
        parts.push(format!("--since={}", s));
    }
    if let Some(ref e) = end_date {
        parts.push(format!("--until={}", e));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

async fn detect_runner() -> String {
    if command_exists("npx").await {
        "npx".to_string()
    } else if command_exists("bunx").await {
        "bunx".to_string()
    } else {
        "npx".to_string()
    }
}

async fn command_exists(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn sleep_backoff(attempt: u32) {
    let ms = 2u64.pow(attempt) * 1000;
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_source_as_str() {
        assert_eq!(CompanionSource::Codex.as_str(), "codex");
        assert_eq!(CompanionSource::OpenCode.as_str(), "opencode");
        assert_eq!(CompanionSource::Qwen.as_str(), "qwen");
    }

    #[test]
    fn companion_source_from_str() {
        assert!(matches!("codex".parse::<CompanionSource>(), Ok(CompanionSource::Codex)));
        assert!("unknown".parse::<CompanionSource>().is_err());
    }

    #[test]
    fn companion_source_display() {
        assert_eq!(CompanionSource::Codex.to_string(), "codex");
    }

    #[test]
    fn companion_source_path_env() {
        assert_eq!(CompanionSource::Codex.path_env(), Some("CODEX_HOME"));
        assert_eq!(CompanionSource::Amp.path_env(), None);
    }

    #[test]
    fn extract_json_skips_log_prefix() {
        let input = "[@ccusage/opencode] fetching workspace\n{\"daily\":[]}";
        assert_eq!(extract_json(input), "{\"daily\":[]}");
    }

    #[test]
    fn extract_json_no_prefix() {
        assert_eq!(extract_json("{\"daily\":[]}"), "{\"daily\":[]}");
    }

    #[test]
    fn extract_json_starts_with_bracket() {
        assert_eq!(extract_json("[{\"date\":\"2025-01-01\"}]"), "[{\"date\":\"2025-01-01\"}]");
    }

    #[test]
    fn build_date_flags_none() {
        assert_eq!(build_date_flags(&None, &None), "");
    }

    #[test]
    fn build_date_flags_since_only() {
        assert_eq!(build_date_flags(&Some("2025-01-01".into()), &None), " --since=2025-01-01");
    }

    #[test]
    fn build_date_flags_both() {
        assert_eq!(
            build_date_flags(&Some("2025-01-01".into()), &Some("2025-01-31".into())),
            " --since=2025-01-01 --until=2025-01-31"
        );
    }
}
