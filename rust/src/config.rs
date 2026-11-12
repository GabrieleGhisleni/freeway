use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Public listing; only models priced at 0 are kept.
    Openrouter,
    /// OpenCode Zen listing; only models priced at 0 are kept.
    Opencode,
    /// Plain `GET {base_url}/models`; every listed model counts as free.
    #[default]
    Openai,
    /// Z.ai pricing page (`models_url`, Markdown); keeps models whose prices are all "Free".
    Zai,
}

fn d_priority() -> i64 {
    100
}
fn d_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    pub base_url: String,
    #[serde(default)]
    pub models_url: Option<String>,
    pub api_key_env: String,
    /// Lower is tried first when the same model is served by several providers.
    #[serde(default = "d_priority")]
    pub priority: i64,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Static list; skips discovery.
    #[serde(default)]
    pub models: Vec<String>,
    /// For listings that include dead or non-chat models (NVIDIA lists 83, about 10 answer):
    /// send each listed model a 1-token chat and keep only those that answer.
    #[serde(default)]
    pub probe: bool,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
}

impl ProviderConfig {
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.api_key_env).ok().filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TierConfig {
    pub name: String,
    pub min_score: f64,
    /// Tiers tried, in order, when nothing in this tier can answer.
    #[serde(default)]
    pub fallback: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelOverride {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScoresConfig {
    #[serde(default = "d_scores_key")]
    pub api_key_env: String,
    #[serde(default = "d_scores_hours")]
    pub refresh_hours: f64,
    #[serde(default = "d_scores_cache")]
    pub cache_file: String,
    /// Override of the Artificial Analysis endpoint (used by tests).
    #[serde(default)]
    pub url: Option<String>,
}

fn d_scores_key() -> String {
    "ARTIFICIAL_ANALYSIS_API_KEY".into()
}
fn d_scores_hours() -> f64 {
    24.0
}
fn d_scores_cache() -> String {
    "data/scores.json".into()
}

impl Default for ScoresConfig {
    fn default() -> Self {
        Self { api_key_env: d_scores_key(), refresh_hours: d_scores_hours(), cache_file: d_scores_cache(), url: None }
    }
}

impl ScoresConfig {
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.api_key_env).ok().filter(|v| !v.is_empty())
    }
}

fn d_host() -> String {
    "127.0.0.1".into()
}
fn d_port() -> u16 {
    4100
}
fn d_refresh() -> f64 {
    30.0
}
fn d_connect() -> f64 {
    10.0
}
fn d_request() -> f64 {
    120.0
}
fn d_first() -> f64 {
    30.0
}
fn d_idle() -> f64 {
    90.0
}
fn d_attempts() -> usize {
    8
}
fn d_session() -> f64 {
    120.0
}
fn d_scores() -> Option<ScoresConfig> {
    Some(ScoresConfig::default())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    #[serde(default = "d_host")]
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default = "d_refresh")]
    pub refresh_minutes: f64,
    #[serde(default = "d_connect")]
    pub connect_timeout: f64,
    /// Non-streaming: whole response.
    #[serde(default = "d_request")]
    pub request_timeout: f64,
    /// Streaming: time allowed before the first chunk.
    #[serde(default = "d_first")]
    pub first_token_timeout: f64,
    /// Streaming: max silence between chunks.
    #[serde(default = "d_idle")]
    pub stream_idle_timeout: f64,
    #[serde(default = "d_attempts")]
    pub max_attempts: usize,
    #[serde(default = "d_session")]
    pub session_ttl_minutes: f64,
    #[serde(default = "d_scores")]
    pub scores: Option<ScoresConfig>,
    pub tiers: Vec<TierConfig>,
    pub providers: Vec<ProviderConfig>,
    /// Canonical name -> other names that must be treated as the same model.
    #[serde(default)]
    pub aliases: BTreeMap<String, Vec<String>>,
    /// Canonical name (or any alias) -> manual tier/score/disable.
    #[serde(default)]
    pub models: BTreeMap<String, ModelOverride>,
    /// Globs on canonical names.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Settings {
    pub fn from_yaml(text: &str) -> Result<Settings, Error> {
        let s: Settings = serde_yaml::from_str(text)?;
        s.validate()?;
        Ok(s)
    }

    fn validate(&self) -> Result<(), Error> {
        let names: Vec<&str> = self.tiers.iter().map(|t| t.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != names.len() {
            return Err("duplicate tier names".into());
        }
        for t in &self.tiers {
            for f in &t.fallback {
                if !names.contains(&f.as_str()) {
                    return Err(format!("tier {:?} falls back to unknown tier {:?}", t.name, f).into());
                }
            }
        }
        for (key, ov) in &self.models {
            if let Some(t) = &ov.tier
                && !names.contains(&t.as_str()) {
                    return Err(format!("model override {key:?} uses unknown tier {t:?}").into());
                }
        }
        let mut pnames: Vec<&str> = self.providers.iter().map(|p| p.name.as_str()).collect();
        let n = pnames.len();
        pnames.sort();
        pnames.dedup();
        if pnames.len() != n {
            return Err("duplicate provider names".into());
        }
        Ok(())
    }

    pub fn tier(&self, name: &str) -> Option<&TierConfig> {
        self.tiers.iter().find(|t| t.name == name)
    }

    /// The tier with the highest threshold the score reaches.
    pub fn tier_for_score(&self, score: f64) -> Option<String> {
        let mut ranked: Vec<&TierConfig> = self.tiers.iter().collect();
        ranked.sort_by(|a, b| b.min_score.total_cmp(&a.min_score));
        ranked.into_iter().find(|t| score >= t.min_score).map(|t| t.name.clone())
    }
}

