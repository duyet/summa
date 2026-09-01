use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub type EnvLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// Product / XDG config directory name (`~/.config/summa/`).
pub const CONFIG_DIR_NAME: &str = "summa";
/// Main config file basename.
pub const CONFIG_FILE_NAME: &str = "config.toml";
/// Credentials file basename (passwords/tokens only).
pub const CREDENTIALS_FILE_NAME: &str = "credentials.toml";
/// Env override for main config path.
pub const CONFIG_ENV: &str = "SUMMA_CONFIG";
/// Env override for credentials path.
pub const CREDENTIALS_ENV: &str = "SUMMA_CREDENTIALS";
/// Legacy env override (still honored).
pub const LEGACY_CONFIG_ENV: &str = "CCUSAGE_IMPORT_CONFIG";

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ClickHouseConfig {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub protocol: String,
}

impl ClickHouseConfig {
    pub fn from_env() -> Self {
        let port_str = std::env::var("CH_PORT").unwrap_or_else(|_| "8123".to_string());
        let port = port_str.parse().unwrap_or(8123);
        Self {
            host: std::env::var("CH_HOST").unwrap_or_else(|_| "localhost".to_string()),
            port,
            user: std::env::var("CH_USER").unwrap_or_else(|_| "default".to_string()),
            password: std::env::var("CH_PASSWORD").unwrap_or_default(),
            database: std::env::var("CH_DATABASE").unwrap_or_else(|_| "default".to_string()),
            protocol: std::env::var("CH_PROTOCOL").unwrap_or_else(|_| {
                if port == 443 || port == 8443 || port == 9440 {
                    "https".to_string()
                } else {
                    "http".to_string()
                }
            }),
        }
    }
}

/// Secrets-only overlay. Never required in main `config.toml`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Credentials {
    #[serde(default)]
    pub clickhouse_password: Option<String>,
    #[serde(default)]
    pub motherduck_token: Option<String>,
    /// Alias accepted for MotherDuck token.
    #[serde(default)]
    pub motherduck: Option<String>,
    #[serde(default)]
    pub ch_password: Option<String>,
    /// Cookie header or WorkosCursorSessionToken for cursor.com dashboard APIs.
    #[serde(default)]
    pub cursor_session: Option<String>,
    /// Alias for `cursor_session`.
    #[serde(default)]
    pub cursor_cookie: Option<String>,
    /// Cursor Team Admin API key (Basic auth username).
    #[serde(default)]
    pub cursor_api_key: Option<String>,
    /// Bearer token for `summa serve` ingest/analytics.
    #[serde(default)]
    pub telemetry_token: Option<String>,
}

impl Credentials {
    /// Load credentials from an explicit path or discovery order.
    pub fn load(path: Option<&str>) -> anyhow::Result<Self> {
        let raw = match path {
            Some(p) => {
                if Path::new(p).exists() {
                    std::fs::read_to_string(p)?
                } else {
                    String::new()
                }
            }
            None => Self::find_and_read()?,
        };
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let interpolated = Config::interpolate_env(&raw);
        Ok(toml::from_str(&interpolated)?)
    }

    fn find_and_read() -> anyhow::Result<String> {
        for path in Self::candidate_paths() {
            if Path::new(&path).exists() {
                return Ok(std::fs::read_to_string(&path)?);
            }
        }
        Ok(String::new())
    }

