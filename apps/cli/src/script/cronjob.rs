//! Generate and register a user-level import scheduler.
//!
//! Backends (auto-detected):
//! - macOS: launchd LaunchAgent
//! - Linux with systemd --user: systemd timer + oneshot service
//! - otherwise: crontab (stdin to `crontab -`)
//!
//! `summa cronjob status` also reports legacy `run-import.sh` crontab lines.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context};
use clap::{Parser, Subcommand};

pub const LAUNCHD_LABEL: &str = "net.duyet.summa.import";
pub const SYSTEMD_UNIT: &str = "summa-import";
pub const CRON_MARKER: &str = "# summa-import managed";
const INSTALLER_URL: &str = "https://summa.duyet.net/install.sh";

#[derive(Parser, Debug, Clone)]
pub struct CronjobArgs {
    #[command(subcommand)]
    pub action: CronjobAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum CronjobAction {
    /// Generate and register a scheduled `summa import` job
    Install(InstallArgs),
    /// Unregister the managed job (launchd / systemd / crontab)
    Remove {
        #[arg(long)]
        dry_run: bool,
    },
    /// Show whether a managed or legacy import job is installed
    Status,
}

#[derive(Parser, Debug, Clone)]
pub struct InstallArgs {
    /// How often to import: `1h`, `6h`, or `1d` (daily 08:00)
    #[arg(long, default_value = "1h")]
    pub every: String,
    /// Passed to `summa import --days-back`. Default: config, else 2.
    #[arg(long)]
    pub days_back: Option<i64>,
    /// auto | launchd | systemd | cron
    #[arg(long, default_value = "auto")]
    pub backend: String,
    /// Remove legacy `run-import.sh` / old `summa import` crontab lines
    #[arg(long)]
    pub replace: bool,
    /// Print unit/crontab text; do not register
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Launchd,
    Systemd,
    Cron,
    /// No crontab/systemd (e.g. Debian container): nohup sleep-loop.
    Loop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Hourly,
    EveryHours(u32),
    Daily { hour: u32 },
}

pub fn parse_every(raw: &str) -> anyhow::Result<Schedule> {
    let s = raw.trim().to_ascii_lowercase();
    match s.as_str() {
        "1h" | "1hr" | "hour" | "hourly" | "60m" => Ok(Schedule::Hourly),
        "1d" | "day" | "daily" => Ok(Schedule::Daily { hour: 8 }),
        other => {
            if let Some(n) = other.strip_suffix('h').and_then(|n| n.parse::<u32>().ok()) {
                if n == 0 || n > 24 {
                    bail!("--every hours must be 1..=24, got {n}");
                }
                if n == 1 {
                    return Ok(Schedule::Hourly);
                }
                if 24 % n == 0 {
                    return Ok(Schedule::EveryHours(n));
                }
                bail!("--every {n}h must divide 24 (use 1h, 2h, 3h, 4h, 6h, 8h, 12h, 1d)");
            }
            bail!("unknown --every `{raw}` (try 1h, 6h, 1d)")
        }
    }
}

pub fn parse_backend(raw: &str) -> anyhow::Result<Option<Backend>> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(None),
        "launchd" => Ok(Some(Backend::Launchd)),
        "systemd" => Ok(Some(Backend::Systemd)),
        "cron" | "crontab" => Ok(Some(Backend::Cron)),
        "loop" => Ok(Some(Backend::Loop)),
        other => bail!("unknown --backend `{other}` (auto|launchd|systemd|cron|loop)"),
    }
}

