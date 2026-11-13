//! Durable accounting and score snapshots. No request text or credentials are stored.
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Identity(pub String, pub String);

pub fn named_tokens() -> BTreeMap<String, String> {
    let raw = std::env::var("ROUTER_NAMED_TOKENS").unwrap_or_default();
    parse_named_tokens(if raw.is_empty() { "{}" } else { &raw })
        .expect("ROUTER_NAMED_TOKENS must be a JSON object of unique non-empty name: token pairs")
}

pub fn parse_named_tokens(raw: &str) -> Result<BTreeMap<String, String>, &'static str> {
    let names: BTreeMap<String, String> = serde_json::from_str(raw).map_err(|_| "invalid named tokens")?;
    let mut tokens = std::collections::BTreeSet::new();
    if names.iter().any(|(n, t)| n.trim().is_empty() || t.trim().is_empty() || !tokens.insert(t)) {
        return Err("invalid named tokens");
    }
    Ok(names)
}

pub fn identity(token: &str, names: &BTreeMap<String, String>) -> Identity {
    if token.is_empty() {
        return Identity("anonymous".into(), "anonymous".into());
    }
    Identity(
        format!("{:x}", Sha256::digest(token.as_bytes())),
        names.iter().find(|(_, t)| t.as_str() == token).map(|(n, _)| n.clone()).unwrap_or_else(|| "unnamed".into()),
    )
}

pub struct Store(Mutex<Connection>);
impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        if path != ":memory:"
            && let Some(dir) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(dir).map_err(|_| rusqlite::Error::InvalidPath(dir.into()))?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self(Mutex::new(db)))
    }
    pub fn register(&self, keys: &[Identity]) -> rusqlite::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        for key in keys {
            tx.execute("INSERT INTO access_keys VALUES (?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name", params![key.0, key.1])?;
        }
        tx.commit()
    }
    pub fn record(&self, event: Event) -> rusqlite::Result<()> {
        let count = |name: &str| event.usage[name].as_i64().filter(|v| *v >= 0);
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO access_keys VALUES (?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            params![event.key.0, event.key.1],
        )?;
        tx.execute(
            "INSERT INTO usage_events(key_id,provider,model,prompt_tokens,completion_tokens,completed) VALUES (?,?,?,?,?,?)",
            params![event.key.0, event.provider, event.model, count("prompt_tokens"), count("completion_tokens"), event.completed],
        )?;
        tx.commit()
    }
    pub fn summary(&self) -> rusqlite::Result<Value> {
        let db = self.0.lock().unwrap();
        let mut stmt = db.prepare(SUMMARY)?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "key_id": r.get::<_,String>(0)?, "name": r.get::<_,String>(1)?,
                "provider": r.get::<_,String>(2)?, "model": r.get::<_,String>(3)?,
                "requests": r.get::<_,i64>(4)?, "completed": r.get::<_,i64>(5)?,
                "unknown_prompt_requests": r.get::<_,i64>(6)?, "unknown_completion_requests": r.get::<_,i64>(7)?,
                "prompt_tokens": r.get::<_,Option<i64>>(8)?, "completion_tokens": r.get::<_,Option<i64>>(9)?
            }))
        })?;
        Ok(json!({"data":rows.collect::<rusqlite::Result<Vec<_>>>()?}))
    }
    pub fn load_scores(&self, key: &str) -> rusqlite::Result<Option<Value>> {
        use rusqlite::OptionalExtension;
        self.0
            .lock()
            .unwrap()
            .query_row("SELECT fetched_at,scores FROM score_cache WHERE cache_key=?", [key], |r| {
                let text: String = r.get(1)?;
                Ok(json!({"fetched_at":r.get::<_,f64>(0)?, "table":serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)}))
            })
            .optional()
    }
    pub fn save_scores(&self, key: &str, table: &Value, at: f64) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute("INSERT INTO score_cache VALUES (?,?,?) ON CONFLICT(cache_key) DO UPDATE SET fetched_at=excluded.fetched_at,scores=excluded.scores", params![key,at,table.to_string()])?;
        Ok(())
    }
}

pub struct Event {
    pub key: Identity,
    pub provider: String,
    pub model: String,
    pub usage: Value,
    pub completed: bool,
}

pub async fn record(store: Arc<Store>, event: Event) {
    match tokio::task::spawn_blocking(move || store.record(event)).await {
        Ok(Ok(())) => {}
        result => tracing::error!("could not persist usage: {result:?}"),
    }
}

