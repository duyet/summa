use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::UpdateChannel;

const DEFAULT_REPO: &str = "duyet/summa";
const GH_API: &str = "https://api.github.com";
const USER_AGENT: &str = "summa-update";
/// Workflows that upload `summa-<target>` artifacts. Master CI (`ci.yml`)
/// publishes linux-amd64 on every push; `release.yml` publishes all OS/arch.
pub const UPDATE_WORKFLOWS: &[&str] = &["ci.yml", "release.yml"];
/// Rolling tag receiving every master-branch build (beta channel).
pub const BETA_TAG: &str = "beta";

#[derive(Parser, Debug, Clone)]
pub struct UpdateArgs {
    /// Print actions only; do not download or replace the binary
    #[arg(long)]
    pub dry_run: bool,
    /// Switch to and update from the beta channel (rolling master CI builds)
    #[arg(long, conflicts_with = "stable")]
    pub beta: bool,
    /// Switch to and update from the stable channel (tagged releases)
    #[arg(long)]
    pub stable: bool,
    /// GitHub owner/repo (default: duyet/summa)
    #[arg(long)]
    pub repo: Option<String>,
}

impl UpdateArgs {
    /// Explicit channel override from flags; flags also persist the choice.
    pub fn channel_override(&self) -> Option<UpdateChannel> {
        if self.beta {
            return Some(UpdateChannel::Beta);
        }
        if self.stable {
            return Some(UpdateChannel::Stable);
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallState {
    pub source: String,
    pub run_id: u64,
    pub head_sha: String,
    pub target: String,
    pub sha256: String,
    pub artifact_name: String,
    /// Channel the binary came from (beta/stable). Older state files lack it.
    #[serde(default)]
    pub channel: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub id: u64,
    pub head_sha: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactMeta {
    pub id: u64,
    pub name: String,
}

/// OS/arch triple used by the Release workflow asset names.
pub fn detect_target() -> anyhow::Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => bail!("unsupported OS for prebuilt summa: {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => bail!("unsupported architecture for prebuilt summa: {other}"),
    };
    Ok(format!("{arch}-{os}"))
}

pub fn artifact_name_for_target(target: &str) -> String {
    format!("summa-{target}")
}

pub fn should_install(current_sha256: Option<&str>, incoming_sha256: &str) -> bool {
    if incoming_sha256.is_empty() {
        return false;
    }
    match current_sha256 {
        None => true,
        Some(cur) => !cur.eq_ignore_ascii_case(incoming_sha256),
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// Path of the CI sidecar: `<archive>.tar.gz.sha256`.
pub fn sha256_sidecar_path(archive: &Path) -> PathBuf {
    let mut os = archive.as_os_str().to_os_string();
    os.push(".sha256");
    PathBuf::from(os)
}

pub fn sha256_sidecar_url(tarball_url: &str) -> String {
    format!("{tarball_url}.sha256")
}

/// First field of a GNU `shasum -a 256` sidecar (`<hex>  dist/<asset>.tar.gz`).
/// Exactly one non-empty record; extra lines (malformed or conflicting) fail closed.
pub fn parse_sha256_sidecar(text: &str) -> anyhow::Result<String> {
    let mut records = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = records
        .next()
        .ok_or_else(|| anyhow!("checksum file is empty"))?;
    if records.next().is_some() {
        bail!("malformed checksum file: expected exactly one checksum record");
    }
    let hex = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("malformed checksum file: expected 64 hex chars (GNU shasum -a 256), got {hex:?}");
    }
    Ok(hex)
}

pub fn verify_sha256(path: &Path, expected: &str) -> anyhow::Result<()> {
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(expected) {
        bail!(
            "checksum mismatch for {} (expected {}, got {}). Refusing to install.",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("archive"),
            expected.to_ascii_lowercase(),
            actual
        );
    }
    Ok(())
}

fn verify_tarball_sidecar(tar: &Path) -> anyhow::Result<()> {
    let sidecar = sha256_sidecar_path(tar);
    if !sidecar.is_file() {
        bail!(
            "checksum not found at {}. CI publishes a .sha256 sidecar next to every tarball.",
            sidecar.display()
        );
    }
    let text = fs::read_to_string(&sidecar)
        .with_context(|| format!("read {}", sidecar.display()))?;
    let expected = parse_sha256_sidecar(&text)?;
    verify_sha256(tar, &expected)?;
    println!("update: checksum ok ({expected})");
    Ok(())
}

/// Successful completed runs from a GitHub Actions workflow-runs JSON body.
pub fn parse_successful_runs(json: &str) -> anyhow::Result<Vec<WorkflowRun>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parse workflow runs json")?;
    let Some(runs) = v.get("workflow_runs").and_then(|r| r.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for run in runs {
        let conclusion = run.get("conclusion").and_then(|c| c.as_str()).unwrap_or("");
        let status = run.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if status != "completed" || conclusion != "success" {
            continue;
        }
        let id = run.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
        let head_sha = run
            .get("head_sha")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if id == 0 || head_sha.is_empty() {
            continue;
        }
        let created_at = run
            .get("created_at")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        out.push(WorkflowRun {
            id,
            head_sha,
            created_at,
        });
    }
    Ok(out)
}

/// First successful completed run (API lists newest first).
pub fn parse_latest_successful_run(json: &str) -> anyhow::Result<Option<WorkflowRun>> {
    Ok(parse_successful_runs(json)?.into_iter().next())
}

/// Newest successful run across one or more workflow-run listings.
pub fn pick_newest_run(runs: impl IntoIterator<Item = WorkflowRun>) -> Option<WorkflowRun> {
    runs.into_iter().max_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    })
}

/// Find a non-expired artifact whose name matches `want`.
pub fn parse_artifact_by_name(json: &str, want: &str) -> anyhow::Result<Option<ArtifactMeta>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parse artifacts json")?;
    let Some(arts) = v.get("artifacts").and_then(|a| a.as_array()) else {
        return Ok(None);
    };
    for a in arts {
        let name = a.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let expired = a.get("expired").and_then(|e| e.as_bool()).unwrap_or(false);
        if expired || name != want {
            continue;
        }
        let id = a.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
        if id == 0 {
            continue;
        }
        return Ok(Some(ArtifactMeta {
            id,
            name: name.to_string(),
        }));
    }
    Ok(None)
}

pub fn default_state_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("summa")
        .join("install-state.json")
}