pub fn detect_backend() -> anyhow::Result<Backend> {
    if cfg!(target_os = "macos") {
        return Ok(Backend::Launchd);
    }
    if systemd_user_available() {
        return Ok(Backend::Systemd);
    }
    if command_exists("crontab") {
        return Ok(Backend::Cron);
    }
    Ok(Backend::Loop)
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success() || s.code().is_some())
        .unwrap_or(false)
        || Command::new("sh")
            .args(["-c", &format!("command -v {name}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

fn systemd_user_available() -> bool {
    Path::new("/run/systemd/system").exists() && command_exists("systemctl")
}

pub fn cron_expr(schedule: Schedule) -> &'static str {
    match schedule {
        Schedule::Hourly => "0 * * * *",
        Schedule::EveryHours(6) => "0 */6 * * *",
        Schedule::EveryHours(2) => "0 */2 * * *",
        Schedule::EveryHours(3) => "0 */3 * * *",
        Schedule::EveryHours(4) => "0 */4 * * *",
        Schedule::EveryHours(8) => "0 */8 * * *",
        Schedule::EveryHours(12) => "0 */12 * * *",
        Schedule::EveryHours(_) => "0 * * * *",
        Schedule::Daily { .. } => "0 8 * * *",
    }
}

pub fn systemd_on_calendar(schedule: Schedule) -> String {
    match schedule {
        Schedule::Hourly => "hourly".into(),
        Schedule::EveryHours(n) => format!("*-*-* 0/{n}:00:00"),
        Schedule::Daily { hour } => format!("*-*-* {hour:02}:00:00"),
    }
}

pub fn launchd_start_interval_secs(schedule: Schedule) -> u32 {
    match schedule {
        Schedule::Hourly => 3600,
        Schedule::EveryHours(n) => n * 3600,
        Schedule::Daily { .. } => 24 * 3600,
    }
}

pub fn preferred_bin() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        let local = home.join(".local/bin/summa");
        if local.is_file() {
            return local;
        }
        let cargo = home.join(".cargo/bin/summa");
        if cargo.is_file() {
            return cargo;
        }
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("summa"))
}

pub fn log_dir(home: &Path) -> PathBuf {
    home.join(".local/log/summa")
}

pub fn log_path(home: &Path) -> PathBuf {
    log_dir(home).join("cron.log")
}

pub fn launchd_plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

pub fn systemd_unit_dir(home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
}

pub fn job_script_path(home: &Path) -> PathBuf {
    home.join(".config/summa/import-job.sh")
}

pub fn loop_script_path(home: &Path) -> PathBuf {
    home.join(".config/summa/import-loop.sh")
}

pub fn loop_pid_path(home: &Path) -> PathBuf {
    home.join(".local/log/summa/import-loop.pid")
}

pub fn generate_loop_script(job_script: &Path, schedule: Schedule) -> String {
    let secs = launchd_start_interval_secs(schedule);
    let job = job_script.display().to_string().replace('\'', "'\\''");
    let mut s = String::from("#!/bin/sh\n");
    s.push_str(&format!("INTERVAL={secs}\n"));
    s.push_str(&format!("JOB='{job}'\n"));
    s.push_str("while true; do\n");
    s.push_str("  \"$JOB\"\n");
    s.push_str("  sleep \"$INTERVAL\"\n");
    s.push_str("done\n");
    s
}

pub fn generate_job_script(bin: &Path, days_back: i64) -> String {
    let bin_q = bin.display().to_string().replace('\'', "'\\''");
    let mut s = String::from("#!/bin/sh\n");
    s.push_str("export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH\"\n");
    s.push_str("for f in \"$HOME/.config/summa/env\" \"$HOME/.env\"; do\n");
    s.push_str("  if [ -r \"$f\" ]; then\n");
    s.push_str("    set -a\n");
    s.push_str("    # shellcheck disable=SC1090\n");
    s.push_str("    . \"$f\"\n");
    s.push_str("    set +a\n");
    s.push_str("  fi\n");
    s.push_str("done\n");
    let install_env = match bin.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            let dir_q = dir.display().to_string().replace('\'', "'\\''");
            format!("SUMMA_INSTALL_DIR='{dir_q}' ")
        }
        _ => String::new(),
    };
    s.push_str(&format!("BIN='{bin_q}'\n"));
    s.push_str("ensure_bin() {\n");
    s.push_str("  if [ -x \"$BIN\" ]; then\n");
    s.push_str("    return 0\n");
    s.push_str("  fi\n");
    s.push_str("  echo \"summa-import: $BIN is missing, reinstalling\" >&2\n");
    s.push_str(&format!(
        "  curl -fsSL {INSTALLER_URL} | {install_env}bash >&2 || true\n"
    ));
    s.push_str("  if [ ! -x \"$BIN\" ]; then\n");
    s.push_str("    echo \"summa-import: $BIN is still missing after reinstall\" >&2\n");
    s.push_str("    exit 127\n");
    s.push_str("  fi\n");
    s.push_str("}\n");
    s.push_str("ensure_bin\n");
    s.push_str("\"$BIN\" update >/dev/null || echo \"summa-import: update failed (exit $?), continuing\" >&2\n");
    s.push_str("ensure_bin\n");
    s.push_str(&format!("exec \"$BIN\" import --days-back={days_back}\n"));
    s
}

