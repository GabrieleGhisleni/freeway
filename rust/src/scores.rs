//! Intelligence scores from Artificial Analysis, cached on disk.
//!
//! A model with no score is *unrated*: it is never picked by a tier, only by name.
//! The cache file has the same shape as the Python version's (`data/scores.json`).

use crate::config::ScoresConfig;
use crate::normalize;
use crate::util::now;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::RwLock;
use std::time::Duration;

const URL: &str = "https://artificialanalysis.ai/api/v2/language/models/free";

/// family key -> intelligence index.
///
/// One model appears several times (one row per reasoning effort). The tier is
/// a floor, so the lowest variant wins.
pub fn parse(rows: &[Value]) -> HashMap<String, f64> {
    let mut table: HashMap<String, f64> = HashMap::new();
    for r in rows {
        let Some(value) = r["evaluations"]["artificial_analysis_intelligence_index"].as_f64() else { continue };
        let Some(slug) = r["slug"].as_str().filter(|s| !s.is_empty()) else { continue };
        let k = normalize::family_key(slug);
        if k.is_empty() {
            continue;
        }
        table.entry(k).and_modify(|v| *v = v.min(value)).or_insert(value);
    }
    table
}

#[derive(Default)]
struct State {
    table: HashMap<String, f64>,
    fetched_at: Option<f64>,
    error: Option<String>,
}

pub struct ScoreBook {
    cfg: ScoresConfig,
    client: reqwest::Client,
    state: RwLock<State>,
}

impl ScoreBook {
    pub fn new(cfg: ScoresConfig, client: reqwest::Client) -> Self {
        let book = Self { cfg, client, state: RwLock::new(State::default()) };
        book.load_cache();
        book
    }

    fn load_cache(&self) {
        let Ok(text) = std::fs::read_to_string(&self.cfg.cache_file) else { return };
        let Ok(data) = serde_json::from_str::<Value>(&text) else { return };
        let (Some(table), Some(at)) = (data["table"].as_object(), data["fetched_at"].as_f64()) else { return };
        let mut st = self.state.write().unwrap();
        st.table = table.iter().filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f))).collect();
        st.fetched_at = Some(at);
    }

    fn save_cache(&self, table: &HashMap<String, f64>, at: f64) {
        let path = Path::new(&self.cfg.cache_file);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, json!({ "fetched_at": at, "table": table }).to_string()) {
            tracing::warn!("could not write score cache: {e}");
        }
    }

    pub fn stale(&self) -> bool {
        match self.state.read().unwrap().fetched_at {
            None => true,
            Some(at) => now() - at > self.cfg.refresh_hours * 3600.0,
        }
    }

    pub fn entries(&self) -> usize {
        self.state.read().unwrap().table.len()
    }
    pub fn fetched_at(&self) -> Option<f64> {
        self.state.read().unwrap().fetched_at
    }
    pub fn error(&self) -> Option<String> {
        self.state.read().unwrap().error.clone()
    }

    pub async fn ensure_fresh(&self) {
        if !self.stale() {
            return;
        }
        let Some(key) = self.cfg.api_key() else {
            let cached = self.entries() > 0;
            self.state.write().unwrap().error =
                Some(format!("{} is not set{}", self.cfg.api_key_env, if cached { " (using cached scores)" } else { "" }));
            return;
        };
        match self.fetch(&key).await {
            Ok(table) => {
                let at = now();
                self.save_cache(&table, at);
                tracing::info!("scores: {} models rated by Artificial Analysis", table.len());
                let mut st = self.state.write().unwrap();
                st.table = table;
                st.fetched_at = Some(at);
                st.error = None;
            }
            Err(e) => {
                // Keep serving with whatever we had.
                let mut st = self.state.write().unwrap();
                tracing::warn!("score refresh failed: {e} (keeping {} cached scores)", st.table.len());
                st.error = Some(e);
            }
        }
    }

    async fn fetch(&self, key: &str) -> Result<HashMap<String, f64>, String> {
        let url = self.cfg.url.as_deref().unwrap_or(URL);
        let mut rows: Vec<Value> = Vec::new();
        let mut page = 1;
        loop {
            let sep = if url.contains('?') { '&' } else { '?' };
            let resp = self
                .client
                .get(format!("{url}{sep}page={page}"))
                .header("x-api-key", key)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?
                .error_for_status()
                .map_err(|e| e.to_string())?;
            let body: Value = resp.json().await.map_err(|e| format!("bad JSON: {e}"))?;
            if let Some(data) = body["data"].as_array() {
                rows.extend(data.iter().cloned());
            }
            if body["pagination"]["has_more"].as_bool() != Some(true) || page >= 20 {
                break;
            }
            page += 1;
        }
        let table = parse(&rows);
        if table.is_empty() {
            return Err("no scores in the answer".into());
        }
        Ok(table)
    }

    /// Lowest score among the names the benchmark knows.
    pub fn lookup(&self, names: &BTreeSet<String>) -> Option<f64> {
        let st = self.state.read().unwrap();
        names.iter().filter_map(|n| st.table.get(&normalize::family_key(n)).copied()).reduce(f64::min)
    }

    #[cfg(test)]
    pub fn set_table(&self, table: HashMap<String, f64>) {
        self.state.write().unwrap().table = table;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slug: &str, value: Option<f64>) -> Value {
        json!({ "slug": slug, "evaluations": { "artificial_analysis_intelligence_index": value } })
    }

    #[test]
    fn parse_takes_lowest_variant_and_skips_nulls() {
        let table = parse(&[row("model-x-high", Some(40.0)), row("model-x-low", Some(31.0)), row("model-y", None), json!({"slug": null})]);
        assert_eq!(table.len(), 1);
        assert_eq!(table["modelx"], 31.0);
    }
}