pub fn default_install_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SUMMA_INSTALL_DIR") {
        return PathBuf::from(dir).join("summa");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin")
        .join("summa")
}

pub fn load_state(path: &Path) -> Option<InstallState> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_state(path: &Path, state: &InstallState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state)?;
    fs::write(path, text)?;
    Ok(())
}

pub fn github_token() -> Option<String> {
    for key in ["SUMMA_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let out = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Version reported by a summa binary (`summa --version` → "summa 0.1.2").
pub fn binary_version(path: &Path) -> Option<String> {
    let out = Command::new(path).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace()
        .nth(1)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub async fn run(args: UpdateArgs) -> anyhow::Result<()> {
    let repo = args
        .repo
        .clone()
        .unwrap_or_else(|| DEFAULT_REPO.to_string());
    let target = detect_target()?;
    let artifact_name = artifact_name_for_target(&target);
    let install_path = default_install_path();
    let state_path = default_state_path();
    let current_state = load_state(&state_path);
    let current_version = if install_path.is_file() {
        binary_version(&install_path)
    } else {
        None
    };
    let current_sha = if install_path.is_file() {
        sha256_file(&install_path).ok()
    } else {
        None
    };

    // Channel: --beta/--stable flag wins, then config, then beta. A flag also
    // persists the switch into `[update] channel` so later auto-updates follow.
    let channel = if let Some(ch) = args.channel_override() {
        let cfg_path = crate::config::Config::resolve_write_path(None);
        let prev = crate::config::Config::load(None)
            .ok()
            .map(|c| c.update_channel())
            .unwrap_or_default();
        if prev != ch {
            crate::config::Config::set_value(&cfg_path, "update.channel", ch.as_str())?;
            println!("update: channel set to {} ({})", ch.as_str(), cfg_path.display());
        }
        ch
    } else {
        crate::config::Config::load(None)
            .map(|c| c.update_channel())
            .unwrap_or_default()
    };

    println!(
        "update: target={target} channel={} install={} current={}",
        channel.as_str(),
        install_path.display(),
        current_version.as_deref().unwrap_or("not installed"),
    );

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()?;
    let token = github_token();

    // Resolve once: either the stable release asset or the newest beta CI artifact.
    enum PendingUpdate {
        Stable {
            rel: StableRelease,
        },
        /// Rolling `beta` tag (public release assets) or CI artifact fallback.
        Beta {
            tag: Option<String>,
            tarball_url: Option<String>,
            artifact_id: u64,
        },
    }
    let (run_id, head_sha, source, pending) = match channel {
        UpdateChannel::Stable => {
            let rel = resolve_stable_release(&client, token.as_deref(), &repo, &artifact_name)
                .await?;
            (
                rel.tag.clone(),
                rel.commitish.clone().unwrap_or_default(),
                format!("release:{}", rel.tag),
                PendingUpdate::Stable { rel },
            )
        }
        UpdateChannel::Beta => match resolve_beta_release(&client, token.as_deref(), &repo, &artifact_name).await? {
            Some(rel) => (
                rel.tag.clone(),
                rel.commitish.clone().unwrap_or_default(),
                format!("beta:{}", rel.tag),
                PendingUpdate::Beta {
                    tag: Some(rel.tag),
                    tarball_url: Some(rel.tarball_url),
                    artifact_id: 0,
                },
            ),
            None => {
                let (run, artifact) =
                    resolve_ci_artifact(&client, token.as_deref(), &repo, &artifact_name).await?;
                (
                    run.id.to_string(),
                    run.head_sha.clone(),
                    format!("ci:{}", run.id),
                    PendingUpdate::Beta {
                        tag: None,
                        tarball_url: None,
                        artifact_id: artifact.id,
                    },
                )
            }
        },
    };

    if let Some(st) = &current_state {
        if st.run_id.to_string() == run_id && st.artifact_name == artifact_name {
            if let Some(cur) = &current_sha {
                if !should_install(Some(cur), &st.sha256) {
                    println!(
                        "update: already current ({}, {} sha {})",
                        current_version.as_deref().unwrap_or("unknown version"),
                        source,
                        &st.sha256[..12.min(st.sha256.len())]
                    );
                    return Ok(());
                }
            }
        }
    }

    if args.dry_run {
        println!(
            "dry-run: would install {artifact_name} from {source} ({})",
            head_sha
        );
        return Ok(());
    }

    let tmp = tempfile::tempdir()?;
    let bin = match pending {
        PendingUpdate::Stable { rel } => {
            let tarball =
                download_and_verify_release_tarball(&client, &rel.tarball_url, tmp.path()).await?;
            extract_summa_from_tarball(&tarball, tmp.path())?
        }
        PendingUpdate::Beta {
            tag,
            tarball_url,
            artifact_id,
        } => {
            if let (Some(_tag), Some(url)) = (&tag, &tarball_url) {
                let tarball =
                    download_and_verify_release_tarball(&client, url, tmp.path()).await?;
                extract_summa_from_tarball(&tarball, tmp.path())?
            } else {
                let token = token.ok_or_else(|| {
                    anyhow!("GitHub token required to download Actions artifacts (SUMMA_GITHUB_TOKEN / GH_TOKEN / gh auth)")
                })?;
                let zip_path = tmp.path().join("artifact.zip");
                download_artifact_zip(&client, &token, &repo, artifact_id, &zip_path).await?;
                extract_summa_from_artifact_zip(&zip_path, tmp.path())?
            }
        }
    };
    let incoming_sha = sha256_file(&bin)?;
    // Real version of the downloaded binary; falls back to the resolved tag/sha.
    let incoming_version = binary_version(&bin)
        .unwrap_or_else(|| {
            if run_id.starts_with('v') {
                run_id.clone()
            } else {
                format!("sha-{}", &head_sha[..12.min(head_sha.len())])
            }
        });

    if !should_install(current_sha.as_deref(), &incoming_sha) {
        let state = InstallState {
            source: source.clone(),
            run_id: 0,
            head_sha,
            target,
            sha256: incoming_sha,
            artifact_name,
            channel: Some(channel.as_str().to_string()),
        };
        save_state(&state_path, &state)?;
        println!(
            "update: already current ({}, {} sha {})",
            incoming_version,
            source,
            &state.sha256[..12.min(state.sha256.len())]
        );
        return Ok(());
    }

    install_binary(&bin, &install_path)?;
    sync_repo_release_copy(&install_path)?;

    let state = InstallState {
        source,
        run_id: run_id.parse().unwrap_or(0),
        head_sha,
        target,
        sha256: incoming_sha.clone(),
        artifact_name,
        channel: Some(channel.as_str().to_string()),
    };
    save_state(&state_path, &state)?;
    println!(
        "update: installed {} ({} channel {} sha {})",
        install_path.display(),
        incoming_version,
        channel.as_str(),
        &incoming_sha[..12]
    );
    Ok(())
}

/// Tagged GitHub Release asset for the stable channel.
#[derive(Debug, Clone)]
pub struct StableRelease {
    pub tag: String,
    pub tarball_url: String,
    pub commitish: Option<String>,
}

/// Newest non-prerelease release whose assets include `asset_name`.
pub fn parse_latest_stable_release(json: &str, asset_name: &str) -> anyhow::Result<Option<StableRelease>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parse releases json")?;
    let Some(releases) = v.as_array() else {
        return Ok(None);
    };
    for rel in releases {
        if let Some(r) = parse_release_value(rel, asset_name, false)? {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

/// Extract tag + asset tarball URL from one GitHub release object.
/// Skips drafts always; prereleases only when `allow_prerelease` is false.
pub fn parse_release_value(
    rel: &serde_json::Value,
    asset_name: &str,
    allow_prerelease: bool,
) -> anyhow::Result<Option<StableRelease>> {
    let draft = rel.get("draft").and_then(|d| d.as_bool()).unwrap_or(false);
    let prerelease = rel
        .get("prerelease")
        .and_then(|p| p.as_bool())
        .unwrap_or(false);
    if draft || (prerelease && !allow_prerelease) {
        return Ok(None);
    }
    let tag = rel.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
    let want_tarball = format!("{asset_name}.tar.gz");
    let tarball_url = rel
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| {
                    let name = a.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    name == asset_name || name == want_tarball
                })
                .and_then(|a| a.get("browser_download_url").and_then(|u| u.as_str()))
        })
        .unwrap_or("")
        .to_string();
    if tag.is_empty() || tarball_url.is_empty() {
        return Ok(None);
    }
    Ok(Some(StableRelease {
        tag: tag.to_string(),
        tarball_url,
        commitish: rel
            .get("target_commitish")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
    }))
}

async fn resolve_stable_release(
    client: &reqwest::Client,
    token: Option<&str>,
    repo: &str,
    asset_name: &str,
) -> anyhow::Result<StableRelease> {
    let url = format!("{GH_API}/repos/{repo}/releases?per_page=20");
    let body = gh_get(client, token, &url).await?;
    parse_latest_stable_release(&body, asset_name)?
        .ok_or_else(|| anyhow!("no stable release with asset {asset_name}"))
}

/// Rolling `beta` tag release (prerelease allowed) for the beta channel.
/// `Ok(None)` when the release or its asset doesn't exist yet — callers fall
/// back to CI artifacts.
pub async fn resolve_beta_release(
    client: &reqwest::Client,
    token: Option<&str>,
    repo: &str,
    asset_name: &str,
) -> anyhow::Result<Option<StableRelease>> {
    let url = format!("{GH_API}/repos/{repo}/releases/tags/{BETA_TAG}");
    match gh_get(client, token, &url).await {
        Ok(body) => {
            let v: serde_json::Value = serde_json::from_str(&body)?;
            parse_release_value(&v, asset_name, true)
        }
        Err(_) => Ok(None),
    }
}

async fn download_release_tarball(
    client: &reqwest::Client,
    url: &str,
    dest_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let path = dest_dir.join("release.tar.gz");
    fs::write(&path, &bytes)?;
    Ok(path)
}

async fn download_and_verify_release_tarball(
    client: &reqwest::Client,
    tarball_url: &str,
    dest_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let tarball = download_release_tarball(client, tarball_url, dest_dir).await?;
    let sidecar_url = sha256_sidecar_url(tarball_url);
    let sidecar_path = sha256_sidecar_path(&tarball);
    let resp = client.get(&sidecar_url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "checksum not found at {sidecar_url} (HTTP {status}). CI publishes a .sha256 sidecar next to every tarball."
        );
    }
    let text = resp.text().await?;
    fs::write(&sidecar_path, &text)?;
    let expected = parse_sha256_sidecar(&text)?;
    verify_sha256(&tarball, &expected)?;
    println!("update: checksum ok ({expected})");
    Ok(tarball)
}

/// Throttle file: records the last auto-update check as unix millis.
pub fn auto_update_marker_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("summa")
        .join("last-auto-update")
}