fn write_job_script(home: &Path, bin: &Path, days_back: i64) -> anyhow::Result<PathBuf> {
    let path = job_script_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, generate_job_script(bin, days_back))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)?;
    }
    Ok(path)
}

pub fn generate_launchd_plist(job_script: &Path, schedule: Schedule, log: &Path) -> String {
    let interval = launchd_start_interval_secs(schedule);
    let job_s = xml_escape(&job_script.display().to_string());
    let log_s = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{job_s}</string>
  </array>
  <key>StartInterval</key>
  <integer>{interval}</integer>
  <key>RunAtLoad</key>
  <false/>
  <key>StandardOutPath</key>
  <string>{log_s}</string>
  <key>StandardErrorPath</key>
  <string>{log_s}</string>
</dict>
</plist>
"#
    )
}

pub fn generate_systemd_service(job_script: &Path, log: &Path, home: &Path) -> String {
    let env_file = home.join(".config/summa/env");
    format!(
        "[Unit]\n\
         Description=summa usage import\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={}\n\
         Environment=PATH={}/.local/bin:{}/.cargo/bin:/usr/local/bin:/usr/bin:/bin\n\
         EnvironmentFile=-{}\n\
         StandardOutput=append:{}\n\
         StandardError=append:{}\n",
        job_script.display(),
        home.display(),
        home.display(),
        env_file.display(),
        log.display(),
        log.display(),
    )
}

pub fn generate_systemd_timer(schedule: Schedule) -> String {
    format!(
        "[Unit]\n\
         Description=summa usage import timer\n\
         \n\
         [Timer]\n\
         OnCalendar={}\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        systemd_on_calendar(schedule)
    )
}

pub fn generate_crontab_line(job_script: &Path, schedule: Schedule, log: &Path) -> String {
    format!(
        "{expr} {job} >> {log} 2>&1",
        expr = cron_expr(schedule),
        job = job_script.display(),
        log = log.display(),
    )
}

