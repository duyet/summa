/**
 * ccusage CLI fetcher — spawns `ccusage@latest` subprocesses, parses JSON,
 * and returns typed Rust structs consumed by `parser::rows`.
 */

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout as tokio_timeout;

use crate::parser::rows::{CcusageData, CcusageFetchOptions};
use crate::parser::types::{
    BlockUsage, CcusageProjectsResponse, DailyUsage, ProjectDailyUsage, SessionUsage,
};

/// Fetch all ccusage Claude data (daily, session, blocks, projects).
pub async fn fetch_all_ccusage_data(opts: CcusageFetchOptions) -> CcusageData {
    let timeout_dur = Duration::from_millis(opts.timeout.unwrap_or(120_000));
    let max_retries = opts.max_retries.unwrap_or(2);
    let verbose = opts.verbose.unwrap_or(false);
    let since = opts.since.clone();
    let end_date = opts.end_date.clone();

    let date_flags = build_date_flags(&since, &end_date);

    // Fetch sequentially to avoid spiking memory with concurrent npm processes.
    let daily = fetch_vec::<DailyWrapper, DailyUsage>(
        "claude daily",
        &date_flags,
        timeout_dur,
        max_retries,
        verbose,
        |w| w.daily,
    )
    .await;

    let session = fetch_vec::<SessionWrapper, SessionUsage>(
        "claude session",
        &date_flags,
        timeout_dur,
        max_retries,
        verbose,
        |w| w.sessions,
    )
    .await;

    let blocks = fetch_vec::<BlocksWrapper, BlockUsage>(
        "claude blocks",
        &date_flags,
        timeout_dur,
        max_retries,
        verbose,
        |w| w.blocks,
    )
    .await;

    let raw_projects: Option<CcusageProjectsResponse> = fetch_wrapper(
        "claude daily --instances",
        &date_flags,
        timeout_dur,
        max_retries,
        verbose,
    )
    .await;

    let projects: HashMap<String, Vec<ProjectDailyUsage>> = raw_projects
        .map(|p| p.projects)
        .unwrap_or_default();

    CcusageData {
        daily,
        session,
        blocks,
        projects,
    }
}

