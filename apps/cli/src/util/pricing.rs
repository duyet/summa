/**
 * Shared model cost estimation from public provider rates.
 *
 * Used when a source has no billed cost (Antigravity, free Hermes sessions)
 * or when Hermes `estimated_cost_usd` is absurd relative to token volume.
 *
 * Rates are USD per 1M tokens: (input, cache_read, cache_write, output).
 * Keep these in sync with provider docs; imperfect rates beat silent $0.
 */

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelRates {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

const FREE: ModelRates = ModelRates {
    input: 0.0,
    cache_read: 0.0,
    cache_write: 0.0,
    output: 0.0,
};

/// Gemini 3 Flash Preview.
const GEMINI_3_FLASH: ModelRates = ModelRates {
    input: 0.50,
    cache_read: 0.05,
    cache_write: 0.50,
    output: 3.00,
};

/// Gemini 3.6 / 3.7 / 3.8 Flash. Introductory rates through 2026-12-31, then
/// $1.50 / $7.50 / $0.15 from 2027-01-01. Rates here are the current ones.
const GEMINI_36_FLASH: ModelRates = ModelRates {
    input: 0.75,
    cache_read: 0.075,
    cache_write: 0.75,
    output: 3.75,
};

/// Gemini 3.5 Flash.
const GEMINI_35_FLASH: ModelRates = ModelRates {
    input: 1.50,
    cache_read: 0.15,
    cache_write: 1.50,
    output: 9.00,
};

/// Gemini 3.5 Flash-Lite.
const GEMINI_35_FLASH_LITE: ModelRates = ModelRates {
    input: 0.30,
    cache_read: 0.03,
    cache_write: 0.30,
    output: 2.50,
};

/// Gemini 3.1 Flash-Lite.
const GEMINI_31_FLASH_LITE: ModelRates = ModelRates {
    input: 0.25,
    cache_read: 0.025,
    cache_write: 0.25,
    output: 1.50,
};

/// Gemini 2.5 Flash.
const GEMINI_25_FLASH: ModelRates = ModelRates {
    input: 0.30,
    cache_read: 0.03,
    cache_write: 0.30,
    output: 2.50,
};

/// Gemini 2.5 Pro (≤200k prompt).
const GEMINI_25_PRO: ModelRates = ModelRates {
    input: 1.25,
    cache_read: 0.125,
    cache_write: 1.25,
    output: 10.00,
};

/// Gemini 3.1 Pro Preview (≤200k prompt).
const GEMINI_31_PRO: ModelRates = ModelRates {
    input: 2.00,
    cache_read: 0.20,
    cache_write: 2.00,
    output: 12.00,
};

/// Claude Sonnet 4.x (Anthropic list, cache write = 1.25× input).
const CLAUDE_SONNET_4: ModelRates = ModelRates {
    input: 3.00,
    cache_read: 0.30,
    cache_write: 3.75,
    output: 15.00,
};

/// Claude Sonnet 5 / 5.5. The $2/$10 launch price became standard price; the
/// scheduled September 2026 increase to $3/$15 did not happen.
const CLAUDE_SONNET_5: ModelRates = ModelRates {
    input: 2.00,
    cache_read: 0.20,
    cache_write: 2.50,
    output: 10.00,
};

/// Claude Opus 4.1 / 4 (retired on the first-party API, still live on Bedrock
/// and Google Cloud).
const CLAUDE_OPUS_4: ModelRates = ModelRates {
    input: 15.00,
    cache_read: 1.50,
    cache_write: 18.75,
    output: 75.00,
};

/// Claude Opus 4.5 through 4.8. Opus 4.1 kept the older $15/$75 rates, so the
/// split is by minor version and not by family name.
const CLAUDE_OPUS_4_5: ModelRates = ModelRates {
    input: 5.00,
    cache_read: 0.50,
    cache_write: 6.25,
    output: 25.00,
};

/// Claude Opus 5 / 5.5.
const CLAUDE_OPUS_5: ModelRates = ModelRates {
    input: 5.00,
    cache_read: 0.50,
    cache_write: 6.25,
    output: 25.00,
};

/// Claude Haiku 4.5.
const CLAUDE_HAIKU_4_5: ModelRates = ModelRates {
    input: 1.00,
    cache_read: 0.10,
    cache_write: 1.25,
    output: 5.00,
};

/// Claude Haiku 3.5.
const CLAUDE_HAIKU_3_5: ModelRates = ModelRates {
    input: 0.80,
    cache_read: 0.08,
    cache_write: 1.00,
    output: 4.00,
};

/// Z.AI / GLM mid-tier (OpenRouter-ish).
const GLM_MID: ModelRates = ModelRates {
    input: 0.50,
    cache_read: 0.05,
    cache_write: 0.50,
    output: 1.50,
};

/// Default when model is unknown — mid Flash tier.
const DEFAULT_RATES: ModelRates = GEMINI_3_FLASH;

fn normalize(model: &str) -> String {
    model.to_ascii_lowercase().replace('_', "-").replace(' ', "-")
}

/// Parse the `(major, minor)` version out of a model id containing `family`.
///
/// Handles `claude-opus-4-5-20260101` (4.5), `claude-opus-4.1` (4.1), `opus-5`
/// (5.0), and display names like `Claude Opus 5 (Thinking)`. Returns `None`
/// when the id carries no version, which leaves the caller on its default.
fn version(normalized: &str, family: &str) -> Option<(u32, u32)> {
    let after = normalized.split_once(family)?.1;
    let start = after.find(|c: char| c.is_ascii_digit())?;
    let digits: String = after[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // A long trailing number is a release date, not a version: the tail of
    // "3-5-haiku-20241022" is 20241022. Only short groups can be versions.
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let major: u32 = digits.parse().ok()?;
    let rest = &after[start + digits.len()..];
    for sep in ['.', '-'] {
        if let Some(tail) = rest.strip_prefix(sep) {
            let minor: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            // "4-5-20260101" is 4.5, not 4.20260101.
            if !minor.is_empty() && minor.len() <= 2 {
                if let Ok(m) = minor.parse::<u32>() {
                    return Some((major, m));
                }
            }
        }
    }
    Some((major, 0))
}

/// Map a free-form model id / display name to public rates.
/// Which Gemini price tier a model id names.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum GeminiTier {
    Flash,
    FlashLite,
    Pro,
}

/// Read the version and class out of a Gemini model id.
///
/// Gemini spells versions with dots (`gemini-3.5-flash`) or hyphens
/// (`gemini-2-5-pro`), so this scans for the first short number group rather
/// than reusing the family-anchored `version` helper.
fn gemini_tier(m: &str) -> Option<(GeminiTier, u32, u32)> {
    let bytes = m.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // A long group is a release date, not a version.
        if i - start > 2 {
            continue;
        }
        let major: u32 = m[start..i].parse().ok()?;
        let mut minor = 0u32;
        let sep = if i < bytes.len() && (bytes[i] == b'.' || bytes[i] == b'-') {
            Some(bytes[i])
        } else {
            None
        };
        if let Some(sep) = sep {
            let ns = i + 1;
            let mut j = ns;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > ns && j - ns <= 2 {
                minor = m[ns..j].parse().ok()?;
                i = j;
            } else {
                // A separator followed by more than two digits is a date.
                let _ = sep;
            }
        }
        let tier = if m.contains("pro") {
            GeminiTier::Pro
        } else if m.contains("lite") {
            GeminiTier::FlashLite
        } else {
            GeminiTier::Flash
        };
        return Some((tier, major, minor));
    }
    None
}