pub fn merge_crontab(existing: &str, new_line: &str, replace_legacy: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut skip_next_if_marker = false;
    for line in existing.lines() {
        if skip_next_if_marker {
            skip_next_if_marker = false;
            if is_managed_cron_line(line) {
                continue;
            }
        }
        if line.trim() == CRON_MARKER {
            skip_next_if_marker = true;
            continue;
        }
        if is_managed_cron_line(line) {
            continue;
        }
        if replace_legacy && is_legacy_cron_line(line) {
            continue;
        }
        out.push(line.to_string());
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.push(CRON_MARKER.to_string());
    out.push(new_line.to_string());
    out.push(String::new());
    out.join("\n")
}

pub fn strip_managed_crontab(existing: &str, replace_legacy: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut skip_next_if_marker = false;
    for line in existing.lines() {
        if skip_next_if_marker {
            skip_next_if_marker = false;
            if is_managed_cron_line(line) {
                continue;
            }
        }
        if line.trim() == CRON_MARKER {
            skip_next_if_marker = true;
            continue;
        }
        if is_managed_cron_line(line) {
            continue;
        }
        if replace_legacy && is_legacy_cron_line(line) {
            continue;
        }
        out.push(line.to_string());
    }
    if !out.is_empty() && !out.last().unwrap().is_empty() {
        out.push(String::new());
    }
    out.join("\n")
}

pub fn is_managed_cron_line(line: &str) -> bool {
    let t = line.trim();
    if t.starts_with('#') {
        return t == CRON_MARKER;
    }
    t.contains("summa import") || t.contains("/summa import") || t.contains("import-job.sh")
}

pub fn is_legacy_cron_line(line: &str) -> bool {
    let t = line.trim();
    !t.starts_with('#') && t.contains("run-import.sh")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub async fn run(args: CronjobArgs) -> anyhow::Result<()> {
    match args.action {
        CronjobAction::Install(opts) => install(opts).await,
        CronjobAction::Remove { dry_run } => remove(dry_run).await,
        CronjobAction::Status => show_status().await,
    }
}

async fn install(opts: InstallArgs) -> anyhow::Result<()> {
    let schedule = parse_every(&opts.every)?;
    let requested = parse_backend(&opts.backend)?;
    let backend = match requested {
        Some(b) => b,
        None => detect_backend()?,
    };
    let days_back = opts.days_back.unwrap_or_else(default_days_back);
    let bin = preferred_bin();
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let log = log_path(&home);

    if opts.dry_run {
        print_generated(&backend, &bin, days_back, schedule, &log, &home);
        return Ok(());
    }

    std::fs::create_dir_all(log_dir(&home))?;

    match backend {
        Backend::Launchd => install_launchd(&bin, days_back, schedule, &log, &home)?,
        Backend::Systemd => install_systemd(&bin, days_back, schedule, &log, &home)?,
        Backend::Cron => install_cron(&bin, days_back, schedule, &log, &home, opts.replace)?,
        Backend::Loop => install_loop(&bin, days_back, schedule, &log, &home)?,
    }

    if opts.replace && backend != Backend::Cron {
        let _ = strip_legacy_crontab();
    } else if has_legacy_crontab() {
        eprintln!("warn: legacy run-import.sh crontab still present; pass --replace to remove it");
    }

    println!("cronjob registered");
    println!("  backend: {}", backend_name(backend));
    println!("  every: {}", opts.every);
    println!("  bin: {}", bin.display());
    println!("  days_back: {days_back}");
    println!("  log: {}", log.display());
    println!("  env: {} (optional)", home.join(".config/summa/env").display());
    Ok(())
}

fn default_days_back() -> i64 {
    crate::config::Config::load(None)
        .ok()
        .and_then(|c| c.importer.days_back)
        .filter(|d| *d > 0)
        .unwrap_or(2)
}

fn backend_name(b: Backend) -> &'static str {
    match b {
        Backend::Launchd => "launchd",
        Backend::Systemd => "systemd",
        Backend::Cron => "cron",
        Backend::Loop => "loop",
    }
}

fn print_generated(
    backend: &Backend,
    bin: &Path,
    days_back: i64,
    schedule: Schedule,
    log: &Path,
    home: &Path,
) {
    println!("dry-run: would register {}", backend_name(*backend));
    println!("  bin: {}", bin.display());
    println!("  days_back: {days_back}");
    println!("  log: {}", log.display());
    let job = job_script_path(home);
    println!("--- {}", job.display());
    print!("{}", generate_job_script(bin, days_back));
    match backend {
        Backend::Launchd => {
            println!("--- {}", launchd_plist_path(home).display());
            print!("{}", generate_launchd_plist(&job, schedule, log));
        }
        Backend::Systemd => {
            println!("--- {}/{SYSTEMD_UNIT}.service", systemd_unit_dir(home).display());
            print!("{}", generate_systemd_service(&job, log, home));
            println!("--- {}/{SYSTEMD_UNIT}.timer", systemd_unit_dir(home).display());
            print!("{}", generate_systemd_timer(schedule));
        }
        Backend::Cron => {
            println!("--- crontab");
            println!("{CRON_MARKER}");
            println!("{}", generate_crontab_line(&job, schedule, log));
        }
        Backend::Loop => {
            println!("--- {}", loop_script_path(home).display());
            print!("{}", generate_loop_script(&job, schedule));
        }
    }
}

fn install_launchd(
    bin: &Path,
    days_back: i64,
    schedule: Schedule,
    log: &Path,
    home: &Path,
) -> anyhow::Result<()> {
    let job = write_job_script(home, bin, days_back)?;
    let plist = launchd_plist_path(home);
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = generate_launchd_plist(&job, schedule, log);
    std::fs::write(&plist, body)?;

    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "0".into());
    let domain = format!("gui/{uid}");

    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Older macOS
    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let bootstrap = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist)
        .output();
    match bootstrap {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("already") {
                let load = Command::new("launchctl")
                    .args(["load", "-w"])
                    .arg(&plist)
                    .status()
                    .context("launchctl load")?;
                if !load.success() {
                    bail!("launchctl bootstrap/load failed: {err}");
                }
            }
        }
        Err(_) => {
            let load = Command::new("launchctl")
                .args(["load", "-w"])
                .arg(&plist)
                .status()
                .context("launchctl load")?;
            if !load.success() {
                bail!("launchctl load failed");
            }
        }
    }
    Ok(())
}