    /// Credential file discovery (first existing wins).
    /// Order: `$SUMMA_CREDENTIALS` → `./credentials.toml` → `./summa.credentials.toml`
    /// → `~/.config/summa/credentials.toml` → `~/.summa/credentials.toml`
    pub fn candidate_paths() -> Vec<String> {
        let mut candidates = Vec::new();
        if let Ok(env_path) = std::env::var(CREDENTIALS_ENV) {
            candidates.push(env_path);
        }
        candidates.push("./credentials.toml".to_string());
        candidates.push("./summa.credentials.toml".to_string());
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join(".config")
                    .join(CONFIG_DIR_NAME)
                    .join(CREDENTIALS_FILE_NAME)
                    .display()
                    .to_string(),
            );
        }
        if let Some(config_home) = dirs::config_dir() {
            let p = config_home
                .join(CONFIG_DIR_NAME)
                .join(CREDENTIALS_FILE_NAME)
                .display()
                .to_string();
            if !candidates.iter().any(|c| c == &p) {
                candidates.push(p);
            }
        }
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join(".summa")
                    .join(CREDENTIALS_FILE_NAME)
                    .display()
                    .to_string(),
            );
        }
        candidates
    }

    pub fn clickhouse_password(&self) -> Option<&str> {
        self.clickhouse_password
            .as_deref()
            .or(self.ch_password.as_deref())
            .filter(|s| !s.is_empty())
    }

    pub fn motherduck_token(&self) -> Option<&str> {
        self.motherduck_token
            .as_deref()
            .or(self.motherduck.as_deref())
            .filter(|s| !s.is_empty())
    }

    pub fn cursor_session(&self) -> Option<&str> {
        self.cursor_session
            .as_deref()
            .or(self.cursor_cookie.as_deref())
            .filter(|s| !s.is_empty())
    }

    pub fn cursor_api_key(&self) -> Option<&str> {
        self.cursor_api_key.as_deref().filter(|s| !s.is_empty())
    }

    pub fn telemetry_token(&self) -> Option<&str> {
        self.telemetry_token.as_deref().filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ImporterConfig {
    pub hash_project_names: Option<bool>,
    pub machine_name: Option<String>,
    pub command_timeout: Option<u64>,
    pub max_parallel_workers: Option<u32>,
    pub duckdb_path: Option<String>,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub skip_ccusage: Option<bool>,
    pub skip_opencode: Option<bool>,
    pub skip_codex: Option<bool>,
    pub skip_antigravity: Option<bool>,
    pub skip_hermes: Option<bool>,
    pub skip_grok: Option<bool>,
    pub skip_cursor: Option<bool>,
    pub skip_clickhouse: Option<bool>,
    pub opencode_path: Option<String>,
    pub codex_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UiConfig {
    pub animated: Option<bool>,
    pub color: Option<bool>,
    pub verbose: Option<bool>,
    pub quiet: Option<bool>,
    pub heatmap_min_intensity: Option<u8>,
    pub heatmap_max_intensity: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SinksConfig {
    /// Ordered sink routes: first matching enabled sink wins for writes.
    /// Supported values: `local`, `motherduck`, `clickhouse`.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Explicit overrides when route-based selection is insufficient.
    #[serde(default)]
    pub skip_clickhouse: Option<bool>,
    #[serde(default)]
    pub skip_duckdb: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TelemetryConfig {
    /// Cloud hub URL (default https://summa.duyet.net).
    pub endpoint: Option<String>,
    /// Optional bearer token (prefer credentials.toml `telemetry_token`).
    pub token: Option<String>,
    /// Deprecated local bind; ignored. Cloud hub replaced `summa serve`.
    pub bind: Option<String>,
}

/// Release channel for `summa update` / auto-update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    /// Master-branch CI builds (rolling).
    #[default]
    Beta,
    /// release-please tagged GitHub Releases.
    Stable,
}

impl UpdateChannel {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "beta" => Ok(Self::Beta),
            "stable" => Ok(Self::Stable),
            other => Err(anyhow::anyhow!(
                "unknown update channel `{other}` (expected beta or stable)"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Beta => "beta",
            Self::Stable => "stable",
        }
    }
}

impl std::str::FromStr for UpdateChannel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl std::fmt::Display for UpdateChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Auto-update mode. `auto` = check after each command, download in background,
/// new binary is active on the next invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// Check and download on every run; install lands for next launch.
    #[default]
    Manual,
    Auto,
}

impl UpdateMode {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "manual" | "off" => Ok(Self::Manual),
            "auto" => Ok(Self::Auto),
            other => Err(anyhow::anyhow!(
                "unknown update mode `{other}` (expected manual or auto)"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

impl std::str::FromStr for UpdateMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl std::fmt::Display for UpdateMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UpdateConfig {
    /// `stable` (release-please tags) or `beta` (master CI builds).
    pub channel: Option<String>,
    /// `manual` or `auto` (download updates in background for next launch).
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub clickhouse: ClickHouseConfig,
    #[serde(default)]
    pub importer: ImporterConfig,
    #[serde(default)]
    pub sinks: SinksConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub update: UpdateConfig,
}

impl Config {
    pub fn load(path: Option<&str>) -> anyhow::Result<Self> {
        let raw = match path {
            Some(p) => std::fs::read_to_string(p)?,
            None => Self::find_and_read()?,
        };
        let interpolated = Self::interpolate_env(&raw);
        let mut cfg: Config = if raw.trim().is_empty() {
            Config::default()
        } else {
            toml::from_str(&interpolated)?
        };
        cfg.apply_credentials(&Credentials::load(None)?)?;
        cfg.apply_env_fallback();
        Ok(cfg)
    }

    /// Load config + credentials with explicit credential path (for tests).
    /// When `config_path` is `Some`, the file must exist (CLI `--config`).
    /// When `None`, discovery is used and missing files yield defaults.
    pub fn load_with_credentials(
        config_path: Option<&str>,
        credentials_path: Option<&str>,
    ) -> anyhow::Result<Self> {
        let raw = match config_path {
            Some(p) => std::fs::read_to_string(p).map_err(|e| {
                anyhow::anyhow!("config file not found or unreadable at `{p}`: {e}")
            })?,
            None => Self::find_and_read()?,
        };
        let interpolated = Self::interpolate_env(&raw);
        let mut cfg: Config = if raw.trim().is_empty() {
            Config::default()
        } else {
            toml::from_str(&interpolated)?
        };
        let creds = Credentials::load(credentials_path)?;
        cfg.apply_credentials(&creds)?;
        cfg.apply_env_fallback();
        Ok(cfg)
    }

    /// Merge secrets from a credentials file. Password/token in main config
    /// win if already set; credentials fill empty fields only.
    pub fn apply_credentials(&mut self, creds: &Credentials) -> anyhow::Result<()> {
        if self.clickhouse.password.is_empty() {
            if let Some(pw) = creds.clickhouse_password() {
                self.clickhouse.password = pw.to_string();
            }
        }
        // MotherDuck token is env-driven at runtime; export if not already set.
        if std::env::var("MOTHERDUCK_TOKEN").ok().filter(|s| !s.is_empty()).is_none() {
            if let Some(token) = creds.motherduck_token() {
                std::env::set_var("MOTHERDUCK_TOKEN", token);
            }
        }
        if std::env::var("CURSOR_SESSION")
            .ok()
            .filter(|s| !s.is_empty())
            .is_none()
            && std::env::var("CURSOR_COOKIE")
                .ok()
                .filter(|s| !s.is_empty())
                .is_none()
        {
            if let Some(session) = creds.cursor_session() {
                std::env::set_var("CURSOR_SESSION", session);
            }
        }
        if std::env::var("CURSOR_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .is_none()
        {
            if let Some(key) = creds.cursor_api_key() {
                std::env::set_var("CURSOR_API_KEY", key);
            }
        }
        if self
            .telemetry
            .token
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            if let Some(t) = creds.telemetry_token.as_deref().filter(|s| !s.is_empty()) {
                self.telemetry.token = Some(t.to_string());
            }
        }
        Ok(())
    }

    fn find_and_read() -> anyhow::Result<String> {
        for path in Self::candidate_paths() {
            if Path::new(&path).exists() {
                return Ok(std::fs::read_to_string(&path)?);
            }
        }
        Ok(String::new())
    }

    /// Config discovery order (first existing wins).
    ///
    /// 1. `$SUMMA_CONFIG` / `$CCUSAGE_IMPORT_CONFIG`
    /// 2. `./summa.toml` / `./summa-import.toml`
    /// 3. `~/.config/summa/config.toml` (XDG)
    /// 4. `~/.summa/config.toml`
    /// 5. `~/.summa-import.toml` (legacy)
    /// 6. `/etc/summa/config.toml`
    pub fn candidate_paths() -> Vec<String> {
        Self::candidate_paths_with(|k| std::env::var(k).ok(), dirs::config_dir(), dirs::home_dir())
    }

    /// Pure path resolution for tests (inject env + home dirs).
    pub fn candidate_paths_with<F>(
        env_lookup: F,
        config_dir: Option<PathBuf>,
        home_dir: Option<PathBuf>,
    ) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut candidates = Vec::new();

        if let Some(p) = env_lookup(CONFIG_ENV) {
            candidates.push(p);
        }
        if let Some(p) = env_lookup(LEGACY_CONFIG_ENV) {
            candidates.push(p);
        }

        candidates.push("./summa.toml".to_string());
        candidates.push("./summa-import.toml".to_string());

        if let Some(home) = &home_dir {
            candidates.push(
                home.join(".config")
                    .join(CONFIG_DIR_NAME)
                    .join(CONFIG_FILE_NAME)
                    .display()
                    .to_string(),
            );
        }

        if let Some(config_home) = config_dir {
            let p = config_home
                .join(CONFIG_DIR_NAME)
                .join(CONFIG_FILE_NAME)
                .display()
                .to_string();
            if !candidates.iter().any(|c| c == &p) {
                candidates.push(p);
            }
        }

        if let Some(home) = home_dir {
            candidates.push(
                home.join(".summa")
                    .join(CONFIG_FILE_NAME)
                    .display()
                    .to_string(),
            );
            candidates.push(format!("{}/.summa-import.toml", home.display()));
        }

        candidates.push(format!("/etc/{CONFIG_DIR_NAME}/{CONFIG_FILE_NAME}"));
        candidates
    }

    /// Default local DuckDB path: `~/.local/share/summa/summa.duckdb`
    /// (or platform data dir). Auto-created by the DuckDB sink on open.
    pub fn default_duckdb_path() -> String {
        Self::default_duckdb_path_with(dirs::data_local_dir(), dirs::home_dir())
    }

    pub fn default_duckdb_path_with(
        data_local: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> String {
        if let Some(base) = data_local {
            return base
                .join(CONFIG_DIR_NAME)
                .join("summa.duckdb")
                .display()
                .to_string();
        }
        if let Some(home) = home {
            return home
                .join(".local")
                .join("share")
                .join(CONFIG_DIR_NAME)
                .join("summa.duckdb")
                .display()
                .to_string();
        }
        format!("./{CONFIG_DIR_NAME}.duckdb")
    }

    /// Resolve DuckDB path: CLI/env/config override, else local default.
    /// MotherDuck (`md:…`) is only used when explicitly configured.
    pub fn resolve_duckdb_path(explicit: Option<&str>) -> String {
        if let Some(p) = explicit {
            if !p.is_empty() {
                return p.to_string();
            }
        }
        if let Ok(p) = std::env::var("DUCKDB_PATH") {
            if !p.is_empty() {
                return p;
            }
        }
        Self::default_duckdb_path()
    }

    pub fn interpolate_env(input: &str) -> String {
        Self::interpolate_env_with(input, |k| std::env::var(k).ok())
    }

    fn interpolate_env_with(input: &str, env_lookup: impl Fn(&str) -> Option<String>) -> String {
        let re = regex::Regex::new(r"\$\{([^}]+)\}").expect("valid env interpolation regex");
        re.replace_all(input, |caps: &regex::Captures<'_>| {
            env_lookup(&caps[1]).unwrap_or_else(|| caps[0].to_string())
        })
        .to_string()
    }

    pub fn apply_env_fallback(&mut self) {
        self.apply_env_fallback_with(|k| std::env::var(k).ok());
    }

    pub fn apply_env_fallback_with<F>(&mut self, env_lookup: F)
    where
        F: Fn(&str) -> Option<String>,
    {
        if self.clickhouse.host.is_empty() {
            self.clickhouse.host = env_lookup("CH_HOST").unwrap_or_default();
        }
        if self.clickhouse.port == 0 {
            self.clickhouse.port = env_lookup("CH_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(8123);
        }
        if self.clickhouse.user.is_empty() {
            self.clickhouse.user = env_lookup("CH_USER").unwrap_or_default();
        }
        if self.clickhouse.password.is_empty() {
            self.clickhouse.password = env_lookup("CH_PASSWORD").unwrap_or_default();
        }
        if self.clickhouse.database.is_empty() {
            self.clickhouse.database = env_lookup("CH_DATABASE").unwrap_or_default();
        }
        if self.clickhouse.protocol.is_empty() {
            self.clickhouse.protocol = env_lookup("CH_PROTOCOL").unwrap_or_else(|| "http".into());
        }
        if self.importer.duckdb_path.is_none() {
            self.importer.duckdb_path = env_lookup("DUCKDB_PATH");
        }
        if self.importer.machine_name.is_none() {
            self.importer.machine_name = env_lookup("IMPORT_MACHINE_NAME");
        }
        if self.importer.command_timeout.is_none() {
            self.importer.command_timeout = env_lookup("IMPORT_COMMAND_TIMEOUT_MS")
                .and_then(|v| v.parse().ok());
        }
        if self.importer.max_parallel_workers.is_none() {
            self.importer.max_parallel_workers = env_lookup("IMPORT_MAX_PARALLEL_WORKERS")
                .and_then(|v| v.parse().ok());
        }
        if self.importer.hash_project_names.is_none() {
            self.importer.hash_project_names = env_lookup("IMPORT_HASH_PROJECT_NAMES")
                .and_then(|v| v.parse().ok());
        }
        if self.importer.days_back.is_none() {
            self.importer.days_back = env_lookup("IMPORT_DAYS_BACK")
                .and_then(|v| v.parse().ok());
        }
        if self.importer.opencode_path.is_none() {
            self.importer.opencode_path = env_lookup("OPENCODE_DATA_DIR");
        }
        if self.importer.codex_path.is_none() {
            self.importer.codex_path = env_lookup("CODEX_HOME");
        }
        if self
            .telemetry
            .endpoint
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            if let Some(ep) = env_lookup("SUMMA_TELEMETRY_ENDPOINT").filter(|s| !s.is_empty()) {
                self.telemetry.endpoint = Some(ep);
            }
        }
        if self
            .telemetry
            .bind
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            if let Some(bind) = env_lookup("SUMMA_TELEMETRY_BIND").filter(|s| !s.is_empty()) {
                self.telemetry.bind = Some(bind);
            }
        }
        if self
            .telemetry
            .token
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            if let Some(token) = env_lookup("SUMMA_TELEMETRY_TOKEN").filter(|s| !s.is_empty()) {
                self.telemetry.token = Some(token);
            }
        }
    }

    /// Resolve sink activation based on `[sinks]` routes plus legacy skip flags.
    /// Order matters: routes are evaluated first-to-last and the first enabled
    /// cloud/local sink becomes the active target for writes.
    ///
    /// Supported routes:
    /// - `local` — local DuckDB file
    /// - `motherduck` — `md:...` DuckDB database
    /// - `clickhouse` — ClickHouse HTTP sink
    ///
    /// Legacy `skip_clickhouse` / `skip_duckdb` still apply when no routes are set.
    pub fn sink_routes(&self) -> SinkRoutes {
        SinkRoutes::resolve(self)
    }

    /// Update channel: config `[update] channel`, else `SUMMA_UPDATE_CHANNEL`, else beta.
    pub fn update_channel(&self) -> UpdateChannel {
        if let Some(c) = self.update.channel.as_deref().filter(|s| !s.is_empty()) {
            if let Ok(ch) = UpdateChannel::parse(c) {
                return ch;
            }
        }
        if let Ok(v) = std::env::var("SUMMA_UPDATE_CHANNEL") {
            if let Ok(ch) = UpdateChannel::parse(&v) {
                return ch;
            }
        }
        UpdateChannel::default()
    }

    /// Update mode: config `[update] mode`, else `SUMMA_UPDATE_MODE`, else manual.
    pub fn update_mode(&self) -> UpdateMode {
        if let Some(m) = self.update.mode.as_deref().filter(|s| !s.is_empty()) {
            if let Ok(m) = UpdateMode::parse(m) {
                return m;
            }
        }
        if let Ok(v) = std::env::var("SUMMA_UPDATE_MODE") {
            if let Ok(m) = UpdateMode::parse(&v) {
                return m;
            }
        }
        UpdateMode::default()
    }

    pub fn to_env_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();

        map.insert("CH_HOST".into(), self.clickhouse.host.clone());
        map.insert("CH_PORT".into(), self.clickhouse.port.to_string());
        map.insert("CH_USER".into(), self.clickhouse.user.clone());
        map.insert("CH_PASSWORD".into(), self.clickhouse.password.clone());
        map.insert("CH_DATABASE".into(), self.clickhouse.database.clone());
        map.insert("CH_PROTOCOL".into(), self.clickhouse.protocol.clone());

        if let Some(path) = &self.importer.duckdb_path {
            map.insert("DUCKDB_PATH".into(), path.clone());
        }
        if let Some(name) = &self.importer.machine_name {
            map.insert("IMPORT_MACHINE_NAME".into(), name.clone());
        }
        if let Some(timeout) = self.importer.command_timeout {
            map.insert("IMPORT_COMMAND_TIMEOUT_MS".into(), timeout.to_string());
        }
        if let Some(workers) = self.importer.max_parallel_workers {
            map.insert("IMPORT_MAX_PARALLEL_WORKERS".into(), workers.to_string());
        }
        if let Some(hash) = self.importer.hash_project_names {
            map.insert("IMPORT_HASH_PROJECT_NAMES".into(), hash.to_string());
        }
        if let Some(days) = self.importer.days_back {
            map.insert("IMPORT_DAYS_BACK".into(), days.to_string());
        }
        if let Some(path) = &self.importer.opencode_path {
            map.insert("OPENCODE_DATA_DIR".into(), path.clone());
        }
        if let Some(path) = &self.importer.codex_path {
            map.insert("CODEX_HOME".into(), path.clone());
        }

        map
    }

    pub fn write_toml<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<PathBuf> {
        let content = toml::to_string_pretty(self)?;
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&path, content)?;
        Ok(path.as_ref().to_path_buf())
    }

    /// First existing config path, or the XDG default when none exist.
    pub fn resolve_write_path(explicit: Option<&str>) -> PathBuf {
        if let Some(p) = explicit.filter(|s| !s.is_empty()) {
            return PathBuf::from(p);
        }
        Self::candidate_paths()
            .into_iter()
            .find(|p| Path::new(p).exists())
            .map(PathBuf::from)
            .unwrap_or_else(Self::default_config_path)
    }

    fn default_config_path() -> PathBuf {
        if let Some(home) = dirs::home_dir() {
            return home
                .join(".config")
                .join(CONFIG_DIR_NAME)
                .join(CONFIG_FILE_NAME);
        }
        dirs::config_dir()
            .map(|d| d.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME))
            .unwrap_or_else(|| PathBuf::from(CONFIG_FILE_NAME))
    }

    /// Set a dotted config key (`update.channel`, `update.mode`, …) in the
    /// config file, preserving formatting and comments via toml_edit.
    pub fn set_value(path: &Path, key: &str, value: &str) -> anyhow::Result<()> {
        const KNOWN: &[&str] = &[
            "update.channel",
            "update.mode",
            "importer.machine_name",
            "importer.days_back",
            "importer.since",
            "importer.end_date",
            "importer.duckdb_path",
            "telemetry.endpoint",
        ];
        if !KNOWN.contains(&key) {
            anyhow::bail!("unknown config key `{key}` (known: {})", KNOWN.join(", "));
        }
        let mut doc = match std::fs::read_to_string(path) {
            Ok(text) => text.parse::<toml_edit::DocumentMut>()?,
            Err(_) => toml_edit::DocumentMut::new(),
        };
        let parsed = parse_set_value(key, value)?;
        let segments: Vec<&str> = key.split('.').collect();
        let mut table = doc.as_table_mut();
        for seg in &segments[..segments.len() - 1] {
            table = table
                .entry(seg)
                .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
                .as_table_mut()
                .ok_or_else(|| anyhow::anyhow!("config key `{seg}` is not a table"))?;
        }
        table.insert(segments[segments.len() - 1], toml_edit::value(parsed));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, doc.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvLock;
    use std::io::Write;

    #[test]
    fn empty_toml_returns_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.clickhouse.host.is_empty());
        assert_eq!(cfg.clickhouse.port, 0);
    }

    #[test]
    fn parses_full_toml() {
        let toml_str = r#"
            [clickhouse]
            host = "localhost"
            port = 8123
            user = "default"
            password = ""
            database = "analytics"
            protocol = "https"

            [importer]
            machine_name = "devbox"
            hash_project_names = true
            days_back = 7
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.clickhouse.host, "localhost");
        assert_eq!(cfg.clickhouse.port, 8123);
        assert_eq!(cfg.clickhouse.user, "default");
        assert_eq!(cfg.clickhouse.database, "analytics");
        assert_eq!(cfg.clickhouse.protocol, "https");
        assert_eq!(cfg.importer.machine_name.as_deref(), Some("devbox"));
        assert_eq!(cfg.importer.hash_project_names, Some(true));
        assert_eq!(cfg.importer.days_back, Some(7));
    }

    #[test]
    fn env_interpolation_replaces_placeholders() {
        let mut env_map: HashMap<String, String> = HashMap::new();
        env_map.insert("MY_CH_HOST".into(), "db.example.com".into());
        env_map.insert("MY_CH_DB".into(), "prod".into());
        let toml_str = r#"
            [clickhouse]
            host = "${MY_CH_HOST}"
            port = 8123
            user = "default"
            password = ""
            database = "${MY_CH_DB}"
            protocol = "http"
        "#;
        let interpolated = Config::interpolate_env_with(toml_str, |k| env_map.get(k).cloned());
        let cfg: Config = toml::from_str(&interpolated).unwrap();
        assert_eq!(cfg.clickhouse.host, "db.example.com");
        assert_eq!(cfg.clickhouse.database, "prod");
    }

    #[test]
    fn env_interpolation_keeps_placeholder_on_missing() {
        let toml_str = r#"
            [clickhouse]
            host = "${MISSING_VAR}"
            port = 8123
            user = "default"
            password = ""
            database = "analytics"
            protocol = "http"
        "#;
        let interpolated = Config::interpolate_env_with(toml_str, |_| None);
        let cfg: Config = toml::from_str(&interpolated).unwrap();
        assert_eq!(cfg.clickhouse.host, "${MISSING_VAR}");
    }

    #[test]
    fn apply_env_fallback_when_fields_empty() {
        let mut env_map: HashMap<String, String> = HashMap::new();
        env_map.insert("CH_HOST".into(), "env-host".into());
        env_map.insert("CH_PORT".into(), "8443".into());
        env_map.insert("CH_DATABASE".into(), "env_db".into());
        env_map.insert("DUCKDB_PATH".into(), "/tmp/duck.duckdb".into());
        env_map.insert("IMPORT_MACHINE_NAME".into(), "env-machine".into());

        let toml_str = r#"
            [clickhouse]
            host = ""
            port = 0
            user = ""
            password = ""
            database = ""
            protocol = ""

            [importer]
        "#;
        let mut cfg: Config = toml::from_str(toml_str).unwrap();
        cfg.apply_env_fallback_with(|key| env_map.get(key).cloned());
        assert_eq!(cfg.clickhouse.host, "env-host");
        assert_eq!(cfg.clickhouse.port, 8443);
        assert_eq!(cfg.clickhouse.user, "");
        assert_eq!(cfg.clickhouse.password, "");
        assert_eq!(cfg.clickhouse.database, "env_db");
        assert_eq!(cfg.clickhouse.protocol, "http");
        assert_eq!(
            cfg.importer.duckdb_path.as_deref(),
            Some("/tmp/duck.duckdb")
        );
        assert_eq!(cfg.importer.machine_name.as_deref(), Some("env-machine"));
    }

    #[test]
    fn candidate_paths_include_local_and_xdg() {
        let config_dir = PathBuf::from("/home/user/.config");
        let home = PathBuf::from("/home/user");
        let paths = Config::candidate_paths_with(|_| None, Some(config_dir), Some(home));
        assert!(paths.iter().any(|p| p.ends_with("./summa.toml")));
        assert!(paths
            .iter()
            .any(|p| p.contains("/.config/summa/config.toml")));
        assert!(paths.iter().any(|p| p.contains("/.summa/config.toml")));
        assert!(paths.iter().any(|p| p.ends_with(".summa-import.toml")));
        assert!(paths.iter().any(|p| p == "/etc/summa/config.toml"));
        // Local project files before XDG
        let local_idx = paths.iter().position(|p| p == "./summa.toml").unwrap();
        let xdg_idx = paths
            .iter()
            .position(|p| p.contains("/.config/summa/config.toml"))
            .unwrap();
        assert!(local_idx < xdg_idx);
    }

    #[test]
    fn candidate_paths_env_override_first() {
        let paths = Config::candidate_paths_with(
            |k| {
                if k == CONFIG_ENV {
                    Some("/custom/summa.toml".into())
                } else {
                    None
                }
            },
            None,
            None,
        );
        assert_eq!(paths[0], "/custom/summa.toml");
    }

    #[test]
    fn credentials_fill_password_separately_from_main_config() {
        let _env = EnvLock::isolate_summa();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let creds_path = dir.path().join("credentials.toml");

        let mut f = std::fs::File::create(&config_path).unwrap();
        writeln!(
            f,
            r#"[clickhouse]
host = "ch.example.com"
port = 8443
user = "analytics"
password = ""
database = "usage"
protocol = "https"
"#
        )
        .unwrap();

        let mut f = std::fs::File::create(&creds_path).unwrap();
        writeln!(
            f,
            r#"clickhouse_password = "s3cret-from-creds"
motherduck_token = "md-token-xyz"
"#
        )
        .unwrap();

        // Main TOML must not require password
        let main: Config = toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(main.clickhouse.password.is_empty());

        let cfg = Config::load_with_credentials(
            Some(config_path.to_str().unwrap()),
            Some(creds_path.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(cfg.clickhouse.host, "ch.example.com");
        assert_eq!(cfg.clickhouse.password, "s3cret-from-creds");
        assert_eq!(
            std::env::var("MOTHERDUCK_TOKEN").ok().as_deref(),
            Some("md-token-xyz")
        );
    }

    #[test]
    fn default_duckdb_path_uses_data_local() {
        let path = Config::default_duckdb_path_with(
            Some(PathBuf::from("/Users/me/Library/Application Support")),
            Some(PathBuf::from("/Users/me")),
        );
        assert!(path.ends_with("summa/summa.duckdb"));
        assert!(path.contains("Application Support") || path.contains("summa"));
    }

    #[test]
    fn resolve_duckdb_path_prefers_explicit_over_default() {
        let _env = EnvLock::isolate_summa();
        let p = Config::resolve_duckdb_path(Some("md:cloud-db"));
        assert_eq!(p, "md:cloud-db");
        let local = Config::resolve_duckdb_path(Some(""));
        // empty explicit falls through to default (DUCKDB_PATH isolated)
        assert!(!local.is_empty());
        assert!(!local.starts_with("md:"));
    }

    #[test]
    fn round_trip_toml_preserves_values() {
        let original = Config {
            clickhouse: ClickHouseConfig {
                host: "h".into(),
                port: 1,
                user: "u".into(),
                password: "p".into(),
                database: "d".into(),
                protocol: "http".into(),
            },
            importer: ImporterConfig {
                machine_name: Some("m".into()),
                days_back: Some(3),
                ..ImporterConfig::default()
            },
            sinks: SinksConfig::default(),
            ui: UiConfig::default(),
            telemetry: TelemetryConfig::default(),
            update: UpdateConfig::default(),
        };
        let toml_str = toml::to_string_pretty(&original).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.clickhouse.host, "h");
        assert_eq!(parsed.importer.machine_name.as_deref(), Some("m"));
        assert_eq!(parsed.importer.days_back, Some(3));
    }

    #[test]
    fn update_channel_parses_and_rejects() {
        assert_eq!(UpdateChannel::parse("beta").unwrap(), UpdateChannel::Beta);
        assert_eq!(
            UpdateChannel::parse("Stable").unwrap(),
            UpdateChannel::Stable
        );
        assert!(UpdateChannel::parse("nightly").is_err());
        assert_eq!(UpdateChannel::default().as_str(), "beta");
    }

    #[test]
    fn update_mode_parses_and_defaults_manual() {
        assert_eq!(UpdateMode::parse("auto").unwrap(), UpdateMode::Auto);
        assert_eq!(UpdateMode::parse("manual").unwrap(), UpdateMode::Manual);
        assert_eq!(UpdateMode::parse("off").unwrap(), UpdateMode::Manual);
        assert!(UpdateMode::parse("whenever").is_err());
        assert_eq!(UpdateMode::default().as_str(), "manual");
    }

    #[test]
    fn update_section_round_trips() {
        let cfg: Config = toml::from_str(
            r#"
            [update]
            channel = "stable"
            mode = "auto"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.update_channel(), UpdateChannel::Stable);
        assert_eq!(cfg.update_mode(), UpdateMode::Auto);
    }

    #[test]
    fn update_channel_env_fallback() {
        let env = EnvLock::isolate_summa();
        let cfg = Config::default();
        env.set("SUMMA_UPDATE_CHANNEL", "stable");
        assert_eq!(cfg.update_channel(), UpdateChannel::Stable);
        env.set("SUMMA_UPDATE_MODE", "auto");
        assert_eq!(cfg.update_mode(), UpdateMode::Auto);
    }

    #[test]
    fn set_value_writes_update_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::set_value(&path, "update.channel", "stable").unwrap();
        Config::set_value(&path, "update.mode", "auto").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[update]"));
        assert!(text.contains("channel = \"stable\""));
        assert!(text.contains("mode = \"auto\""));
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.update.channel.as_deref(), Some("stable"));
        assert_eq!(cfg.update.mode.as_deref(), Some("auto"));
    }

    #[test]
    fn set_value_preserves_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my machine\n[importer]\nmachine_name = \"laptop\"\n",
        )
        .unwrap();
        Config::set_value(&path, "update.channel", "beta").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my machine"), "comment kept: {text}");
        assert!(text.contains("machine_name = \"laptop\""));
    }

    #[test]
    fn set_value_rejects_unknown_key_and_bad_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::set_value(&path, "nope.key", "x").is_err());
        assert!(Config::set_value(&path, "update.channel", "nightly").is_err());
        assert!(Config::set_value(&path, "importer.days_back", "soon").is_err());
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let env = EnvLock::isolate_summa();
        // Isolated empty file — `load(None)` would pick up a live XDG config.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.toml");
        std::fs::write(&path, "").unwrap();
        let creds = dir.path().join("credentials.toml");
        std::fs::write(&creds, "").unwrap();
        env.set(CREDENTIALS_ENV, creds.to_str().unwrap());
        let cfg = Config::load(Some(path.to_str().unwrap())).unwrap();
        assert!(cfg.clickhouse.host.is_empty());
    }

    #[test]
    fn credentials_candidate_paths_include_xdg() {
        let _env = EnvLock::isolate_summa();
        let paths = Credentials::candidate_paths();
        assert!(paths.iter().any(|p| p.ends_with("credentials.toml")));
    }

    #[test]
    fn telemetry_env_fills_bind_and_token() {
        let mut cfg = Config::default();
        let mut env_map: HashMap<String, String> = HashMap::new();
        env_map.insert("SUMMA_TELEMETRY_ENDPOINT".into(), "https://summa.duyet.net".into());
        env_map.insert("SUMMA_TELEMETRY_BIND".into(), "0.0.0.0:8787".into());
        env_map.insert("SUMMA_TELEMETRY_TOKEN".into(), "tok".into());
        cfg.apply_env_fallback_with(|k| env_map.get(k).cloned());
        assert_eq!(
            cfg.telemetry.endpoint.as_deref(),
            Some("https://summa.duyet.net")
        );
        assert_eq!(cfg.telemetry.bind.as_deref(), Some("0.0.0.0:8787"));
        assert_eq!(cfg.telemetry.token.as_deref(), Some("tok"));
    }
}

