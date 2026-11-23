//! What each endpoint accepts for `reasoning_effort`, learned from its own error messages.
//!
//! Providers disagree: Groq's `allam-2-7b` rejects the parameter outright, Groq's `gpt-oss-120b` accepts
//! only low/medium/high, OpenRouter's `lfm-3` cannot turn reasoning off. Instead of keeping a hand-written
//! table, the first 400 that talks about reasoning teaches us the rule; the request is fixed and retried at
//! once, and every later request to that endpoint is adapted before it is sent.
//!
//! * not supported            -> drop `reasoning_effort`
//! * only some values allowed -> clamp to the nearest allowed one (`none` becomes the lowest, usually `low`)
//! * "mandatory" / "cannot be disabled" -> allowed = low, medium, high

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// From the least to the most reasoning.
const ORDER: [&str; 5] = ["none", "minimal", "low", "medium", "high"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "allowed", rename_all = "snake_case")]
pub enum Effort {
    /// The endpoint rejects the parameter: leave it out.
    Unsupported,
    /// The endpoint accepts only these values, listed from lowest to highest.
    Only(Vec<String>),
}

fn rank(v: &str) -> Option<usize> {
    ORDER.iter().position(|o| *o == v)
}

/// Reads a rule out of an upstream error message. `None` when the message is not about reasoning.
pub fn learn(message: &str) -> Option<Effort> {
    let m = message.to_lowercase();
    if !m.contains("reasoning") {
        return None;
    }
    if m.contains("mandatory") || m.contains("cannot be disabled") || m.contains("cannot disable") || m.contains("can't be disabled") {
        return Some(Effort::Only(vec!["low".into(), "medium".into(), "high".into()]));
    }
    static WORDS: OnceLock<Regex> = OnceLock::new();
    let words = WORDS.get_or_init(|| Regex::new(r"\b(none|minimal|low|medium|high)\b").unwrap());
    for marker in ["one of", "valid values", "supported values", "allowed values", "expected"] {
        if let Some(at) = m.find(marker) {
            let mut found: Vec<String> = words.find_iter(&m[at..]).map(|w| w.as_str().to_string()).collect();
            found.sort_by_key(|w| rank(w));
            found.dedup();
            if !found.is_empty() {
                return Some(Effort::Only(found));
            }
        }
    }
    const NO: [&str; 7] = ["not supported", "unsupported", "does not support", "doesn't support", "unknown parameter", "unrecognized", "extra inputs"];
    NO.iter().any(|p| m.contains(p)).then_some(Effort::Unsupported)
}

