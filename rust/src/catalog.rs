//! Discovery of free models on each provider and reconciliation into one catalog.

use crate::config::{Kind, ModelOverride, ProviderConfig, Settings};
use crate::normalize;
use crate::scores::ScoreBook;
use crate::util::fnmatch;
use futures_util::future::join_all;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Listed by generic providers next to chat models, but not chat models.
const NON_CHAT: &[&str] = &["*whisper*", "*tts*", "*guard*", "*embed*", "*rerank*", "*moderation*", "*speech*"];

/// One model as served by one provider.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub provider: String,
    /// What the provider wants in the `model` field.
    pub raw_id: String,
    pub base_url: String,
    pub keys: BTreeSet<String>,
    pub priority: i64,
    pub context: Option<u64>,
    /// None = the provider does not say.
    pub tools: Option<bool>,
    pub vision: Option<bool>,
}

impl Endpoint {
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.raw_id)
    }
}

/// The same model across every provider that serves it.
#[derive(Debug, Clone)]
pub struct Model {
    pub name: String,
    pub keys: BTreeSet<String>,
    /// None = unrated: never chosen through a tier.
    pub score: Option<f64>,
    pub tier: Option<String>,
    pub endpoints: Vec<Arc<Endpoint>>,
}

fn wanted(p: &ProviderConfig, raw_id: &str) -> bool {
    if !p.include.is_empty() && !p.include.iter().any(|g| fnmatch(raw_id, g)) {
        return false;
    }
    let lower = raw_id.to_lowercase();
    !p.exclude.iter().map(String::as_str).chain(NON_CHAT.iter().copied()).any(|g| fnmatch(&lower, g))
}

fn is_zero(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.trim().parse::<f64>().map(|f| f == 0.0).unwrap_or(false),
        _ => false,
    }
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

pub fn parse_openrouter(p: &ProviderConfig, payload: &Value) -> Vec<Endpoint> {
    let mut out = Vec::new();
    for m in payload["data"].as_array().into_iter().flatten() {
        let pricing = &m["pricing"];
        if !(is_zero(&pricing["prompt"]) && is_zero(&pricing["completion"])) {
            continue;
        }
        let arch = &m["architecture"];
        let outputs = strings(&arch["output_modalities"]);
        if !outputs.is_empty() && outputs != ["text"] {
            continue; // image / audio / music generators are not chat models
        }
        let Some(id) = m["id"].as_str() else { continue };
        let params = &m["supported_parameters"];
        out.push(Endpoint {
            provider: p.name.clone(),
            raw_id: id.to_string(),
            base_url: p.base_url.clone(),
            keys: normalize::keys([Some(id), m["canonical_slug"].as_str(), m["hugging_face_id"].as_str()]),
            priority: p.priority,
            context: m["context_length"].as_u64(),
            tools: params.as_array().map(|a| a.iter().any(|x| x == "tools")),
            vision: Some(strings(&arch["input_modalities"]).contains(&"image")),
        });
    }
    out
}

pub fn parse_opencode(p: &ProviderConfig, payload: &Value) -> Vec<Endpoint> {
    let mut out = Vec::new();
    for m in payload["data"].as_array().into_iter().flatten() {
        let cost = &m["cost"]["text"];
        if !(is_zero(&cost["input"]["price"]) && is_zero(&cost["output"]["price"])) {
            continue;
        }
        if m["type"].as_str().unwrap_or("chat") != "chat" {
            continue;
        }
        let api = &m["api"];
        let chat = &api["chat-completions"];
        if crate::util::truthy(Some(api)) && !crate::util::truthy(Some(chat)) {
            continue; // only reachable through a non-OpenAI wire format
        }
        let Some(id) = m["id"].as_str() else { continue };
        out.push(Endpoint {
            provider: p.name.clone(),
            raw_id: id.to_string(),
            base_url: chat["base_url"].as_str().unwrap_or(&p.base_url).to_string(),
            keys: normalize::keys([Some(id), m["canonical_id"].as_str()]),
            priority: p.priority,
            context: m["limit"]["context"].as_u64(),
            tools: m["capabilities"]["tools"]["supported"].as_bool(),
            vision: Some(strings(&m["modalities"]["input"]).iter().any(|i| i.starts_with("image"))),
        });
    }
    out
}