fn install_systemd(
    bin: &Path,
    days_back: i64,
    schedule: Schedule,
    log: &Path,
    home: &Path,
) -> anyhow::Result<()> {
    let job = write_job_script(home, bin, days_back)?;
    let dir = systemd_unit_dir(home);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{SYSTEMD_UNIT}.service")),
        generate_systemd_service(&job, log, home),
    )?;
    std::fs::write(
        dir.join(format!("{SYSTEMD_UNIT}.timer")),
        generate_systemd_timer(schedule),
    )?;

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", "--now", &format!("{SYSTEMD_UNIT}.timer")])?;
    Ok(())
}

fn run_systemctl(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("systemctl {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn install_loop(
    bin: &Path,
    days_back: i64,
    schedule: Schedule,
    log: &Path,
    home: &Path,
) -> anyhow::Result<()> {
    let job = write_job_script(home, bin, days_back)?;
    let loop_sh = loop_script_path(home);
    if let Some(parent) = loop_sh.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&loop_sh, generate_loop_script(&job, schedule))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&loop_sh)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&loop_sh, perms)?;
    }
    stop_loop(home);
    std::fs::create_dir_all(log_dir(home))?;
    let pid_path = loop_pid_path(home);
    let child = Command::new(&loop_sh)
        .stdin(Stdio::null())
        .stdout(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)?,
        )
        .stderr(Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)?,
        ))
        .spawn()
        .context("start import-loop.sh")?;
    std::fs::write(&pid_path, format!("{}\n", child.id()))?;
    Ok(())
}

fn stop_loop(home: &Path) {
    let pid_path = loop_pid_path(home);
    if let Ok(s) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = s.trim().parse::<i32>() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
    let _ = std::fs::remove_file(pid_path);
}

fn loop_running(home: &Path) -> Option<i32> {
    let pid_path = loop_pid_path(home);
    let s = std::fs::read_to_string(pid_path).ok()?;
    let pid = s.trim().parse::<i32>().ok()?;
    let ok = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|st| st.success())
        .unwrap_or(false);
    if ok {
        Some(pid)
    } else {
        None
    }
}

fn install_cron(
    bin: &Path,
    days_back: i64,
    schedule: Schedule,
    log: &Path,
    home: &Path,
    replace: bool,
) -> anyhow::Result<()> {
    if !command_exists("crontab") {
        bail!("crontab not found; use --backend systemd (or install cron)");
    }
    let job = write_job_script(home, bin, days_back)?;
    let existing = read_crontab();
    let line = generate_crontab_line(&job, schedule, log);
    let merged = merge_crontab(&existing, &line, replace);
    write_crontab(&merged)
}