pub const AUTO_UPDATE_MIN_INTERVAL_MS: u128 = 60 * 60 * 1000;

pub fn should_auto_update_now(marker: &Path, now_ms: u128) -> bool {
    match fs::read_to_string(marker) {
        Ok(text) => text
            .trim()
            .parse::<u128>()
            .map(|last| now_ms.saturating_sub(last) >= AUTO_UPDATE_MIN_INTERVAL_MS)
            .unwrap_or(true),
        Err(_) => true,
    }
}

pub fn stamp_auto_update_marker(marker: &Path, now_ms: u128) -> anyhow::Result<()> {
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(marker, now_ms.to_string())?;
    Ok(())
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Fire-and-forget auto-update used when `[update] mode = "auto"`.
/// Downloads in this process's background task; the new binary is
/// active the next time `summa` starts. Never fails the caller.
pub async fn auto_update_tick() {
    let cfg = crate::config::Config::load(None).ok();
    let Some(cfg) = cfg else {
        return;
    };
    if cfg.update_mode() != crate::config::UpdateMode::Auto {
        return;
    }
    let marker = auto_update_marker_path();
    if !should_auto_update_now(&marker, now_ms()) {
        return;
    }
    if stamp_auto_update_marker(&marker, now_ms()).is_err() {
        return;
    }
    let args = UpdateArgs {
        dry_run: false,
        beta: false,
        stable: false,
        repo: None,
    };
    println!("update: checking for a newer build (auto)…");
    let res = tokio::spawn(async move { run(args).await }).await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("update: auto-update skipped ({e})"),
        Err(_) => {}
    }
}