// ---------------------------------------------------------------------------
// JSON wrapper types — match the top-level response shapes from ccusage CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct DailyWrapper {
    #[serde(default)]
    daily: Vec<DailyUsage>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct SessionWrapper {
    #[serde(default)]
    sessions: Vec<SessionUsage>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct BlocksWrapper {
    #[serde(default)]
    blocks: Vec<BlockUsage>,
}

// ---------------------------------------------------------------------------
// Generic subprocess runner
// ---------------------------------------------------------------------------

/// Spawn `ccusage@latest <command> --json`, parse into `Wrapper`, and extract `Vec<Inner>`.
async fn fetch_vec<Wrapper, Inner>(
    command: &str,
    date_flags: &str,
    timeout_dur: Duration,
    max_retries: u32,
    verbose: bool,
    extract: fn(Wrapper) -> Vec<Inner>,
) -> Vec<Inner>
where
    Wrapper: serde::de::DeserializeOwned + Default,
    Inner: 'static,
{
    fetch_wrapper(command, date_flags, timeout_dur, max_retries, verbose)
        .await
        .map(extract)
        .unwrap_or_default()
}

/// Spawn `ccusage@latest <command> --json` and return parsed `Option<T>`.
async fn fetch_wrapper<T: serde::de::DeserializeOwned>(
    command: &str,
    date_flags: &str,
    timeout_dur: Duration,
    max_retries: u32,
    verbose: bool,
) -> Option<T> {
    let runner = detect_runner().await;
    let mut last_err = None;

    for attempt in 0..max_retries {
        match run_once(command, date_flags, &runner, timeout_dur).await {
            Ok(raw) => match serde_json::from_str::<T>(&raw) {
                Ok(parsed) => return Some(parsed),
                Err(e) => {
                    last_err = Some(format!("deserialize: {}", e));
                    if attempt < max_retries - 1 {
                        sleep_backoff(attempt).await;
                    }
                }
            },
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
            eprintln!("ccusage {} failed: {}", command, e);
        }
    }

    None
}

async fn run_once(command: &str, date_flags: &str, runner: &str, timeout_dur: Duration) -> anyhow::Result<String> {
    let mut args: Vec<&str> = vec![runner, "-y", "ccusage@latest"];
    args.extend(command.split(' ').filter(|s| !s.is_empty()));
    args.push("--json");
    if !date_flags.is_empty() {
        args.extend(date_flags.split(' ').filter(|s| !s.is_empty()));
    }

    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(std::env::vars_os());

    let output = tokio_timeout(timeout_dur, cmd.output()).await??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "ccusage {} exited with {}: {}",
            command,
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = extract_json(&stdout);
    if json.is_empty() || json == "null" {
        anyhow::bail!("no JSON in ccusage {} output", command);
    }

    Ok(json.to_string())
}

/// Strip log lines before the first JSON object/array.
///
/// ccusage may print `[@ccusage/...]` log lines that also start with `[`;
/// we skip those by validating the candidate parses as JSON.
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
    fn extract_json_skips_log_prefix() {
        // ccusage log prefixes use [@ccusage/...]; those lines start with '[' but
        // are not JSON, so we skip them and return the JSON line that follows.
        let input = "[@ccusage/claude] fetching workspace\n{\"daily\":[]}";
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

    #[test]
    fn daily_wrapper_deserializes() {
        let json = r#"{"daily":[{"date":"2025-01-05","inputTokens":100,"outputTokens":200,"cacheCreationInputTokens":10,"cacheReadInputTokens":20,"totalCost":0.05,"modelsUsed":["claude-3-5-sonnet"],"modelBreakdowns":[{"modelName":"claude-3-5-sonnet","input_tokens":100,"output_tokens":200,"cache_creation_tokens":10,"cache_read_tokens":20,"cost":0.05}]}]}"#;
        let wrapper: DailyWrapper = serde_json::from_str(json).unwrap();
        assert_eq!(wrapper.daily.len(), 1);
        assert_eq!(wrapper.daily[0].date, "2025-01-05");
    }

    #[test]
    fn session_wrapper_deserializes() {
        let json = r#"{"sessions":[{"sessionId":"s1","lastActivity":"2025-01-05","inputTokens":50,"outputTokens":100,"cacheCreationInputTokens":5,"cacheReadInputTokens":10,"totalCost":0.01,"modelsUsed":["claude-3-5-sonnet"],"modelBreakdowns":[]}]}"#;
        let wrapper: SessionWrapper = serde_json::from_str(json).unwrap();
        assert_eq!(wrapper.sessions.len(), 1);
        assert_eq!(wrapper.sessions[0].session_id, "s1");
    }

    #[test]
    fn blocks_wrapper_deserializes() {
        let json = r#"{"blocks":[{"id":"b1","startTime":"2025-01-05T10:00:00Z","endTime":"2025-01-05T11:00:00Z","tokenCounts":{"input_tokens":10,"output_tokens":20,"cache_creation_tokens":1,"cache_read_tokens":2},"totalTokens":33,"costUSD":0.001}]}"#;
        let wrapper: BlocksWrapper = serde_json::from_str(json).unwrap();
        assert_eq!(wrapper.blocks.len(), 1);
        assert_eq!(wrapper.blocks[0].id, "b1");
        assert_eq!(wrapper.blocks[0].total_tokens, 33);
    }

    #[test]
    fn projects_response_deserializes() {
        let json = r#"{"projects":{"/repo":[{"date":"2025-01-05","inputTokens":100,"outputTokens":200,"cacheCreationInputTokens":10,"cacheReadInputTokens":20,"totalCost":0.05,"modelsUsed":["claude-3-5-sonnet"],"modelBreakdowns":[]}]}}"#;
        let resp: CcusageProjectsResponse = serde_json::from_str(json).unwrap();
        assert!(resp.projects.contains_key("/repo"));
        let project_daily: Vec<ProjectDailyUsage> = serde_json::from_str(r#"[{"date":"2025-01-05","inputTokens":100,"outputTokens":200,"cacheCreationInputTokens":10,"cacheReadInputTokens":20,"totalCost":0.05,"modelsUsed":["claude-3-5-sonnet"],"modelBreakdowns":[]}]"#).unwrap();
        assert_eq!(project_daily[0].date, "2025-01-05");
    }

    #[test]
    fn fetch_all_ccusage_data_returns_struct() {
        // Structural test: `CcusageData` can be constructed and is empty by default.
        let data = CcusageData {
            daily: vec![],
            session: vec![],
            blocks: vec![],
            projects: HashMap::new(),
        };
        assert!(data.daily.is_empty());
        assert!(data.projects.is_empty());
    }
}