/// Resolved sink activation for an import run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkRoutes {
    pub local: bool,
    pub motherduck: bool,
    pub clickhouse: bool,
}

/// Coerce a CLI-provided string into the TOML value type for a dotted key.
fn parse_set_value(key: &str, value: &str) -> anyhow::Result<toml_edit::Value> {
    let v = value.trim();
    Ok(match key {
        "update.channel" => toml_edit::Value::from(UpdateChannel::parse(v)?.as_str()),
        "update.mode" => toml_edit::Value::from(UpdateMode::parse(v)?.as_str()),
        "importer.days_back" => toml_edit::Value::from(
            v.parse::<i64>()
                .map_err(|_| anyhow::anyhow!("importer.days_back must be an integer"))?,
        ),
        "telemetry.endpoint" | "importer.machine_name" | "importer.since"
        | "importer.end_date" | "importer.duckdb_path" => toml_edit::Value::from(v),
        _ => anyhow::bail!("unknown config key `{key}`"),
    })
}

impl SinkRoutes {
    /// Resolve which sinks are active for writes.
    ///
    /// Behavior:
    /// - If `routes` is non-empty, enable sinks in route order until one matches.
    ///   `local` always matches when DuckDB is local-first. `motherduck` matches when
    ///   `duckdb_path` starts with `md:`. `clickhouse` matches when CH host is set.
    /// - If `routes` is empty, fall back to legacy `skip_*` flags.
    pub fn resolve(cfg: &Config) -> Self {
        if !cfg.sinks.routes.is_empty() {
            let duckdb_path = Config::resolve_duckdb_path(cfg.importer.duckdb_path.as_deref());
            let is_md = duckdb_path.starts_with("md:");
            let has_ch = !cfg.clickhouse.host.is_empty();

            let mut routes = Self {
                local: false,
                motherduck: false,
                clickhouse: false,
            };
            for route in &cfg.sinks.routes {
                match route.as_str() {
                    "local" if !is_md => {
                        routes.local = true;
                        break;
                    }
                    "motherduck" if is_md => {
                        routes.motherduck = true;
                        break;
                    }
                    "clickhouse" if has_ch => {
                        routes.clickhouse = true;
                        break;
                    }
                    _ => continue,
                }
            }
            return routes;
        }

        let skip_ch = cfg.sinks.skip_clickhouse.unwrap_or(false)
            || cfg.importer.skip_clickhouse.unwrap_or(false);
        let skip_duckdb = cfg.sinks.skip_duckdb.unwrap_or(false);

        let duckdb_path = Config::resolve_duckdb_path(cfg.importer.duckdb_path.as_deref());
        let is_md = duckdb_path.starts_with("md:");
        let has_ch = !cfg.clickhouse.host.is_empty();

        Self {
            local: !is_md && !skip_duckdb,
            motherduck: is_md && !skip_duckdb,
            clickhouse: has_ch && !skip_ch,
        }
    }
}