pub fn parse_openai(p: &ProviderConfig, payload: &Value) -> Vec<Endpoint> {
    let mut out = Vec::new();
    for m in payload["data"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str().filter(|s| !s.is_empty()) else { continue };
        if m["active"].as_bool() == Some(false) {
            continue;
        }
        // Google lists "models/gemini-..." but wants the bare name in requests.
        let raw_id = id.strip_prefix("models/").unwrap_or(id);
        out.push(Endpoint {
            provider: p.name.clone(),
            raw_id: raw_id.to_string(),
            base_url: p.base_url.clone(),
            keys: normalize::keys([Some(raw_id)]),
            priority: p.priority,
            context: m["context_window"].as_u64().or_else(|| m["context_length"].as_u64()),
            tools: None,
            vision: None,
        });
    }
    out
}

pub const ZAI_PRICING_URL: &str = "https://docs.z.ai/guides/overview/pricing.md";

fn cells(line: &str) -> Vec<String> {
    line.trim().trim_matches('|').split('|').map(|c| c.trim().replace(['\\', '*'], "")).collect()
}

/// Z.ai lists neither prices nor its free models in /models (the free `-flash` ones are
/// missing and every listed one needs a balance), so read the public pricing page: a
/// token-priced model is free when every price cell says "Free". "Limited-time Free" on a
/// paid model is only its cache storage, so it does not count.
pub fn parse_zai(p: &ProviderConfig, text: &str) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut priced = false; // inside a table whose columns are Model | Input | ...
    for line in text.lines() {
        let line = line.trim_start_matches([' ', '>']); // the cheaper models sit in a collapsed "Other Pricing" blockquote
        if line.starts_with('#') {
            heading = line.trim_start_matches('#').trim().to_lowercase();
        }
        if !line.trim_start().starts_with('|') {
            priced = false;
            continue;
        }
        let c = cells(line);
        if c[0].eq_ignore_ascii_case("model") {
            priced = c.len() > 2 && c[1].eq_ignore_ascii_case("input");
        } else if priced && c.len() > 2 && c[1..].iter().all(|x| x.eq_ignore_ascii_case("free")) {
            let raw_id = c[0].to_lowercase();
            out.push(Endpoint {
                provider: p.name.clone(),
                keys: normalize::keys([Some(raw_id.as_str())]),
                raw_id,
                base_url: p.base_url.clone(),
                priority: p.priority,
                context: None,
                tools: None,
                vision: Some(heading.contains("vision")),
            });
        }
    }
    out
}

struct UnionFind {
    parent: HashMap<String, String>,
}

impl UnionFind {
    fn find(&mut self, k: &str) -> String {
        self.parent.entry(k.to_string()).or_insert_with(|| k.to_string());
        let mut root = k.to_string();
        while self.parent[&root] != root {
            root = self.parent[&root].clone();
        }
        let mut cur = k.to_string();
        while cur != root {
            let next = self.parent[&cur].clone();
            self.parent.insert(cur, root.clone());
            cur = next;
        }
        root
    }

    fn union(&mut self, a: &str, b: &str) {
        let (ra, rb) = (self.find(a), self.find(b));
        self.parent.insert(ra, rb);
    }
}