#[derive(Default)]
pub struct UsageParser {
    pending: Vec<u8>,
    discarding: bool,
    pub usage: Value,
    pub done: bool,
    pub failed: bool,
}
impl UsageParser {
    pub fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == b'\n' {
                if !self.discarding
                    && let Some(data) = self.pending.strip_prefix(b"data:")
                {
                    let data = data.trim_ascii();
                    if data == b"[DONE]" {
                        self.done = true;
                    } else if let Ok(obj) = serde_json::from_slice::<Value>(data) {
                        self.failed |= obj.get("error").is_some();
                        if obj["usage"].is_object() {
                            self.usage = obj["usage"].clone();
                        }
                    }
                }
                self.pending.clear();
                self.discarding = false;
            } else if self.pending.len() >= 1048576 {
                self.pending.clear();
                self.discarding = true;
            } else if !self.discarding {
                self.pending.push(b);
            }
        }
    }
}

/// Drop also runs when the client disconnects before the stream ends.
pub struct StreamAccounting {
    pub parser: UsageParser,
    pub event: Option<Event>,
    pub store: Arc<Store>,
}
impl Drop for StreamAccounting {
    fn drop(&mut self) {
        if let Some(mut event) = self.event.take() {
            event.usage = self.parser.usage.take();
            event.completed = self.parser.done && !self.parser.failed;
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = store.record(event) {
                    tracing::error!("could not persist usage: {e}");
                }
            });
        }
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS access_keys (id TEXT PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS usage_events (
 id INTEGER PRIMARY KEY, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
 key_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
 prompt_tokens INTEGER, completion_tokens INTEGER, completed INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS usage_key ON usage_events(key_id);
CREATE TABLE IF NOT EXISTS score_cache (cache_key TEXT PRIMARY KEY, fetched_at REAL NOT NULL, scores TEXT NOT NULL);
"#;
const SUMMARY: &str = r#"SELECT k.id AS key_id, k.name, u.provider, u.model, COUNT(*) AS requests,
SUM(u.completed) AS completed, COUNT(*)-COUNT(u.prompt_tokens) AS unknown_prompt_requests,
COUNT(*)-COUNT(u.completion_tokens) AS unknown_completion_requests,
SUM(u.prompt_tokens) AS prompt_tokens, SUM(u.completion_tokens) AS completion_tokens
FROM usage_events u JOIN access_keys k ON k.id=u.key_id
GROUP BY k.id,k.name,u.provider,u.model ORDER BY k.name,k.id,u.provider,u.model"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_tokens_validate_and_identity_survives_rename() {
        let names = parse_named_tokens(r#"{"eHub":"secret"}"#).unwrap();
        let key = identity("secret", &names);
        assert_eq!(key.1, "eHub");
        assert_eq!(key.0, "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b");
        for raw in ["[]", r#"{"a":""}"#, r#"{"a":"x","b":"x"}"#, r#"{"a":3}"#] {
            assert!(parse_named_tokens(raw).is_err());
        }
        let store = Store::open(":memory:").unwrap();
        store
            .record(Event {
                key: key.clone(),
                provider: "p".into(),
                model: "m".into(),
                usage: json!({"prompt_tokens":4,"completion_tokens":9}),
                completed: true,
            })
            .unwrap();
        store
            .record(Event {
                key: Identity(key.0, "books".into()),
                provider: "p".into(),
                model: "m".into(),
                usage: Value::Null,
                completed: false,
            })
            .unwrap();
        let summary = store.summary().unwrap();
        let row = &summary["data"][0];
        assert_eq!(row["name"], "books");
        assert_eq!(row["requests"], 2);
        assert_eq!(row["prompt_tokens"], 4);
        assert_eq!(row["unknown_prompt_requests"], 1);
        assert_eq!(row["completed"], 1);
        assert!(!summary.to_string().contains("secret"));
    }

    #[test]
    fn sse_usage_survives_every_chunk_boundary() {
        let bytes = b"data: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":9}}\r\n\r\ndata: [DONE]\n\n";
        for split in 0..bytes.len() {
            let mut parser = UsageParser::default();
            parser.feed(&bytes[..split]);
            parser.feed(&bytes[split..]);
            assert!(parser.done);
            assert_eq!(parser.usage["completion_tokens"], 9);
        }
        let mut parser = UsageParser::default();
        parser.feed(&vec![b'x'; 1100000]);
        parser.feed(b"\ndata: {\"error\":{}}\n\ndata: [DONE]\n\n");
        assert!(parser.done && parser.failed);
        assert!(parser.pending.len() < 1048576);
    }

    #[tokio::test]
    async fn dropped_stream_is_accounted_as_incomplete() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let guard = StreamAccounting {
            parser: UsageParser::default(),
            store: store.clone(),
            event: Some(Event {
                key: identity("secret", &BTreeMap::new()),
                provider: "p".into(),
                model: "m".into(),
                usage: Value::Null,
                completed: true,
            }),
        };
        drop(guard);
        for _ in 0..100 {
            if store.summary().unwrap()["data"].as_array().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(store.summary().unwrap()["data"][0]["completed"], 0);
        assert_eq!(store.summary().unwrap()["data"][0]["prompt_tokens"], Value::Null);
    }
}
