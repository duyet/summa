use regex::Regex;
use std::sync::OnceLock;

const MASK: &str = "[REDACTED]";

fn token_param() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(motherduck_token\s*=\s*['"]?)[^&\s'")]+"#).expect("valid regex")
    })
}

/// Hide the MotherDuck token in `text`, whether it appears as a
/// `motherduck_token=...` connection parameter or as the bare env value.
pub fn redact_secrets(text: &str) -> String {
    let mut out = token_param()
        .replace_all(text, format!("${{1}}{MASK}"))
        .into_owned();
    if let Ok(token) = std::env::var("MOTHERDUCK_TOKEN") {
        if !token.is_empty() {
            out = out.replace(&token, MASK);
        }
    }
    out
}

/// Re-render `err` (with its cause chain) with secrets hidden. Apply where an
/// error is produced from a connection string, before it reaches a log.
pub fn redact_error(err: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(redact_secrets(&format!("{err:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvLock;

    #[test]
    fn hides_token_in_connection_string_error() {
        let err = anyhow::anyhow!("IO Error: cannot open `md:ccusage?motherduck_token=fake-value-1` failed");
        let shown = redact_error(err).to_string();
        assert!(!shown.contains("fake-value-1"), "{shown}");
        assert!(shown.contains("md:ccusage?motherduck_token=[REDACTED]"), "{shown}");
    }

    #[test]
    fn keeps_other_query_params() {
        let shown = redact_secrets("md:db?motherduck_token=abc123&x=1 failed");
        assert_eq!(shown, "md:db?motherduck_token=[REDACTED]&x=1 failed");
    }

    #[test]
    fn hides_token_in_the_cause_chain() {
        let inner = anyhow::anyhow!("open md:db?motherduck_token=sekret");
        let err = inner.context("connect failed");
        let shown = redact_error(err).to_string();
        assert!(!shown.contains("sekret"), "{shown}");
        assert!(shown.starts_with("connect failed"), "{shown}");
    }

    #[test]
    fn hides_the_env_token_value_without_a_param_name() {
        let env = EnvLock::isolate(&["MOTHERDUCK_TOKEN"]);
        env.set("MOTHERDUCK_TOKEN", "fake-env-token");
        let shown = redact_secrets("auth rejected for fake-env-token");
        assert_eq!(shown, "auth rejected for [REDACTED]");
    }

    #[test]
    fn text_without_a_token_is_unchanged() {
        let _env = EnvLock::isolate(&["MOTHERDUCK_TOKEN"]);
        assert_eq!(redact_secrets("md:ccusage open failed"), "md:ccusage open failed");
    }
}