fn read_crontab() -> String {
    Command::new("crontab")
        .arg("-l")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn write_crontab(contents: &str) -> anyhow::Result<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("crontab -")?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("crontab stdin"))?;
        stdin.write_all(contents.as_bytes())?;
        if !contents.ends_with('\n') {
            stdin.write_all(b"\n")?;
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "crontab - failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn has_legacy_crontab() -> bool {
    read_crontab().lines().any(is_legacy_cron_line)
}

fn strip_legacy_crontab() -> anyhow::Result<()> {
    if !command_exists("crontab") {
        return Ok(());
    }
    let existing = read_crontab();
    let stripped = strip_managed_crontab(&existing, true);
    if stripped != existing {
        write_crontab(&stripped)?;
    }
    Ok(())
}

async fn remove(dry_run: bool) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    if dry_run {
        println!("dry-run: would unregister launchd/systemd/cron managed jobs");
        return Ok(());
    }

    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path(&home);
        let uid = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if !uid.is_empty() {
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
                .status();
        }
        let _ = Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&plist)
            .status();
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
    }

    if systemd_user_available() {
        let _ = run_systemctl(&["--user", "disable", "--now", &format!("{SYSTEMD_UNIT}.timer")]);
        let dir = systemd_unit_dir(&home);
        let _ = std::fs::remove_file(dir.join(format!("{SYSTEMD_UNIT}.service")));
        let _ = std::fs::remove_file(dir.join(format!("{SYSTEMD_UNIT}.timer")));
        let _ = run_systemctl(&["--user", "daemon-reload"]);
    }

    if command_exists("crontab") {
        let existing = read_crontab();
        let stripped = strip_managed_crontab(&existing, false);
        if stripped != existing {
            write_crontab(&stripped)?;
        }
    }

    stop_loop(&home);

    println!("cronjob removed");
    Ok(())
}