/// Group endpoints that share a key into models, then rate each model.
///
/// A score comes from the benchmark table or from `models:` in the config.
/// With neither the model is unrated and belongs to no tier.
pub fn reconcile(endpoints: Vec<Arc<Endpoint>>, settings: &Settings, scores: Option<&ScoreBook>) -> BTreeMap<String, Model> {
    let mut uf = UnionFind { parent: HashMap::new() };

    let mut forced_name: HashMap<String, String> = HashMap::new(); // any alias key -> display name
    for (name, others) in &settings.aliases {
        let group: Vec<String> = std::iter::once(name.as_str())
            .chain(others.iter().map(String::as_str))
            .map(normalize::key)
            .filter(|g| !g.is_empty())
            .collect();
        let Some(first) = group.first().cloned() else { continue };
        for g in &group {
            uf.union(&first, g);
            forced_name.insert(g.clone(), name.clone());
        }
    }

    for ep in &endpoints {
        let mut it = ep.keys.iter();
        let Some(first) = it.next() else { continue };
        uf.find(first);
        for k in it {
            uf.union(first, k);
        }
    }

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Arc<Endpoint>>> = HashMap::new();
    for ep in &endpoints {
        let Some(first) = ep.keys.iter().next() else { continue };
        let root = uf.find(first);
        if !groups.contains_key(&root) {
            order.push(root.clone());
        }
        groups.entry(root).or_default().push(ep.clone());
    }

    let overrides: HashMap<String, &ModelOverride> = settings.models.iter().map(|(k, v)| (normalize::key(k), v)).collect();
    let mut models = BTreeMap::new();
    for root in order {
        let mut eps = groups.remove(&root).unwrap_or_default();
        let all_keys: BTreeSet<String> = eps.iter().flat_map(|e| e.keys.iter().cloned()).collect();
        let name = all_keys
            .iter()
            .find_map(|k| forced_name.get(k).cloned())
            .or_else(|| all_keys.iter().min_by_key(|k| (k.len(), (*k).clone())).cloned());
        let Some(name) = name else { continue };
        if all_keys.iter().chain(std::iter::once(&name)).any(|k| settings.exclude.iter().any(|g| fnmatch(k, g))) {
            continue;
        }
        let ov = all_keys.iter().find_map(|k| overrides.get(k).copied());
        if ov.is_some_and(|o| o.disabled) {
            continue;
        }
        let mut lookup_names = all_keys.clone();
        lookup_names.insert(name.clone());
        let mut score = scores.and_then(|b| b.lookup(&lookup_names));
        let mut tier = score.and_then(|s| settings.tier_for_score(s));
        if let Some(o) = ov {
            if let Some(s) = o.score {
                score = Some(s);
                tier = settings.tier_for_score(s);
            }
            if let Some(t) = &o.tier {
                tier = Some(t.clone());
            }
        }
        eps.sort_by(|a, b| (a.priority, &a.provider, &a.raw_id).cmp(&(b.priority, &b.provider, &b.raw_id)));
        let mut keys = all_keys;
        keys.insert(normalize::key(&name));
        models.insert(name.clone(), Model { name, keys, score, tier, endpoints: eps });
    }
    models
}

#[derive(Default)]
struct State {
    models: BTreeMap<String, Arc<Model>>,
    by_key: HashMap<String, String>,
    by_provider: HashMap<String, Vec<Arc<Endpoint>>>,
    errors: BTreeMap<String, String>,
    skipped: BTreeMap<String, String>,
    active: Vec<String>,
}

pub struct Catalog {
    settings: Arc<Settings>,
    client: reqwest::Client,
    scores: Option<Arc<ScoreBook>>,
    state: RwLock<State>,
    refresh_lock: tokio::sync::Mutex<()>,
    /// `probe: true` providers: (provider, model) -> (answers a chat, verdict valid until).
    probes: Mutex<HashMap<(String, String), (bool, Instant)>>,
}

// `probe: true` providers. Few in parallel: free tiers allow about 40 requests a minute.
const PROBE_PARALLEL: usize = 2;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20); // a model that needs longer is too slow to be a fallback anyway
const PROBE_OK_TTL: Duration = Duration::from_secs(24 * 3600);
const PROBE_GONE_TTL: Duration = Duration::from_secs(6 * 3600);
const PROBE_RETRY_TTL: Duration = Duration::from_secs(10 * 60);

/// What `/admin/status` reports about providers.
pub struct ProviderStatus {
    pub active: Vec<String>,
    pub skipped: BTreeMap<String, String>,
    pub errors: BTreeMap<String, String>,
}