async fn resolve_ci_artifact(
    client: &reqwest::Client,
    token: Option<&str>,
    repo: &str,
    artifact_name: &str,
) -> anyhow::Result<(WorkflowRun, ArtifactMeta)> {
    let mut candidates: Vec<(WorkflowRun, ArtifactMeta)> = Vec::new();
    for workflow in UPDATE_WORKFLOWS {
        let runs_url = format!(
            "{GH_API}/repos/{repo}/actions/workflows/{workflow}/runs?status=completed&per_page=10"
        );
        let runs_body = match gh_get(client, token, &runs_url).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("update: skip {workflow}: {e}");
                continue;
            }
        };
        for run in parse_successful_runs(&runs_body)? {
            let arts_url = format!("{GH_API}/repos/{repo}/actions/runs/{}/artifacts", run.id);
            let arts_body = match gh_get(client, token, &arts_url).await {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Some(artifact) = parse_artifact_by_name(&arts_body, artifact_name)? {
                candidates.push((run, artifact));
                break;
            }
        }
    }
    let (run, artifact) = candidates
        .into_iter()
        .max_by(|(a, _), (b, _)| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        })
        .ok_or_else(|| anyhow!("no successful CI/Release artifact named {artifact_name}"))?;
    Ok((run, artifact))
}