async fn show_status() -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let mut found = false;

    let plist = launchd_plist_path(&home);
    if plist.exists() {
        found = true;
        println!("cronjob: launchd");
        println!("  plist: {}", plist.display());
    }

    let timer = systemd_unit_dir(&home).join(format!("{SYSTEMD_UNIT}.timer"));
    if timer.exists() {
        found = true;
        println!("cronjob: systemd");
        println!("  timer: {}", timer.display());
        if let Ok(out) = Command::new("systemctl")
            .args(["--user", "is-enabled", &format!("{SYSTEMD_UNIT}.timer")])
            .output()
        {
            println!(
                "  enabled: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
    }

    let crontab = read_crontab();
    let managed: Vec<&str> = crontab
        .lines()
        .filter(|l| is_managed_cron_line(l) && !l.trim().starts_with('#'))
        .collect();
    let legacy: Vec<&str> = crontab.lines().filter(|l| is_legacy_cron_line(l)).collect();
    if !managed.is_empty() {
        found = true;
        println!("cronjob: crontab (managed)");
        for l in managed {
            println!("  {l}");
        }
    }
    if !legacy.is_empty() {
        found = true;
        println!("cronjob: crontab (legacy run-import.sh)");
        for l in legacy {
            println!("  {l}");
        }
    }

    if let Some(pid) = loop_running(&home) {
        found = true;
        println!("cronjob: loop");
        println!("  script: {}", loop_script_path(&home).display());
        println!("  pid: {pid}");
    }

    if !found {
        println!("cronjob: not installed");
        println!("  install: summa cronjob install --every 1h");
        if !command_exists("crontab") && !systemd_user_available() && !cfg!(target_os = "macos") {
            println!("  note: no crontab/systemd user timer detected");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_every_presets() {
        assert_eq!(parse_every("1h").unwrap(), Schedule::Hourly);
        assert_eq!(parse_every("hourly").unwrap(), Schedule::Hourly);
        assert_eq!(parse_every("6h").unwrap(), Schedule::EveryHours(6));
        assert_eq!(parse_every("1d").unwrap(), Schedule::Daily { hour: 8 });
        assert!(parse_every("5h").is_err());
        assert!(parse_every("nope").is_err());
    }

    #[test]
    fn schedule_exprs() {
        assert_eq!(cron_expr(Schedule::Hourly), "0 * * * *");
        assert_eq!(cron_expr(Schedule::EveryHours(6)), "0 */6 * * *");
        assert_eq!(cron_expr(Schedule::Daily { hour: 8 }), "0 8 * * *");
        assert_eq!(systemd_on_calendar(Schedule::Hourly), "hourly");
        assert_eq!(
            systemd_on_calendar(Schedule::EveryHours(6)),
            "*-*-* 0/6:00:00"
        );
        assert_eq!(launchd_start_interval_secs(Schedule::Hourly), 3600);
        assert_eq!(launchd_start_interval_secs(Schedule::EveryHours(6)), 21600);
    }

    #[test]
    fn job_script_sources_env_and_execs_bin() {
        let sh = generate_job_script(Path::new("/opt/summa"), 3);
        assert!(sh.starts_with("#!/bin/sh"));
        assert!(sh.contains(". \"$HOME/.config/summa/env\"") || sh.contains("$HOME/.config/summa/env"));
        assert!(sh.contains("BIN='/opt/summa'"));
        assert!(sh.contains("exec \"$BIN\" import --days-back=3"));
    }

    #[cfg(unix)]
    fn write_exe(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// Runs the generated job with an isolated HOME; a `curl` in
    /// `$HOME/.local/bin` shadows the real one (the script puts that dir first).
    #[cfg(unix)]
    fn run_job_script(home: &Path, bin: &Path) -> std::process::Output {
        let script = home.join("job.sh");
        std::fs::write(&script, generate_job_script(bin, 3)).unwrap();
        Command::new("/bin/sh")
            .arg(&script)
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    const FAKE_INSTALLER_CURL: &str = "#!/bin/sh
cat <<'EOF'
mkdir -p \"$SUMMA_INSTALL_DIR\"
printf '#!/bin/sh\\necho \"ran $*\"\\n' > \"$SUMMA_INSTALL_DIR/summa\"
chmod 755 \"$SUMMA_INSTALL_DIR/summa\"
EOF
";

    #[cfg(unix)]
    #[test]
    fn job_script_reinstalls_missing_binary_then_imports() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".local/bin/summa");
        write_exe(&tmp.path().join(".local/bin/curl"), FAKE_INSTALLER_CURL);

        let out = run_job_script(tmp.path(), &bin);

        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("ran import --days-back=3"));
        assert!(String::from_utf8_lossy(&out.stderr).contains("is missing, reinstalling"));
    }

    #[cfg(unix)]
    #[test]
    fn job_script_fails_loudly_when_reinstall_cannot_restore_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".local/bin/summa");
        write_exe(
            &tmp.path().join(".local/bin/curl"),
            "#!/bin/sh\necho 'curl: (22) server said no' >&2\nexit 22\n",
        );

        let out = run_job_script(tmp.path(), &bin);

        assert_eq!(out.status.code(), Some(127), "{out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("server said no"), "{stderr}");
        assert!(stderr.contains("still missing after reinstall"), "{stderr}");
    }

    #[cfg(unix)]
    #[test]
    fn job_script_logs_update_failure_and_still_imports() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".local/bin/summa");
        write_exe(
            &bin,
            "#!/bin/sh\nif [ \"$1\" = update ]; then echo 'boom from update' >&2; exit 3; fi\necho \"ran $*\"\n",
        );

        let out = run_job_script(tmp.path(), &bin);

        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("ran import --days-back=3"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("boom from update"), "{stderr}");
        assert!(stderr.contains("update failed (exit 3)"), "{stderr}");
    }

    #[test]
    fn launchd_plist_contains_job_script_and_interval() {
        let plist = generate_launchd_plist(
            Path::new("/home/me/.config/summa/import-job.sh"),
            Schedule::Hourly,
            Path::new("/tmp/cron.log"),
        );
        assert!(plist.contains(LAUNCHD_LABEL));
        assert!(plist.contains("import-job.sh"));
        assert!(plist.contains("<integer>3600</integer>"));
        assert!(plist.contains("/tmp/cron.log"));
    }

    #[test]
    fn systemd_units_contain_exec_and_calendar() {
        let home = PathBuf::from("/home/box");
        let svc = generate_systemd_service(
            Path::new("/home/box/.config/summa/import-job.sh"),
            Path::new("/home/box/.local/log/summa/cron.log"),
            &home,
        );
        assert!(svc.contains("ExecStart=/home/box/.config/summa/import-job.sh"));
        assert!(svc.contains("EnvironmentFile=-/home/box/.config/summa/env"));
        let timer = generate_systemd_timer(Schedule::EveryHours(6));
        assert!(timer.contains("OnCalendar=*-*-* 0/6:00:00"));
        assert!(timer.contains("WantedBy=timers.target"));
    }

    #[test]
    fn crontab_merge_replaces_managed_keeps_others() {
        let existing = "MAILTO=me\n0 8 * * * /old/summa import --days-back=1\n";
        let merged = merge_crontab(
            existing,
            "0 * * * * /home/me/.config/summa/import-job.sh >> /log 2>&1",
            false,
        );
        assert!(merged.contains("MAILTO=me"));
        assert!(merged.contains(CRON_MARKER));
        assert!(merged.contains("import-job.sh"));
        assert!(!merged.contains("/old/summa"));
    }

    #[test]
    fn crontab_merge_replace_drops_run_import_sh() {
        let existing =
            "17 * * * * /Users/me/project/ccusage-import/run-import.sh 2>&1 | tee log\n";
        let merged = merge_crontab(
            existing,
            "0 * * * * /home/me/.config/summa/import-job.sh >> /l 2>&1",
            true,
        );
        assert!(!merged.contains("run-import.sh"));
        assert!(merged.contains("import-job.sh"));
        let kept = merge_crontab(
            existing,
            "0 * * * * /home/me/.config/summa/import-job.sh >> /l 2>&1",
            false,
        );
        assert!(kept.contains("run-import.sh"));
    }

    #[test]
    fn strip_managed_leaves_unrelated() {
        let existing = format!(
            "PATH=/bin\n{CRON_MARKER}\n0 * * * * /bin/summa import --days-back=2 >> /l 2>&1\n"
        );
        let stripped = strip_managed_crontab(&existing, false);
        assert!(stripped.contains("PATH=/bin"));
        assert!(!stripped.contains("summa import"));
        assert!(!stripped.contains(CRON_MARKER));
    }

    #[test]
    fn loop_script_sleeps_interval() {
        let sh = generate_loop_script(Path::new("/home/box/.config/summa/import-job.sh"), Schedule::EveryHours(6));
        assert!(sh.contains("INTERVAL=21600"));
        assert!(sh.contains("import-job.sh"));
        assert!(sh.contains("sleep \"$INTERVAL\""));
    }

    #[test]
    fn clap_install_every_and_dry_run() {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "summa",
            "cronjob",
            "install",
            "--every",
            "6h",
            "--days-back",
            "3",
            "--dry-run",
            "--replace",
        ])
        .unwrap();
        match cli.command {
            Commands::Cronjob(args) => match args.action {
                CronjobAction::Install(a) => {
                    assert_eq!(a.every, "6h");
                    assert_eq!(a.days_back, Some(3));
                    assert!(a.dry_run);
                    assert!(a.replace);
                }
                other => panic!("expected Install, got {other:?}"),
            },
            other => panic!("expected Cronjob, got {other:?}"),
        }
    }
}