impl Catalog {
    pub fn new(settings: Arc<Settings>, client: reqwest::Client, scores: Option<Arc<ScoreBook>>) -> Self {
        Self {
            settings,
            client,
            scores,
            state: RwLock::new(State::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
            probes: Mutex::new(HashMap::new()),
        }
    }

    pub fn scores(&self) -> Option<&Arc<ScoreBook>> {
        self.scores.as_ref()
    }

    fn classify_providers(&self) -> (Vec<&ProviderConfig>, BTreeMap<String, String>) {
        let mut skipped = BTreeMap::new();
        let mut active = Vec::new();
        for p in &self.settings.providers {
            if !p.enabled {
                skipped.insert(p.name.clone(), "disabled in config".to_string());
            } else if p.api_key().is_none() {
                skipped.insert(p.name.clone(), format!("{} is not set", p.api_key_env));
            } else {
                active.push(p);
            }
        }
        (active, skipped)
    }

    async fn discover(&self, p: &ProviderConfig) -> Result<Vec<Endpoint>, String> {
        let eps: Vec<Endpoint> = if !p.models.is_empty() {
            p.models
                .iter()
                .map(|m| Endpoint {
                    provider: p.name.clone(),
                    raw_id: m.clone(),
                    base_url: p.base_url.clone(),
                    keys: normalize::keys([Some(m.as_str())]),
                    priority: p.priority,
                    context: None,
                    tools: None,
                    vision: None,
                })
                .collect()
        } else if p.kind == Kind::Zai {
            // A public docs page: the API key must not be sent to it.
            let url = p.models_url.clone().unwrap_or_else(|| ZAI_PRICING_URL.to_string());
            let resp = self.client.get(&url).timeout(Duration::from_secs(20)).send().await.map_err(|e| format!("request failed: {e}"))?.error_for_status().map_err(|e| e.to_string())?;
            let text = resp.text().await.map_err(|e| format!("bad body: {e}"))?;
            let eps = parse_zai(p, &text);
            if eps.is_empty() {
                // layout changed: keep the previous list instead of emptying it
                return Err("no free model found on the pricing page".into());
            }
            eps
        } else {
            let url = p.models_url.clone().unwrap_or_else(|| format!("{}/models", p.base_url.trim_end_matches('/')));
            let mut req = self.client.get(&url).timeout(Duration::from_secs(20));
            if let Some(key) = p.api_key() {
                req = req.bearer_auth(key);
            }
            for (k, v) in &p.headers {
                req = req.header(k, v);
            }
            let resp = req.send().await.map_err(|e| format!("request failed: {e}"))?.error_for_status().map_err(|e| e.to_string())?;
            let payload: Value = resp.json().await.map_err(|e| format!("bad JSON: {e}"))?;
            match p.kind {
                Kind::Openrouter => parse_openrouter(p, &payload),
                Kind::Opencode => parse_opencode(p, &payload),
                Kind::Openai => parse_openai(p, &payload),
                Kind::Zai => unreachable!("handled above"),
            }
        };
        let eps: Vec<Endpoint> = eps.into_iter().filter(|e| !e.keys.is_empty() && wanted(p, &e.raw_id)).collect();
        Ok(if p.probe { self.probe(p, eps).await } else { eps })
    }

    /// `Some(true)`: answers a chat. `Some(false)`: refuses it for good (404 gone, 400 not a
    /// chat model). `None`: inconclusive (rate limit, 5xx, timeout, network), so ask again soon.
    async fn probe_one(&self, p: &ProviderConfig, ep: &Endpoint) -> Option<bool> {
        let body = serde_json::json!({ "model": ep.raw_id, "max_tokens": 1, "messages": [{ "role": "user", "content": "hi" }] });
        let mut req = self.client.post(format!("{}/chat/completions", p.base_url.trim_end_matches('/'))).timeout(PROBE_TIMEOUT).json(&body);
        if let Some(key) = p.api_key() {
            req = req.bearer_auth(key);
        }
        for (k, v) in &p.headers {
            req = req.header(k, v);
        }
        let status = req.send().await.ok()?.status();
        if status.is_success() {
            Some(true)
        } else if matches!(status.as_u16(), 400 | 404 | 410 | 422) {
            Some(false)
        } else {
            None
        }
    }

    /// The remembered verdict for one model, or a fresh probe when there is none (or it expired).
    async fn verdict(&self, p: &ProviderConfig, ep: &Endpoint, gate: &tokio::sync::Semaphore) -> bool {
        let key = (p.name.clone(), ep.raw_id.clone());
        let known = self.probes.lock().unwrap().get(&key).copied();
        if let Some((ok, until)) = known {
            if Instant::now() < until {
                return ok;
            }
        }
        let answer = {
            let _permit = gate.acquire().await.expect("semaphore is never closed");
            self.probe_one(p, ep).await
        };
        let (ok, ttl) = match answer {
            Some(true) => (true, PROBE_OK_TTL),
            Some(false) => (false, PROBE_GONE_TTL),
            None => (known.map(|k| k.0).unwrap_or(false), PROBE_RETRY_TTL), // keep the old verdict
        };
        self.probes.lock().unwrap().insert(key, (ok, Instant::now() + ttl));
        ok
    }

    /// Keep the models that answer a real chat request. Verdicts are remembered, so a refresh
    /// only asks about models it has not seen (or whose verdict has expired).
    async fn probe(&self, p: &ProviderConfig, eps: Vec<Endpoint>) -> Vec<Endpoint> {
        let gate = tokio::sync::Semaphore::new(PROBE_PARALLEL);
        let verdicts: Vec<bool> = join_all(eps.iter().map(|ep| self.verdict(p, ep, &gate))).await;
        let total = eps.len();
        let kept: Vec<Endpoint> = eps.into_iter().zip(verdicts).filter(|(_, ok)| *ok).map(|(e, _)| e).collect();
        tracing::info!("probe {}: {} of {} listed models answer a chat", p.name, kept.len(), total);
        kept
    }

    pub async fn refresh(&self) {
        let _guard = self.refresh_lock.lock().await;
        if let Some(book) = &self.scores {
            book.ensure_fresh().await;
        }
        let (providers, skipped) = self.classify_providers();
        let results = join_all(providers.iter().map(|p| self.discover(p))).await;

        let previous = self.state.read().unwrap().by_provider.clone();
        let mut errors = BTreeMap::new();
        let mut by_provider: HashMap<String, Vec<Arc<Endpoint>>> = HashMap::new();
        for (p, res) in providers.iter().zip(results) {
            match res {
                Ok(eps) => {
                    by_provider.insert(p.name.clone(), eps.into_iter().map(Arc::new).collect());
                }
                Err(e) => {
                    tracing::warn!("discovery failed for {}: {e} (keeping previous list)", p.name);
                    errors.insert(p.name.clone(), e);
                    by_provider.insert(p.name.clone(), previous.get(&p.name).cloned().unwrap_or_default());
                }
            }
        }
        let all: Vec<Arc<Endpoint>> = by_provider.values().flatten().cloned().collect();
        let models: BTreeMap<String, Arc<Model>> =
            reconcile(all, &self.settings, self.scores.as_deref()).into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        let by_key: HashMap<String, String> =
            models.values().flat_map(|m| m.keys.iter().map(|k| (k.clone(), m.name.clone()))).collect();

        let mut counts: Vec<String> = by_provider.iter().map(|(n, e)| format!("{n}={}", e.len())).collect();
        counts.sort();
        tracing::info!("catalog: {} models from {} providers ({})", models.len(), providers.len(), counts.join(", "));

        let mut st = self.state.write().unwrap();
        *st = State {
            models,
            by_key,
            by_provider,
            errors,
            skipped,
            active: providers.iter().map(|p| p.name.clone()).collect(),
        };
    }

    pub fn provider_status(&self) -> ProviderStatus {
        let st = self.state.read().unwrap();
        ProviderStatus { active: st.active.clone(), skipped: st.skipped.clone(), errors: st.errors.clone() }
    }

    pub fn find_model(&self, requested: &str) -> Option<Arc<Model>> {
        let st = self.state.read().unwrap();
        st.models
            .get(requested)
            .or_else(|| st.by_key.get(&normalize::key(requested)).and_then(|n| st.models.get(n)))
            .cloned()
    }

    /// Match `<provider>/<provider model id>` exactly.
    pub fn find_endpoint(&self, requested: &str) -> Option<(Arc<Model>, Arc<Endpoint>)> {
        let st = self.state.read().unwrap();
        st.models
            .values()
            .find_map(|m| m.endpoints.iter().find(|e| e.reference() == requested).map(|e| (m.clone(), e.clone())))
    }

    /// Models of a tier, best score first.
    pub fn in_tier(&self, tier: &str) -> Vec<Arc<Model>> {
        let st = self.state.read().unwrap();
        let mut found: Vec<Arc<Model>> = st.models.values().filter(|m| m.tier.as_deref() == Some(tier)).cloned().collect();
        found.sort_by(|a, b| b.score.unwrap_or(0.0).total_cmp(&a.score.unwrap_or(0.0)).then_with(|| a.name.cmp(&b.name)));
        found
    }

    pub fn unrated(&self) -> Vec<Arc<Model>> {
        let st = self.state.read().unwrap();
        st.models.values().filter(|m| m.tier.is_none()).cloned().collect()
    }

    /// Every model, best score first (unrated last).
    pub fn all_models(&self) -> Vec<Arc<Model>> {
        let st = self.state.read().unwrap();
        let mut all: Vec<Arc<Model>> = st.models.values().cloned().collect();
        all.sort_by(|a, b| b.score.unwrap_or(-1.0).total_cmp(&a.score.unwrap_or(-1.0)).then_with(|| a.name.cmp(&b.name)));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScoresConfig;
    use serde_json::json;

    fn provider(kind: &str) -> ProviderConfig {
        serde_yaml::from_str(&format!("{{ name: p, kind: {kind}, base_url: 'https://p.test/v1', api_key_env: K }}")).unwrap()
    }

    fn settings(extra: &str) -> Settings {
        Settings::from_yaml(&format!(
            "tiers: [{{ name: smart, min_score: 30 }}, {{ name: fast, min_score: 0 }}]\nproviders: []\nscores: null\n{extra}"
        ))
        .unwrap()
    }

    fn ep(raw: &str) -> Arc<Endpoint> {
        Arc::new(Endpoint {
            provider: "p".into(),
            raw_id: raw.into(),
            base_url: "https://p.test/v1".into(),
            keys: normalize::keys([Some(raw)]),
            priority: 100,
            context: None,
            tools: None,
            vision: None,
        })
    }

    fn book(rows: &[(&str, f64)]) -> ScoreBook {
        let b = ScoreBook::new(ScoresConfig { cache_file: "/nonexistent/none.json".into(), ..Default::default() }, crate::http::build_client(10.0));
        b.set_table(rows.iter().map(|(k, v)| (normalize::family_key(k), *v)).collect());
        b
    }

    const ZAI_PRICING: &str = r"
### Latest Models

| Model | Input | Cached Input | Cached Input Storage | Output |
| :- | :- | :- | :- | :- |
| GLM-5.3 | \$1.4 | \$0.26 | Limited-time Free | \$4.4 |

### Image Generation Models

| Model | Price |
| :- | :- |
| GLM-Image | Free |

> [accordion] Other Pricing
>
> ### Text Models
>
> | Model | Input | Cached Input | Cached Input Storage | Output |
> | :- | :- | :- | :- | :- |
> | GLM-4.7-FlashX | \$0.07 | \$0.01 | Limited-time Free | \$0.4 |
> | GLM-4-32B-0414-128K | \$0.1 | - | - | \$0.1 |
> | GLM-4.7-Flash | Free | Free | Free | Free |
> | GLM-4.5-Flash | Free | Free | Free | Free |
>
> ### Vision Models
>
> | Model | Input | Cached Input | Cached Input Storage | Output |
> | :- | :- | :- | :- | :- |
> | GLM-OCR | \$0.03 | \\ | \\ | \$0.03 |
> | GLM-4.5V | \$0.6 | \$0.11 | Limited-time Free | \$1.8 |
> | GLM-4.6V-Flash | Free | Free | Free | Free |
";

    #[test]
    fn zai_keeps_only_models_free_in_every_price_column() {
        let eps = parse_zai(&provider("zai"), ZAI_PRICING);
        let got: Vec<_> = eps.iter().map(|e| (e.raw_id.as_str(), e.vision)).collect();
        // GLM-Image is a per-image price, not a chat model; "Limited-time Free" is only cache storage.
        assert_eq!(got, [("glm-4.7-flash", Some(false)), ("glm-4.5-flash", Some(false)), ("glm-4.6v-flash", Some(true))]);
        assert!(parse_zai(&provider("zai"), "<html>moved</html>").is_empty());
    }

    #[test]
    fn google_prefix_is_stripped_from_upstream_id() {
        let [e] = parse_openai(&provider("openai"), &json!({ "data": [{ "id": "models/gemini-3.8-flash" }] })).try_into().unwrap();
        assert_eq!(e.raw_id, "gemini-3.8-flash");
        assert_eq!(e.keys, BTreeSet::from(["gemini-3.8-flash".to_string()]));
    }

    #[test]
    fn openrouter_keeps_only_free_text_models() {
        let payload = json!({ "data": [
            { "id": "a/free-one:free", "pricing": { "prompt": "0", "completion": "0" },
              "architecture": { "output_modalities": ["text"], "input_modalities": ["text", "image"] },
              "supported_parameters": ["tools"], "context_length": 1000 },
            { "id": "a/paid", "pricing": { "prompt": "0.1", "completion": "0" }, "architecture": { "output_modalities": ["text"] } },
            { "id": "g/lyria", "pricing": { "prompt": "0", "completion": "0" }, "architecture": { "output_modalities": ["text", "audio"] } },
        ]});
        let eps = parse_openrouter(&provider("openrouter"), &payload);
        assert_eq!(eps.len(), 1);
        assert_eq!((eps[0].tools, eps[0].vision, eps[0].context), (Some(true), Some(true), Some(1000)));
    }

    #[test]
    fn opencode_keeps_only_free_chat_models_and_uses_their_base_url() {
        let free = |id: &str, price: f64| {
            json!({ "id": id, "type": "chat", "cost": { "text": { "input": { "price": price }, "output": { "price": 0 } } },
                    "api": { "chat-completions": { "base_url": "https://oc.test/inference/v1" } },
                    "capabilities": { "tools": { "supported": true } }, "modalities": { "input": ["text"] } })
        };
        let eps = parse_opencode(&provider("opencode"), &json!({ "data": [free("m-free", 0.0), free("m-paid", 2.0)] }));
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].base_url, "https://oc.test/inference/v1");
        assert_eq!((eps[0].tools, eps[0].vision), (Some(true), Some(false)));
    }

