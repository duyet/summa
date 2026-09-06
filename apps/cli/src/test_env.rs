//! Process-wide environment isolation for unit tests.
//!
//! `std::env::{set_var, remove_var}` is shared by every test thread. Hold
//! [`EnvLock`] for the full mutate+assert window so env-dependent tests cannot
//! race each other or tests that *read* contested keys (`DUCKDB_PATH`,
//! `MOTHERDUCK_TOKEN`, `CH_*`, …).

use std::ffi::OsStr;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Keys that import/config tests mutate or whose ambient values would change
/// assertions if a parallel env-mutating test leaked them.
pub const SUMMA_ENV_KEYS: &[&str] = &[
    "CH_HOST",
    "CH_PORT",
    "CH_USER",
    "CH_PASSWORD",
    "CH_DATABASE",
    "CH_PROTOCOL",
    "DUCKDB_PATH",
    "MOTHERDUCK_TOKEN",
    "motherduck_token",
    "IMPORT_DAYS_BACK",
    "IMPORT_MACHINE_NAME",
    "IMPORT_SINCE",
    "IMPORT_SINCE_DATE",
    "IMPORT_END_DATE",
    "IMPORT_COMMAND_TIMEOUT_MS",
    "IMPORT_MAX_PARALLEL_WORKERS",
    "IMPORT_HASH_PROJECT_NAMES",
    "HASH_PROJECT_NAMES",
    "SUMMA_CONFIG",
    "SUMMA_CREDENTIALS",
    "CCUSAGE_IMPORT_CONFIG",
    "SUMMA_UPDATE_CHANNEL",
    "SUMMA_UPDATE_MODE",
    "SUMMA_TELEMETRY_ENDPOINT",
    "SUMMA_TELEMETRY_BIND",
    "SUMMA_TELEMETRY_TOKEN",
    "CURSOR_SESSION",
    "CURSOR_COOKIE",
    "CURSOR_API_KEY",
    "CODEX_HOME",
    "OPENCODE_DATA_DIR",
    "HERMES_HOME",
    "GROK_HOME",
];

/// Exclusive access to process env. Restores snapshotted keys on drop.
pub struct EnvLock {
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<String>)>,
}

impl EnvLock {
    /// Lock the process env, snapshot `keys`, and clear them.
    pub fn isolate(keys: &[&str]) -> Self {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = keys
            .iter()
            .map(|k| ((*k).to_string(), std::env::var(k).ok()))
            .collect::<Vec<_>>();
        for key in keys {
            std::env::remove_var(key);
        }
        Self { _guard, saved }
    }

    /// Isolate the contested summa import/config keys.
    pub fn isolate_summa() -> Self {
        Self::isolate(SUMMA_ENV_KEYS)
    }

    pub fn set(&self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        std::env::set_var(key, value);
    }
}

impl Drop for EnvLock {
    fn drop(&mut self) {
        for (key, prev) in self.saved.drain(..) {
            match prev {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
    }
}

mod tests {
    use super::*;

    #[test]
    fn isolate_clears_sets_and_restores_absent() {
        let env = EnvLock::isolate(&["SUMMA_TEST_ENV_LOCK_ABSENT"]);
        assert!(std::env::var("SUMMA_TEST_ENV_LOCK_ABSENT").is_err());
        env.set("SUMMA_TEST_ENV_LOCK_ABSENT", "inside");
        assert_eq!(
            std::env::var("SUMMA_TEST_ENV_LOCK_ABSENT").as_deref(),
            Ok("inside")
        );
        drop(env);
        assert!(std::env::var("SUMMA_TEST_ENV_LOCK_ABSENT").is_err());
    }
}