/// Reads the config from `path`, else `$ROUTER_CONFIG`, else `./config.yaml`.
pub fn load_settings(path: Option<&str>) -> Result<Settings, Error> {
    let path: PathBuf = path
        .map(PathBuf::from)
        .or_else(|| std::env::var("ROUTER_CONFIG").ok().filter(|v| !v.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("config.yaml"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Settings::from_yaml(&text).map_err(|e| format!("{}: {e}", path.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "
tiers:
  - { name: smart, min_score: 20, fallback: [fast] }
  - { name: fast, min_score: 0 }
providers:
  - { name: a, base_url: 'http://a/v1', api_key_env: KEY_A }
";

    #[test]
    fn defaults_and_tier_for_score() {
        let s = Settings::from_yaml(BASE).unwrap();
        assert_eq!(s.port, 4100);
        assert!(s.scores.is_some());
        assert_eq!(s.providers[0].kind, Kind::Openai);
        assert_eq!(s.providers[0].priority, 100);
        assert_eq!(s.tier_for_score(25.0).as_deref(), Some("smart"));
        assert_eq!(s.tier_for_score(20.0).as_deref(), Some("smart"));
        assert_eq!(s.tier_for_score(19.9).as_deref(), Some("fast"));
        assert_eq!(s.tier_for_score(-1.0), None);
    }

    #[test]
    fn scores_can_be_switched_off() {
        let s = Settings::from_yaml(&format!("scores: null\n{BASE}")).unwrap();
        assert!(s.scores.is_none());
    }

    #[test]
    fn rejects_bad_configs() {
        let bad = BASE.replace("[fast]", "[nope]");
        assert!(Settings::from_yaml(&bad).is_err());
        let dup = format!("{BASE}  - {{ name: a, base_url: x, api_key_env: K }}\n");
        assert!(Settings::from_yaml(&dup).is_err());
        let ov = format!("models:\n  m: {{ tier: nope }}\n{BASE}");
        assert!(Settings::from_yaml(&ov).is_err());
    }

    #[test]
    fn the_real_config_file_parses() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../config.yaml")).unwrap();
        let s = Settings::from_yaml(&text).unwrap();
        assert!(s.providers.iter().any(|p| p.name == "google"));
        assert!(s.models["inkling-2-small"].disabled);
        assert_eq!(s.models["gemini-3.7-flash"].score, Some(21.0));
    }
}