pub fn rates_for_model(model: &str) -> ModelRates {
    let m = normalize(model);

    // Free / unpriced tiers
    if m.contains("free")
        || m.contains("gpt-oss")
        || m.contains("openrouter/free")
        || m.contains("anyrouter/free")
    {
        return FREE;
    }

    // Anthropic. Rates moved twice: Opus 4.1 was $15/$75, Opus 4.5 dropped to
    // $5/$25, and Sonnet 5 dropped to $2/$10. Family name alone is ambiguous.
    if m.contains("opus") {
        return match version(&m, "opus") {
            Some((4, minor)) if minor <= 1 => CLAUDE_OPUS_4,
            _ => CLAUDE_OPUS_5,
        };
    }
    if m.contains("sonnet") {
        return match version(&m, "sonnet") {
            Some((major, _)) if major >= 5 => CLAUDE_SONNET_5,
            _ => CLAUDE_SONNET_4,
        };
    }
    if m.contains("haiku") {
        return match version(&m, "haiku") {
            Some((major, _)) if major >= 4 => CLAUDE_HAIKU_4_5,
            _ => CLAUDE_HAIKU_3_5,
        };
    }
    if m.contains("claude") {
        // bare "claude" → current sonnet-class default
        return CLAUDE_SONNET_5;
    }

    // Google Gemini. Pro and Lite are separate price tiers from Flash, and
    // 3.6/3.7/3.8 Flash are cheaper than 3.5, so the family name is not enough.
    if m.contains("gemini") || m.contains("flash") {
        return match gemini_tier(&m) {
            Some((GeminiTier::Pro, major, _)) => {
                if major >= 3 {
                    GEMINI_31_PRO
                } else {
                    GEMINI_25_PRO
                }
            }
            Some((GeminiTier::FlashLite, 3, minor)) if minor >= 5 => GEMINI_35_FLASH_LITE,
            Some((GeminiTier::FlashLite, _, _)) => GEMINI_31_FLASH_LITE,
            // Within 3.x the rate fell at 3.6: 3.5 is $1.50/$9.00, 3.6+ is
            // $0.75/$3.75. 2.x stays on the 2.5 Flash rate.
            Some((GeminiTier::Flash, 3, minor)) if minor >= 6 => GEMINI_36_FLASH,
            Some((GeminiTier::Flash, 3, minor)) if minor >= 1 => GEMINI_35_FLASH,
            Some((GeminiTier::Flash, 3, _)) => GEMINI_3_FLASH,
            Some((GeminiTier::Flash, _, _)) => GEMINI_25_FLASH,
            None => GEMINI_36_FLASH,
        };
    }

    // Z.AI / GLM
    if m.contains("glm") || m.contains("z-ai") || m.contains("zai") {
        return GLM_MID;
    }

    // Hermes presets often route to a mid coding model — Flash-class default
    if m.contains("hermes") || m.contains("@preset") {
        return GEMINI_3_FLASH;
    }

    // Gemma / other open weights — treat as free-ish local unless known paid
    if m.contains("gemma") {
        return FREE;
    }

    DEFAULT_RATES
}