async fn gh_get(
    client: &reqwest::Client,
    token: Option<&str>,
    url: &str,
) -> anyhow::Result<String> {
    let mut req = client
        .get(url)
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("GET {url} -> {status}: {}", text.chars().take(200).collect::<String>());
    }
    Ok(text)
}

async fn download_artifact_zip(
    client: &reqwest::Client,
    token: &str,
    repo: &str,
    artifact_id: u64,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = format!("{GH_API}/repos/{repo}/actions/artifacts/{artifact_id}/zip");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?;
    fs::write(dest, &bytes)?;
    Ok(())
}

fn extract_summa_from_artifact_zip(zip_path: &Path, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    let unzip_status = Command::new("unzip")
        .args(["-o", "-q"])
        .arg(zip_path)
        .arg("-d")
        .arg(dest_dir)
        .status()
        .context("unzip")?;
    if !unzip_status.success() {
        bail!("unzip failed");
    }
    let tar = find_named(dest_dir, ".tar.gz")
        .ok_or_else(|| anyhow!("artifact zip did not contain a .tar.gz"))?;
    verify_tarball_sidecar(&tar)?;
    extract_summa_from_tarball(&tar, dest_dir)
}

fn extract_summa_from_tarball(tar: &Path, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    let extract_dir = dest_dir.join("unpacked");
    fs::create_dir_all(&extract_dir)?;
    let tar_status = Command::new("tar")
        .args(["-xzf"])
        .arg(tar)
        .arg("-C")
        .arg(&extract_dir)
        .status()
        .context("tar")?;
    if !tar_status.success() {
        bail!("tar extract failed");
    }
    find_named(&extract_dir, "summa")
        .or_else(|| find_file_named(&extract_dir, "summa"))
        .ok_or_else(|| anyhow!("archive did not contain summa"))
}