    #[test]
    fn same_model_is_merged_across_providers() {
        let models = reconcile(vec![ep("org/big-500b:free"), ep("big-500b-free")], &settings(""), None);
        assert_eq!(models.len(), 1);
        assert_eq!(models["big-500b"].endpoints.len(), 2);
    }

    #[test]
    fn aliases_join_names_the_heuristic_cannot() {
        let models = reconcile(vec![ep("alpha-1"), ep("beta-one")], &settings("aliases:\n  alpha-1: [beta-one]\n"), None);
        assert_eq!(models.len(), 1);
        assert_eq!(models["alpha-1"].endpoints.len(), 2);
    }

    #[test]
    fn unrated_models_belong_to_no_tier() {
        let b = book(&[("known-model", 35.0)]);
        let models = reconcile(vec![ep("known-model:free"), ep("mystery-model")], &settings(""), Some(&b));
        assert_eq!((models["known-model"].tier.as_deref(), models["known-model"].score), (Some("smart"), Some(35.0)));
        assert_eq!((models["mystery-model"].tier.clone(), models["mystery-model"].score), (None, None));
    }

    #[test]
    fn manual_score_and_tier_override_the_benchmark() {
        let b = book(&[("known-model", 35.0)]);
        let s = settings("models:\n  mystery-model: { score: 5 }\n  known-model: { tier: fast }\n  gone: { disabled: true }\n");
        let models = reconcile(vec![ep("known-model"), ep("mystery-model"), ep("gone")], &s, Some(&b));
        assert_eq!(models["mystery-model"].tier.as_deref(), Some("fast"));
        assert_eq!(models["known-model"].tier.as_deref(), Some("fast"));
        assert!(!models.contains_key("gone"));
    }

    #[test]
    fn exclude_globs_apply_to_canonical_names() {
        let models = reconcile(vec![ep("keep-me"), ep("drop-pro-x")], &settings("exclude: ['*-pro-*']\n"), None);
        assert!(models.contains_key("keep-me") && !models.contains_key("drop-pro-x"));
    }

    #[test]
    fn provider_filters() {
        let mut p = provider("openai");
        p.exclude = vec!["*-pro*".into()];
        assert!(wanted(&p, "gemini-3.5-flash"));
        assert!(!wanted(&p, "gemini-3.1-pro-preview"));
        assert!(!wanted(&p, "text-embedding-3"));
        assert!(!wanted(&p, "whisper-large"));
        p.include = vec!["gemini-*".into()];
        assert!(!wanted(&p, "gemma-4"));
    }
}