/// Estimate USD cost from token breakdown + model name.
///
/// Returns the precise value: callers sum many per-record costs into a row, so
/// rounding here would round away small turns before they are ever added up.
pub fn estimate_model_cost(
    model: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
) -> f64 {
    let rates = rates_for_model(model);
    (input_tokens as f64 / 1_000_000.0) * rates.input
        + (cache_read_tokens as f64 / 1_000_000.0) * rates.cache_read
        + (cache_write_tokens as f64 / 1_000_000.0) * rates.cache_write
        + (output_tokens as f64 / 1_000_000.0) * rates.output
}

fn round_cents(cost: f64) -> f64 {
    (cost * 100.0).round() / 100.0
}

/// Reported cost below this is never rejected on a per-token basis.
///
/// A "blended $/1M" sanity check is only meaningful once the amount is large
/// enough for the ratio to mean something: a genuine $0.25 request on 150
/// tokens is $1667/1M, which looks insane but is just an expensive model on a
/// tiny sample. Real corruption (Hermes' wild estimates) shows up in the
/// dollars, not the ratio.
const RATIO_CHECK_FLOOR_USD: f64 = 1.0;

/// Hermes sometimes stores wild `estimated_cost_usd` values.
/// Prefer reported cost when sane; otherwise fall back to token estimate.
///
/// "Sane" = positive, and not implausible per-token spend once the amount is
/// large enough for a per-token ratio to carry information.
pub fn resolve_reported_cost(
    model: &str,
    reported: f64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
) -> f64 {
    let estimated = estimate_model_cost(
        model,
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        output_tokens,
    );
    let total = input_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_write_tokens)
        .saturating_add(output_tokens);

    if reported <= 0.0 {
        return estimated;
    }
    if total == 0 {
        return 0.0;
    }

    if reported > RATIO_CHECK_FLOOR_USD {
        let blended_per_m = reported / (total as f64 / 1_000_000.0);
        if blended_per_m > 200.0 {
            return estimated;
        }
        if estimated > 0.0 && reported > estimated * 50.0 {
            return estimated;
        }
    }

    reported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_35_flash_rates() {
        let r = rates_for_model("gemini-3.5-flash-medium");
        assert!((r.input - 1.50).abs() < 1e-9);
        assert!((r.output - 9.00).abs() < 1e-9);
    }

    #[test]
    fn gemini_36_and_newer_flash_is_cheaper_than_35() {
        // 3.6/3.7/3.8 Flash launched at $0.75/$3.75, below 3.5's $1.50/$9.00.
        for id in ["gemini-3.6-flash", "gemini-3.7-flash", "gemini-3.8-flash"] {
            let r = rates_for_model(id);
            assert!((r.input - 0.75).abs() < 1e-9, "{id} input");
            assert!((r.output - 3.75).abs() < 1e-9, "{id} output");
            assert!((r.cache_read - 0.075).abs() < 1e-9, "{id} cache read");
        }
    }

    #[test]
    fn gemini_pro_is_priced_above_flash() {
        // Pro was previously falling through to the Flash default, understating
        // cost by roughly 4x on output.
        let p31 = rates_for_model("gemini-3.1-pro-preview");
        assert!((p31.input - 2.00).abs() < 1e-9);
        assert!((p31.output - 12.00).abs() < 1e-9);

        let p25 = rates_for_model("gemini-2.5-pro");
        assert!((p25.input - 1.25).abs() < 1e-9);
        assert!((p25.output - 10.00).abs() < 1e-9);
    }

    #[test]
    fn gemini_flash_lite_is_its_own_tier() {
        let l35 = rates_for_model("gemini-3.5-flash-lite");
        assert!((l35.input - 0.30).abs() < 1e-9);
        assert!((l35.output - 2.50).abs() < 1e-9);

        let l31 = rates_for_model("gemini-3.1-flash-lite");
        assert!((l31.input - 0.25).abs() < 1e-9);
        assert!((l31.output - 1.50).abs() < 1e-9);
    }

    #[test]
    fn gemini_2_5_flash_unchanged() {
        let r = rates_for_model("gemini-2.5-flash");
        assert!((r.input - 0.30).abs() < 1e-9);
        assert!((r.output - 2.50).abs() < 1e-9);
    }

    #[test]
    fn gemini_tier_ignores_trailing_release_dates() {
        assert_eq!(
            gemini_tier(&normalize("gemini-2.5-flash-001")),
            Some((GeminiTier::Flash, 2, 5))
        );
        assert_eq!(
            gemini_tier(&normalize("gemini-3-5-pro-preview")),
            Some((GeminiTier::Pro, 3, 5))
        );
        assert_eq!(gemini_tier(&normalize("flash")), None);
    }

    #[test]
    fn claude_opus_display_name() {
        // Opus 4.6 is $5/$25, not the $15/$75 of Opus 4.1.
        let r = rates_for_model("Claude Opus 4.6 (Thinking)");
        assert!((r.input - 5.00).abs() < 1e-9);
        assert!((r.output - 25.00).abs() < 1e-9);
        assert!((r.cache_read - 0.50).abs() < 1e-9);
    }

    #[test]
    fn claude_opus_4_1_keeps_the_retired_expensive_rates() {
        // Opus 4.1 and Opus 4 are still sold on Bedrock and Google Cloud.
        for id in ["claude-opus-4-1", "claude-opus-4.1", "claude-opus-4-20250514"] {
            let r = rates_for_model(id);
            assert!((r.input - 15.00).abs() < 1e-9, "{id} should be $15/MTok");
            assert!((r.output - 75.00).abs() < 1e-9, "{id} should be $75/MTok");
        }
    }

    #[test]
    fn claude_opus_4_5_and_5_share_the_lower_rates() {
        for id in [
            "claude-opus-4-5-20260101",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-opus-5-5",
        ] {
            let r = rates_for_model(id);
            assert!((r.input - 5.00).abs() < 1e-9, "{id} should be $5/MTok");
            assert!((r.output - 25.00).abs() < 1e-9, "{id} should be $25/MTok");
        }
    }

    #[test]
    fn claude_sonnet_5_is_cheaper_than_sonnet_4() {
        // The $2/$10 launch rate became standard; the scheduled increase to
        // $3/$15 on 2026-09-01 did not happen.
        let s5 = rates_for_model("claude-sonnet-5");
        assert!((s5.input - 2.00).abs() < 1e-9);
        assert!((s5.output - 10.00).abs() < 1e-9);
        assert!((s5.cache_read - 0.20).abs() < 1e-9);
        let s55 = rates_for_model("claude-sonnet-5-5");
        assert!((s55.input - 2.00).abs() < 1e-9);

        let s4 = rates_for_model("claude-sonnet-4-5-20250929");
        assert!((s4.input - 3.00).abs() < 1e-9);
        assert!((s4.output - 15.00).abs() < 1e-9);
        let s46 = rates_for_model("claude-sonnet-4-6");
        assert!((s46.input - 3.00).abs() < 1e-9);
    }

    #[test]
    fn claude_haiku_4_5_is_more_expensive_than_3_5() {
        let h45 = rates_for_model("claude-haiku-4-5");
        assert!((h45.input - 1.00).abs() < 1e-9);
        assert!((h45.output - 5.00).abs() < 1e-9);
        assert!((h45.cache_read - 0.10).abs() < 1e-9);

        let h35 = rates_for_model("claude-3-5-haiku-20241022");
        assert!((h35.input - 0.80).abs() < 1e-9);
        assert!((h35.output - 4.00).abs() < 1e-9);
    }

    #[test]
    fn unversioned_claude_falls_to_current_sonnet() {
        let r = rates_for_model("claude");
        assert!((r.input - 2.00).abs() < 1e-9);
    }

    #[test]
    fn version_reads_dotted_hyphenated_and_dated_ids() {
        // Dotted and hyphenated spellings agree.
        assert_eq!(version(&normalize("claude-opus-4.1"), "opus"), Some((4, 1)));
        assert_eq!(version(&normalize("claude-opus-4-1"), "opus"), Some((4, 1)));
        // A trailing date must not be read as the minor version.
        assert_eq!(version(&normalize("claude-opus-4-5-20260101"), "opus"), Some((4, 5)));
        assert_eq!(version(&normalize("claude-sonnet-4-5-20250929"), "sonnet"), Some((4, 5)));
        assert_eq!(version(&normalize("claude-opus-5-20260101"), "opus"), Some((5, 0)));
        // A major with no minor.
        assert_eq!(version(&normalize("claude-sonnet-5"), "sonnet"), Some((5, 0)));
        assert_eq!(version(&normalize("claude-opus-4-20250514"), "opus"), Some((4, 0)));
        // Only a date, so no version at all.
        assert_eq!(version(&normalize("claude-3-5-haiku-20241022"), "haiku"), None);
        assert_eq!(version(&normalize("claude-opus"), "opus"), None);
    }

    #[test]
    fn free_models_zero() {
        assert_eq!(estimate_model_cost("openrouter/free", 1_000_000, 0, 0, 1_000_000), 0.0);
        assert_eq!(estimate_model_cost("openai/gpt-oss-20b", 1_000_000, 0, 0, 0), 0.0);
    }

    #[test]
    fn estimate_gemini_cost() {
        // 1M in + 10M cache_read + 0.1M out @ 1.50 / 0.15 / 9
        let c = estimate_model_cost("gemini-3.5-flash-medium", 1_000_000, 10_000_000, 0, 100_000);
        assert!((c - (1.50 + 1.50 + 0.90)).abs() < 0.02);
    }

    #[test]
    fn resolve_rejects_absurd_hermes_estimate() {
        // 1M tokens total, reported $50k → insane
        let c = resolve_reported_cost("gemini-3-flash", 50_000.0, 500_000, 400_000, 0, 100_000);
        assert!(c < 100.0, "should fall back to estimate, got {c}");
    }

    #[test]
    fn resolve_keeps_sane_reported() {
        let c = resolve_reported_cost("@preset/hermes-agent", 1.25, 500_000, 0, 0, 100_000);
        assert!((c - 1.25).abs() < 1e-9);
    }

    #[test]
    fn small_reported_cost_is_kept_despite_a_high_per_token_ratio() {
        // $0.25 on 150 tokens is $1667/1M — a high ratio that a naive guard
        // reads as corruption. It is just a real cost on a tiny sample, and
        // rejecting it silently zeroed a day of spend.
        let c = resolve_reported_cost("anthropic/claude-sonnet-4-5", 0.25, 100, 30, 0, 20);
        assert!((c - 0.25).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn large_reported_cost_still_rejected_when_per_token_rate_is_impossible() {
        // The same ratio on real dollars is still corruption and must fall back.
        let c = resolve_reported_cost("claude-sonnet-4-5", 500.0, 1_000, 0, 0, 0);
        let estimated = estimate_model_cost("claude-sonnet-4-5", 1_000, 0, 0, 0);
        assert!(
            (c - estimated).abs() < 1e-9,
            "expected fallback to estimate {estimated}, got {c}"
        );
    }

    #[test]
    fn per_record_costs_are_not_rounded_to_cents() {
        // Sources sum many per-turn costs into one row. Rounding each turn to
        // cents first rounds $0.0006 turns to $0.00 and loses real money.
        let turn = estimate_model_cost("claude-sonnet-4-5", 1, 0, 0, 1);
        assert!(
            turn > 0.0 && turn < 0.01,
            "a single-token turn must keep sub-cent precision, got {turn}"
        );
        let hundred = estimate_model_cost("claude-sonnet-4-5", 100, 0, 0, 100);
        assert!(hundred > 0.0, "100 turns must not sum to zero");
    }

    #[test]
    fn gateway_prefixed_model_names_still_resolve() {
        // fx logs models as "provider/model". The prefix must not push the
        // lookup into the free tier.
        let r = rates_for_model("anthropic/claude-sonnet-4-5");
        assert!(r.input > 0.0 && r.output > 0.0);
        let g = rates_for_model("openai/gpt-5.5");
        assert!(g.input > 0.0, "unknown openai models must not be free");
    }
}