#[cfg(test)]
mod sink_routes_tests {
    use super::*;
    use crate::test_env::EnvLock;

    #[test]
    fn routes_local_when_configured() {
        let _env = EnvLock::isolate_summa();
        let cfg = Config {
            sinks: SinksConfig {
                routes: vec!["local".into()],
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        let routes = SinkRoutes::resolve(&cfg);
        assert!(routes.local);
        assert!(!routes.motherduck);
        assert!(!routes.clickhouse);
    }

    #[test]
    fn routes_motherduck_when_md_prefix() {
        let cfg = Config {
            sinks: SinksConfig {
                routes: vec!["motherduck".into()],
                ..SinksConfig::default()
            },
            importer: ImporterConfig {
                duckdb_path: Some("md:usage".into()),
                ..ImporterConfig::default()
            },
            clickhouse: ClickHouseConfig {
                host: "ch.example".into(),
                ..ClickHouseConfig::default()
            },
            ..Config::default()
        };
        let routes = SinkRoutes::resolve(&cfg);
        assert!(routes.motherduck);
        assert!(!routes.local);
        assert!(!routes.clickhouse);
    }

    #[test]
    fn routes_clickhouse_when_host_set() {
        let _env = EnvLock::isolate_summa();
        let cfg = Config {
            sinks: SinksConfig {
                routes: vec!["clickhouse".into()],
                ..SinksConfig::default()
            },
            clickhouse: ClickHouseConfig {
                host: "ch.example".into(),
                ..ClickHouseConfig::default()
            },
            ..Config::default()
        };
        let routes = SinkRoutes::resolve(&cfg);
        assert!(routes.clickhouse);
        assert!(!routes.local);
        assert!(!routes.motherduck);
    }

    #[test]
    fn legacy_flags_when_no_routes() {
        let cfg = Config {
            sinks: SinksConfig {
                skip_clickhouse: Some(true),
                skip_duckdb: Some(false),
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        let routes = SinkRoutes::resolve(&cfg);
        assert!(!routes.clickhouse);
        assert!(routes.local);
        assert!(!routes.motherduck);
    }
}