/// Rewrites `reasoning_effort` in `body` to what the endpoint accepts. True when the body changed.
pub fn adapt(body: &mut Value, rule: &Effort) -> bool {
    let Some(current) = body.get("reasoning_effort").and_then(Value::as_str).map(str::to_string) else {
        return false;
    };
    match rule {
        Effort::Unsupported => {
            body.as_object_mut().map(|o| o.remove("reasoning_effort"));
            true
        }
        Effort::Only(allowed) => {
            if allowed.iter().any(|a| *a == current) {
                return false;
            }
            let Some(want) = rank(&current) else { return false };
            let nearest = allowed
                .iter()
                .filter_map(|a| rank(a).map(|r| (r.abs_diff(want), r, a)))
                .min_by_key(|(distance, r, _)| (*distance, *r));
            match nearest {
                Some((_, _, v)) => {
                    body["reasoning_effort"] = json!(v);
                    true
                }
                None => false,
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Row {
    provider: String,
    endpoint: String,
    #[serde(flatten)]
    rule: Effort,
}

/// Rules per (provider, upstream model id), optionally saved to a JSON file so a restart keeps them.
#[derive(Default)]
pub struct Capabilities {
    rules: Mutex<BTreeMap<(String, String), Effort>>,
    path: Option<PathBuf>,
}

impl Capabilities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut rules = BTreeMap::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Vec<Row>>(&text) {
                Ok(rows) => rules.extend(rows.into_iter().map(|r| ((r.provider, r.endpoint), r.rule))),
                Err(e) => tracing::warn!("could not read {} ({e}): starting with no capabilities", path.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("could not read {} ({e}): starting with no capabilities", path.display()),
        }
        Self { rules: Mutex::new(rules), path: Some(path) }
    }

    pub fn get(&self, provider: &str, endpoint: &str) -> Option<Effort> {
        self.rules.lock().unwrap().get(&(provider.to_string(), endpoint.to_string())).cloned()
    }

    /// Fixes `body` with the rule already known for this endpoint, if any. True when it changed.
    pub fn apply(&self, provider: &str, endpoint: &str, body: &mut Value) -> bool {
        self.get(provider, endpoint).is_some_and(|rule| adapt(body, &rule))
    }

    /// Learns from a 400 message. True when it taught something new about this endpoint.
    pub fn learn_from(&self, provider: &str, endpoint: &str, message: &str) -> bool {
        let Some(rule) = learn(message) else { return false };
        let mut rules = self.rules.lock().unwrap();
        let key = (provider.to_string(), endpoint.to_string());
        if rules.get(&key) == Some(&rule) {
            return false;
        }
        rules.insert(key, rule);
        self.write(&rules);
        true
    }

    fn write(&self, rules: &BTreeMap<(String, String), Effort>) {
        let Some(path) = &self.path else { return };
        let rows: Vec<Row> = rules.iter().map(|((p, e), r)| Row { provider: p.clone(), endpoint: e.clone(), rule: r.clone() }).collect();
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&rows)?)?;
            std::fs::rename(tmp, path)
        })();
        if let Err(e) = result {
            tracing::warn!("could not write {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(v: &[&str]) -> Option<Effort> {
        Some(Effort::Only(v.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn learns_the_messages_seen_in_production() {
        assert_eq!(learn("`reasoning_effort` is not supported with this model"), Some(Effort::Unsupported));
        assert_eq!(learn("`reasoning_effort` must be one of `low`, `medium`, or `high`"), only(&["low", "medium", "high"]));
        assert_eq!(learn("Reasoning is mandatory for this endpoint and cannot be disabled."), only(&["low", "medium", "high"]));
        assert_eq!(learn("Function call is missing a thought_signature"), None);
        assert_eq!(learn("high demand, try again later"), None);
    }

    #[test]
    fn none_becomes_the_lowest_allowed_value() {
        let mut b = json!({ "reasoning_effort": "none" });
        assert!(adapt(&mut b, &Effort::Only(vec!["low".into(), "medium".into(), "high".into()])));
        assert_eq!(b["reasoning_effort"], "low");
    }

    #[test]
    fn allowed_values_are_left_alone_and_high_is_clamped() {
        let mut b = json!({ "reasoning_effort": "medium" });
        assert!(!adapt(&mut b, &Effort::Only(vec!["low".into(), "medium".into()])));
        let mut b = json!({ "reasoning_effort": "high" });
        assert!(adapt(&mut b, &Effort::Only(vec!["low".into(), "medium".into()])));
        assert_eq!(b["reasoning_effort"], "medium");
    }

    #[test]
    fn unsupported_drops_the_parameter_and_ignores_requests_without_it() {
        let mut b = json!({ "model": "m", "reasoning_effort": "low" });
        assert!(adapt(&mut b, &Effort::Unsupported));
        assert!(b.get("reasoning_effort").is_none());
        assert!(!adapt(&mut b, &Effort::Unsupported));
    }

    #[test]
    fn rules_are_remembered_per_endpoint_and_survive_a_restart() {
        let path = std::env::temp_dir().join(format!("freeway-caps-{}.json", std::process::id()));
        let c = Capabilities::with_file(&path);
        assert!(c.learn_from("groq", "allam-2-7b", "`reasoning_effort` is not supported with this model"));
        assert!(!c.learn_from("groq", "allam-2-7b", "`reasoning_effort` is not supported with this model"), "not new twice");
        assert!(!c.learn_from("groq", "x", "overloaded"));
        let mut b = json!({ "reasoning_effort": "none" });
        assert!(c.apply("groq", "allam-2-7b", &mut b));
        assert!(!c.apply("groq", "other", &mut json!({ "reasoning_effort": "none" })));
        assert_eq!(Capabilities::with_file(&path).get("groq", "allam-2-7b"), Some(Effort::Unsupported));
        let _ = std::fs::remove_file(path);
    }
}
