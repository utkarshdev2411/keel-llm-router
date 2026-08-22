//! Prompt token counting.
//!
//! The router's token count is the numerator of every KV projection, so it must
//! match whatever the *backend* counts. It is not a free choice of heuristic.
//!
//! This was learned the expensive way. A `chars / 4` heuristic ran against
//! `llm-d-inference-sim`, which counts whitespace-separated words. On the Phase 0
//! trace that heuristic over-counted by a consistent 1.75x, which silently turned
//! a configured `sigma = 0.95` into an effective 0.54 and left 46% of every
//! backend's KV capacity unused. Nothing failed; the numbers just meant something
//! other than what they said.
//!
//! Hence two rules:
//!   1. The counting mode is configuration, not a constant, and it names the thing
//!      it is imitating.
//!   2. Whatever is configured is checked at runtime against the backend's own
//!      reported `usage.prompt_tokens`. See `observe::record_prompt_token_ratio`.

use serde::Deserialize;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCounterKind {
    /// Split on whitespace. Exact for `llm-d-inference-sim`, which tokenizes by
    /// word, and a reasonable floor for anything else.
    Whitespace,
    /// `ceil(bytes / chars_per_token)`. A heuristic, and only correct when the
    /// divisor has been calibrated against the backend actually in use.
    CharsPerToken,
}

/// Counts prompt tokens the way the configured backend does.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TokenCounter {
    kind: TokenCounterKind,
    chars_per_token: f64,
}

impl TokenCounter {
    /// `chars_per_token` must be finite and > 0; it is validated at config load
    /// and ignored entirely under `Whitespace`.
    pub fn new(kind: TokenCounterKind, chars_per_token: f64) -> Self {
        debug_assert!(
            chars_per_token.is_finite() && chars_per_token > 0.0,
            "chars_per_token must be validated before constructing a TokenCounter"
        );
        Self { kind, chars_per_token }
    }

    pub fn kind(&self) -> TokenCounterKind {
        self.kind
    }

    /// True when this mode reproduces the backend's count exactly rather than
    /// approximating it. Surfaced on `RequestFeatures::prompt_tokens_exact`.
    pub fn is_exact(&self) -> bool {
        matches!(self.kind, TokenCounterKind::Whitespace)
    }

    /// Never returns 0 for non-empty input: a zero prompt would make the KV
    /// projection zero and the backend look free. Empty input returns 0 so the
    /// caller can reject it as a malformed request.
    pub fn count(&self, text: &str) -> u32 {
        if text.trim().is_empty() {
            return 0;
        }
        let n = match self.kind {
            TokenCounterKind::Whitespace => text.split_whitespace().count() as u64,
            TokenCounterKind::CharsPerToken => {
                // `len()` is bytes, matching the original heuristic. Saturating
                // rather than `as u32` so a pathological prompt clamps instead of
                // wrapping to a small number and reading as nearly free.
                (text.len() as f64 / self.chars_per_token).ceil() as u64
            }
        };
        n.clamp(1, u32::MAX as u64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> TokenCounter {
        TokenCounter::new(TokenCounterKind::Whitespace, 4.0)
    }

    /// The trace's prompts are single-space-joined words, and the simulator
    /// reports exactly that count. Verified against all 1500 records of the
    /// Phase 0 trace: zero mismatches.
    #[test]
    fn whitespace_matches_the_simulators_word_count() {
        assert_eq!(ws().count("tmhuug tfcnpo rtqlye fmssrw"), 4);
        assert_eq!(ws().count("one"), 1);
    }

    #[test]
    fn whitespace_is_robust_to_irregular_spacing() {
        assert_eq!(ws().count("  a   b \n c \t d  "), 4);
    }

    #[test]
    fn empty_and_blank_count_as_zero_so_the_caller_can_reject() {
        assert_eq!(ws().count(""), 0);
        assert_eq!(ws().count("   \n\t "), 0);
        assert_eq!(TokenCounter::new(TokenCounterKind::CharsPerToken, 4.0).count(""), 0);
    }

    #[test]
    fn chars_per_token_divides_and_rounds_up() {
        let c = TokenCounter::new(TokenCounterKind::CharsPerToken, 4.0);
        assert_eq!(c.count("abcd"), 1);
        assert_eq!(c.count("abcde"), 2);
    }

    /// Non-empty input must never count as zero: a zero projection makes a loaded
    /// backend look empty to the scorer.
    #[test]
    fn non_empty_input_never_counts_as_zero() {
        let c = TokenCounter::new(TokenCounterKind::CharsPerToken, 1000.0);
        assert_eq!(c.count("x"), 1);
    }

    /// The exact bug this module exists to prevent, pinned as a test.
    #[test]
    fn chars_per_four_overcounts_word_tokens_by_one_point_seven_five() {
        // Six-character words joined by single spaces: 7 bytes per token.
        let prompt = vec!["abcdef"; 100].join(" ");
        let exact = ws().count(&prompt);
        let heuristic = TokenCounter::new(TokenCounterKind::CharsPerToken, 4.0).count(&prompt);
        assert_eq!(exact, 100);
        let ratio = heuristic as f64 / exact as f64;
        assert!(
            (ratio - 1.75).abs() < 0.01,
            "expected the documented 1.75x inflation, got {ratio}"
        );
    }

    #[test]
    fn only_whitespace_reports_itself_as_exact() {
        assert!(ws().is_exact());
        assert!(!TokenCounter::new(TokenCounterKind::CharsPerToken, 4.0).is_exact());
    }
}
