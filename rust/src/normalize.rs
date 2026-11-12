//! Turning provider-specific model ids into comparable names.
//!
//! Every provider names the same weights differently
//! (`nvidia/nemotron-3.5-super-vl:free`, `nemotron-3.5-super-vl-free`,
//! `nvidia/nemotron-3.5-super-vl-20261015`). `key()` reduces each of those to
//! `nemotron-3.5-super-vl`; two listings are the same model when they share at
//! least one key. Anything the heuristics cannot join goes under `aliases:` in
//! config.yaml.

use regex::Regex;
use std::collections::BTreeSet;
use std::sync::LazyLock;

/// Trailing tokens that describe packaging, not the model.
const NOISE: &[&str] = &["free", "bf16", "fp8", "fp16", "fp32", "gguf", "awq", "gptq", "int4", "int8", "hf", "latest"];
/// Trailing tokens that describe how a benchmark ran the model (reasoning effort).
const VARIANT: &[&str] =
    &["reasoning", "non", "thinking", "adaptive", "default", "fallback", "minimal", "low", "medium", "high", "xhigh", "max"];

static SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[_\s]+").unwrap());
static DASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-{2,}").unwrap());
static DATE_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:\d{4}|\d{6}|\d{8})$").unwrap());
static ISO_DATE_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{4}-\d{2}-\d{2}$").unwrap());
static NOT_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]").unwrap());

/// Comparable form of a model id. Empty string when nothing is left.
pub fn key(model_id: &str) -> String {
    let mut s = model_id.trim().to_lowercase();
    if s.is_empty() {
        return String::new();
    }
    s = s.replace(":free", "").replace(':', "-");
    if s.contains('/') {
        // The vendor prefix is dropped: listings disagree on whether it is there.
        match s.split('/').rfind(|p| !p.is_empty()) {
            Some(last) => s = last.to_string(),
            None => return String::new(),
        }
    }
    let s = SEPARATORS.replace_all(&s, "-");
    let s = DASHES.replace_all(&s, "-");
    let s = s.trim_matches('-');
    let s = ISO_DATE_TAIL.replace(s, "");
    let mut tokens: Vec<&str> = s.split('-').collect();
    while tokens.len() > 1 {
        let last = tokens[tokens.len() - 1];
        if NOISE.contains(&last) || DATE_TOKEN.is_match(last) {
            tokens.pop();
        } else {
            break;
        }
    }
    tokens.join("-")
}

pub fn keys<'a>(ids: impl IntoIterator<Item = Option<&'a str>>) -> BTreeSet<String> {
    ids.into_iter().flatten().map(key).filter(|k| !k.is_empty()).collect()
}

/// Looser key used to look a model up in benchmark tables.
///
/// Benchmarks write `gemini-3-5-flash-high` where providers write
/// `gemini-3.5-flash`: drop reasoning-effort suffixes and every punctuation mark.
pub fn family_key(model_id: &str) -> String {
    let k = key(model_id);
    let mut tokens: Vec<&str> = k.split('-').collect();
    while tokens.len() > 1 {
        let last = tokens[tokens.len() - 1];
        if VARIANT.contains(&last) || DATE_TOKEN.is_match(last) {
            tokens.pop();
        } else {
            break;
        }
    }
    NOT_ALNUM.replace_all(&tokens.concat(), "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_strips_vendor_suffix_and_date() {
        assert_eq!(key("google/gemma-4-31b-it:free"), "gemma-4-31b-it");
        assert_eq!(key("google/gemma-4-31b-it-20260402"), "gemma-4-31b-it");
        assert_eq!(key("nemotron-3.5-lightning-free"), "nemotron-3.5-lightning");
        assert_eq!(key("models/gemini-3.8-flash"), "gemini-3.8-flash");
    }

    #[test]
    fn key_keeps_vendor_named_models_intact() {
        assert_eq!(key("deepseek-v4.1-flash-free"), "deepseek-v4.1-flash");
        assert_eq!(key("deepseek/deepseek-v4.1-flash-1017"), "deepseek-v4.1-flash");
        assert_eq!(key("nvidia/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-BF16"), "nvidia-nemotron-3.5-lightning-30b-a3b");
    }

    #[test]
    fn key_edge_cases() {
        assert_eq!(key(""), "");
        assert_eq!(key("/"), "");
        assert_eq!(key("free"), "free");
        assert_eq!(key("a__b  c"), "a-b-c");
        assert_eq!(key("model-2026-04-02"), "model");
    }

    #[test]
    fn family_key_joins_provider_and_benchmark_spellings() {
        assert_eq!(family_key("gemini-3.5-flash"), family_key("gemini-3-5-flash-high"));
        assert_eq!(family_key("gemini-3.5-flash"), "gemini35flash");
        assert_eq!(family_key("deepseek-v4.1-flash-free"), family_key("deepseek-v4-1-flash-1017-non-reasoning"));
        assert_eq!(family_key("nemotron-3-nano-omni-30b-a3b-reasoning"), family_key("nemotron-3-nano-omni-30b-a3b"));
    }

    #[test]
    fn family_key_keeps_distinct_models_distinct() {
        assert_ne!(family_key("glm-5-3-flash"), family_key("glm-5-3"));
        assert_ne!(family_key("gemma-4-31b-it"), family_key("gemma-4-26b-a4b-it"));
    }
}