fn find_named(dir: &Path, suffix_or_name: &str) -> Option<PathBuf> {
    let mut found = None;
    if let Ok(walk) = fs::read_dir(dir) {
        for e in walk.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == suffix_or_name || name.ends_with(suffix_or_name) {
                found = Some(p);
                break;
            }
        }
    }
    found
}

fn find_file_named(dir: &Path, name: &str) -> Option<PathBuf> {
    fn rec(dir: &Path, name: &str) -> Option<PathBuf> {
        for e in fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(h) = rec(&p, name) {
                    return Some(h);
                }
            } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(p);
            }
        }
        None
    }
    rec(dir, name)
}

fn install_binary(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("summa.new");
    fs::copy(src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms)?;
    }
    fs::rename(&tmp, dest).or_else(|_| {
        fs::remove_file(dest).ok();
        fs::rename(&tmp, dest)
    })?;
    Ok(())
}

fn sync_repo_release_copy(installed: &Path) -> anyhow::Result<()> {
    let cwd_copy = PathBuf::from("target/release/summa");
    if cwd_copy.exists() || Path::new("Cargo.toml").exists() && Path::new("target/release").is_dir()
    {
        if let Some(parent) = cwd_copy.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = install_binary(installed, &cwd_copy);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_target_matches_release_asset_shape() {
        let t = detect_target().expect("host should be a supported target");
        assert!(
            t.ends_with("-apple-darwin") || t.ends_with("-unknown-linux-gnu"),
            "unexpected target {t}"
        );
        assert!(t.starts_with("aarch64-") || t.starts_with("x86_64-"), "{t}");
        assert_eq!(artifact_name_for_target(&t), format!("summa-{t}"));
    }

    #[test]
    fn should_install_empty_incoming_is_false() {
        assert!(!should_install(None, ""));
        assert!(!should_install(Some("abc"), ""));
    }

    #[test]
    fn should_install_missing_current() {
        assert!(should_install(None, "abc"));
    }

    #[test]
    fn should_install_same_hash_is_noop() {
        assert!(!should_install(Some("AaBb"), "aabb"));
    }

    #[test]
    fn should_install_different_hash() {
        assert!(should_install(Some("aaaa"), "bbbb"));
    }

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"summa"),
            "b44233a2f7cd626f6e8ddce9939e127388251740504e4af932fb75066509fa44"
        );
    }

    #[test]
    fn parse_sha256_sidecar_gnu_shasum() {
        let text = "3f97eacb7335bd7e91de18f5703050da387d88e12733b8c4f893105adc34489c  dist/summa-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            parse_sha256_sidecar(text).unwrap(),
            "3f97eacb7335bd7e91de18f5703050da387d88e12733b8c4f893105adc34489c"
        );
        let crlf =
            "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899  file.tar.gz\r\n";
        assert_eq!(
            parse_sha256_sidecar(crlf).unwrap(),
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"
        );
        assert!(parse_sha256_sidecar("").is_err());
        assert!(parse_sha256_sidecar("not-a-hash  file.tar.gz\n").is_err());
        assert!(parse_sha256_sidecar("abc  file\n").is_err());
        let extra = format!("{text}deadbeef  other.tar.gz\n");
        assert!(parse_sha256_sidecar(&extra).is_err());
        assert_eq!(
            parse_sha256_sidecar(&format!("{text}\n\n")).unwrap(),
            "3f97eacb7335bd7e91de18f5703050da387d88e12733b8c4f893105adc34489c"
        );
    }

    #[test]
    fn verify_sha256_match_and_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let tar = tmp.path().join("summa-x86_64-unknown-linux-gnu.tar.gz");
        std::fs::write(&tar, b"not-a-real-tar").unwrap();
        let digest = sha256_file(&tar).unwrap();
        let sidecar = format!("{digest}  dist/summa-x86_64-unknown-linux-gnu.tar.gz\n");
        let expected = parse_sha256_sidecar(&sidecar).unwrap();
        verify_sha256(&tar, &expected).unwrap();
        let err = verify_sha256(&tar, &"0".repeat(64)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("checksum mismatch"), "{msg}");
        assert_eq!(
            sha256_sidecar_path(&tar),
            tar.with_file_name("summa-x86_64-unknown-linux-gnu.tar.gz.sha256")
        );
        assert_eq!(
            sha256_sidecar_url("https://example/summa-x.tar.gz"),
            "https://example/summa-x.tar.gz.sha256"
        );
    }

    #[test]
    fn parse_latest_successful_run_skips_failures() {
        let json = r#"{
          "workflow_runs": [
            {"id": 1, "head_sha": "aaa", "status": "completed", "conclusion": "failure"},
            {"id": 2, "head_sha": "bbb", "status": "in_progress", "conclusion": null},
            {"id": 99, "head_sha": "ccc111", "status": "completed", "conclusion": "success"}
          ]
        }"#;
        let run = parse_latest_successful_run(json).unwrap().unwrap();
        assert_eq!(run.id, 99);
        assert_eq!(run.head_sha, "ccc111");
    }

    #[test]
    fn update_workflows_include_master_ci_and_release() {
        // curl|bash uses GitHub Release tarballs; `summa update` stays on Actions.
        assert_eq!(UPDATE_WORKFLOWS, &["ci.yml", "release.yml"]);
    }

    #[test]
    fn pick_newest_run_prefers_later_ci_over_older_release() {
        let ci = r#"{
          "workflow_runs": [
            {"id": 200, "head_sha": "newci", "status": "completed", "conclusion": "success",
             "created_at": "2026-08-17T07:00:00Z"}
          ]
        }"#;
        let release = r#"{
          "workflow_runs": [
            {"id": 100, "head_sha": "oldrel", "status": "completed", "conclusion": "success",
             "created_at": "2026-08-17T06:00:00Z"}
          ]
        }"#;
        let mut runs = parse_successful_runs(ci).unwrap();
        runs.extend(parse_successful_runs(release).unwrap());
        let picked = pick_newest_run(runs).unwrap();
        assert_eq!(picked.id, 200);
        assert_eq!(picked.head_sha, "newci");
    }

    #[test]
    fn parse_latest_successful_run_empty() {
        assert!(parse_latest_successful_run(r#"{"workflow_runs":[]}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn parse_artifact_skips_expired_and_wrong_name() {
        let json = r#"{
          "artifacts": [
            {"id": 1, "name": "summa-x86_64-unknown-linux-gnu", "expired": false},
            {"id": 2, "name": "summa-aarch64-apple-darwin", "expired": true},
            {"id": 3, "name": "summa-aarch64-apple-darwin", "expired": false}
          ]
        }"#;
        let a = parse_artifact_by_name(json, "summa-aarch64-apple-darwin")
            .unwrap()
            .unwrap();
        assert_eq!(a.id, 3);
        assert!(parse_artifact_by_name(json, "summa-nope").unwrap().is_none());
    }

    #[test]
    fn state_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("install-state.json");
        let st = InstallState {
            source: "ci".into(),
            run_id: 42,
            head_sha: "deadbeef".into(),
            target: "aarch64-apple-darwin".into(),
            sha256: "abc".into(),
            artifact_name: "summa-aarch64-apple-darwin".into(),
            channel: Some("beta".into()),
        };
        save_state(&path, &st).unwrap();
        assert_eq!(load_state(&path).unwrap(), st);
    }

    #[test]
    fn legacy_state_without_channel_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("install-state.json");
        std::fs::write(
            &path,
            r#"{"source":"ci","run_id":1,"head_sha":"a","target":"t","sha256":"s","artifact_name":"summa-t"}"#,
        )
        .unwrap();
        let st = load_state(&path).unwrap();
        assert_eq!(st.channel, None);
    }

    #[test]
    fn parse_latest_stable_release_skips_prerelease_and_missing_asset() {
        let json = r#"[
            {"tag_name": "v0.2.0-beta", "prerelease": true, "draft": false,
             "assets": [{"name": "summa-x86_64-unknown-linux-gnu",
                         "browser_download_url": "https://x/beta.tar.gz"}]},
            {"tag_name": "v0.1.9", "prerelease": false, "draft": false,
             "assets": [{"name": "other", "browser_download_url": "https://x/other"}]},
            {"tag_name": "v0.1.8", "prerelease": false, "draft": true,
             "assets": []},
            {"tag_name": "v0.1.7", "prerelease": false, "draft": false,
             "assets": [{"name": "summa-x86_64-unknown-linux-gnu",
                         "browser_download_url": "https://x/v017.tar.gz"},
                        {"name": "summa-aarch64-apple-darwin",
                         "browser_download_url": "https://x/mac.tar.gz"}]}
          ]"#;
        let rel = parse_latest_stable_release(json, "summa-x86_64-unknown-linux-gnu")
            .unwrap()
            .unwrap();
        assert_eq!(rel.tag, "v0.1.7");
        assert_eq!(rel.tarball_url, "https://x/v017.tar.gz");
        assert!(parse_latest_stable_release("[]", "nope").unwrap().is_none());
        assert!(parse_latest_stable_release("{}", "nope").unwrap().is_none());
    }

    #[test]
    fn update_args_channel_override() {
        use clap::Parser as _;
        let a = UpdateArgs::parse_from(["update", "--beta"]);
        assert_eq!(a.channel_override(), Some(UpdateChannel::Beta));
        let b = UpdateArgs::parse_from(["update", "--stable"]);
        assert_eq!(b.channel_override(), Some(UpdateChannel::Stable));
        let c = UpdateArgs::parse_from(["update"]);
        assert_eq!(c.channel_override(), None);
        assert!(UpdateArgs::try_parse_from(["update", "--beta", "--stable"]).is_err());
    }

    #[test]
    fn binary_version_parses_summa_output() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("summa");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&fake, "#!/bin/sh\necho \"summa 9.9.9\"\n").unwrap();
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
            assert_eq!(
                binary_version(&fake).as_deref(),
                Some("9.9.9"),
                "version parsed from --version output"
            );
        }
        // Missing/invalid binary → None.
        assert_eq!(binary_version(&tmp.path().join("nope")), None);
    }

    #[test]
    fn parse_release_value_accepts_tarball_and_artifact_names() {
        let rel = serde_json::json!({
            "tag_name": "v0.1.2",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": "summa-aarch64-apple-darwin.tar.gz",
                 "browser_download_url": "https://x/a.tar.gz"}
            ]
        });
        let got = super::parse_release_value(&rel, "summa-aarch64-apple-darwin", false)
            .unwrap()
            .unwrap();
        assert_eq!(got.tag, "v0.1.2");
        assert_eq!(got.tarball_url, "https://x/a.tar.gz");
        // Bare artifact-style names still match (release uploads may vary).
        let rel2 = serde_json::json!({
            "tag_name": "v9",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": "summa-aarch64-apple-darwin",
                 "browser_download_url": "https://x/b"}
            ]
        });
        assert!(super::parse_release_value(&rel2, "summa-aarch64-apple-darwin", false)
            .unwrap()
            .is_some());
    }

    #[test]
    fn auto_update_marker_throttles_within_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("last-auto-update");
        assert!(should_auto_update_now(&marker, 1_000_000));
        stamp_auto_update_marker(&marker, 1_000_000).unwrap();
        // Just under the interval: no update.
        assert!(!should_auto_update_now(
            &marker,
            1_000_000 + AUTO_UPDATE_MIN_INTERVAL_MS - 1
        ));
        // At/after the interval: update.
        assert!(should_auto_update_now(
            &marker,
            1_000_000 + AUTO_UPDATE_MIN_INTERVAL_MS
        ));
        // Garbage contents behave like "never checked".
        std::fs::write(&marker, "not-a-number").unwrap();
        assert!(should_auto_update_now(&marker, 0));
    }
}

