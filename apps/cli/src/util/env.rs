//! Dotenv loading shared by every command.
//!
//! `.env.local` is loaded before `.env`. dotenvy never overrides a variable that is
//! already set, so the first writer wins: the real environment beats `.env.local`,
//! and `.env.local` beats `.env`.

/// Dotenv files in precedence order (highest first).
const DOTENV_FILES: [&str; 2] = [".env.local", ".env"];

/// Load `.env.local` then `.env`, searching the current directory and its parents.
/// Missing files are normal and ignored.
pub fn load_dotenv() {
    for name in DOTENV_FILES {
        let _ = dotenvy::from_filename(name);
    }
}

#[cfg(test)]
fn load_dotenv_in(dir: &std::path::Path) {
    for name in DOTENV_FILES {
        let _ = dotenvy::from_path(dir.join(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvLock;

    const KEY: &str = "SUMMA_TEST_DOTENV_KEY";
    const ONLY_ENV: &str = "SUMMA_TEST_DOTENV_ONLY_ENV";

    #[test]
    fn env_local_wins_over_env_and_env_still_fills_gaps() {
        let _env = EnvLock::isolate(&[KEY, ONLY_ENV]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env.local"), format!("{KEY}=local\n")).unwrap();
        std::fs::write(
            dir.path().join(".env"),
            format!("{KEY}=shared\n{ONLY_ENV}=from-env\n"),
        )
        .unwrap();

        load_dotenv_in(dir.path());

        assert_eq!(std::env::var(KEY).as_deref(), Ok("local"));
        assert_eq!(std::env::var(ONLY_ENV).as_deref(), Ok("from-env"));
    }

    #[test]
    fn real_environment_wins_over_both_files() {
        let env = EnvLock::isolate(&[KEY]);
        env.set(KEY, "process");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env.local"), format!("{KEY}=local\n")).unwrap();
        std::fs::write(dir.path().join(".env"), format!("{KEY}=shared\n")).unwrap();

        load_dotenv_in(dir.path());

        assert_eq!(std::env::var(KEY).as_deref(), Ok("process"));
    }

    #[test]
    fn missing_files_are_ignored() {
        let _env = EnvLock::isolate(&[KEY]);
        let dir = tempfile::tempdir().unwrap();

        load_dotenv_in(dir.path());

        assert!(std::env::var(KEY).is_err());
    }
}
