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
        db.execute_batch(MIGRATION)?;
        Ok(Self(Mutex::new(db)))
    }
    pub fn register(&self, keys: &[Identity]) -> rusqlite::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        for key in keys {
            tx.execute("INSERT INTO access_keys VALUES (?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name", params![key.0, key.1],
            )?;
        }
        tx.commit()
    }
    pub fn record(&self, event: Event) -> rusqlite::Result<()> {
        self.record_batch(&[event])
    }
    pub fn record_batch(&self, events: &[Event]) -> rusqlite::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        for event in events {
            let count = |name: &str| event.usage[name].as_i64().filter(|v| *v >= 0);
        tx.execute(
            "INSERT INTO access_keys VALUES (?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            params![event.key.0, event.key.1],
        )?;
        tx.execute(
            "INSERT INTO usage_events(key_id,provider,model,prompt_tokens,completion_tokens,completed) VALUES (?,?,?,?,?,?)",
            params![event.key.0, event.provider, event.model, count("prompt_tokens"), count("completion_tokens"), event.completed],
        )?;
        }
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
        self.0.lock().unwrap().execute("INSERT INTO score_cache VALUES (?,?,?) ON CONFLICT(cache_key) DO UPDATE SET fetched_at=excluded.fetched_at,scores=excluded.scores", params![key,at,table.to_string()],
        )?;
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

// A reservation is acquired before an upstream request. It stays alive until
// its event is committed, bounding in-flight accounting and the channel together.
const WRITE_CAPACITY: usize = 1024;
struct WriteJob {
    event: Option<Event>,
    ack: Option<tokio::sync::oneshot::Sender<()>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

pub struct UsageWriter {
    sender: std::sync::mpsc::Sender<WriteJob>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl UsageWriter {
    pub fn new(store: Arc<Store>) -> Self {
        Self::with_capacity(store, WRITE_CAPACITY)
    }

    fn with_capacity(store: Arc<Store>, capacity: usize) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel::<WriteJob>();
        std::thread::Builder::new()
            .name("usage-writer".into())
            .spawn(move || {
                while let Ok(first) = receiver.recv() {
                    let mut jobs = vec![first];
                    while jobs.len() < 64 && jobs.last().unwrap().ack.is_none() {
                        match receiver.try_recv() {
                            Ok(job) => jobs.push(job),
                            Err(_) => break,
                        }
                    }
                    let events: Vec<Event> = jobs.iter_mut().filter_map(|j| j.event.take()).collect();
                    if !events.is_empty()
                        && let Err(e) = store.record_batch(&events)
                    {
                        tracing::error!("could not persist usage batch: {e}");
                    }
                    for mut job in jobs {
                        let ack = job.ack.take();
                        // Release capacity before acknowledging the flush.
                        drop(job);
                        if let Some(ack) = ack {
                            let _ = ack.send(());
                        }
                    }
                }
            })
            .expect("cannot start usage writer");
        Self {
            sender,
            slots: Arc::new(tokio::sync::Semaphore::new(capacity)),
        }
    }

    pub async fn reserve(&self) -> UsageTicket {
        let permit = self.slots.clone().acquire_owned().await.expect("usage writer closed");
        UsageTicket {
            sender: self.sender.clone(),
            permit,
        }
    }

    /// Wait for already enqueued events, including at graceful shutdown.
    pub async fn flush(&self) {
        let ticket = self.reserve().await;
        let (ack, done) = tokio::sync::oneshot::channel();
        if ticket
            .sender
            .send(WriteJob {
                event: None,
                ack: Some(ack),
                _permit: ticket.permit,
            })
            .is_ok()
        {
            let _ = done.await;
        }
    }
}

pub struct UsageTicket {
    sender: std::sync::mpsc::Sender<WriteJob>,
    permit: tokio::sync::OwnedSemaphorePermit,
}
impl UsageTicket {
    pub fn record(self, mut event: Event) {
        // Retain counters only, never the full upstream usage object.
        event.usage = json!({"prompt_tokens": event.usage["prompt_tokens"].as_i64(),
                            "completion_tokens": event.usage["completion_tokens"].as_i64()});
        if self
            .sender
            .send(WriteJob {
                event: Some(event),
                ack: None,
                _permit: self.permit,
            })
            .is_err()
        {
            tracing::error!("usage writer stopped before event could be queued");
        }
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
    pub ticket: Option<UsageTicket>,
}
impl Drop for StreamAccounting {
    fn drop(&mut self) {
        if let Some(mut event) = self.event.take() {
            event.usage = self.parser.usage.take();
            event.completed = self.parser.done && !self.parser.failed;
            if let Some(ticket) = self.ticket.take() {
                ticket.record(event);
                }
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
const MIGRATION: &str = r#"BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS storage_migrations (name TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS usage_totals (
 key_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
 requests INTEGER NOT NULL, completed INTEGER NOT NULL,
 unknown_prompt_requests INTEGER NOT NULL, unknown_completion_requests INTEGER NOT NULL,
 prompt_tokens INTEGER, completion_tokens INTEGER,
 PRIMARY KEY(key_id,provider,model));
INSERT INTO usage_totals
SELECT key_id,provider,model,COUNT(*),SUM(completed),
 COUNT(*)-COUNT(prompt_tokens),COUNT(*)-COUNT(completion_tokens),SUM(prompt_tokens),SUM(completion_tokens)
FROM usage_events WHERE NOT EXISTS (SELECT 1 FROM storage_migrations WHERE name='usage_totals_v1')
GROUP BY key_id,provider,model;
CREATE TRIGGER IF NOT EXISTS usage_totals_insert AFTER INSERT ON usage_events BEGIN
 INSERT INTO usage_totals VALUES (NEW.key_id,NEW.provider,NEW.model,1,NEW.completed,
 NEW.prompt_tokens IS NULL,NEW.completion_tokens IS NULL,NEW.prompt_tokens,NEW.completion_tokens)
 ON CONFLICT(key_id,provider,model) DO UPDATE SET
 requests=requests+1, completed=completed+excluded.completed,
 unknown_prompt_requests=unknown_prompt_requests+excluded.unknown_prompt_requests,
 unknown_completion_requests=unknown_completion_requests+excluded.unknown_completion_requests,
 prompt_tokens=CASE WHEN excluded.prompt_tokens IS NULL THEN usage_totals.prompt_tokens ELSE COALESCE(usage_totals.prompt_tokens,0)+excluded.prompt_tokens END,
 completion_tokens=CASE WHEN excluded.completion_tokens IS NULL THEN usage_totals.completion_tokens ELSE COALESCE(usage_totals.completion_tokens,0)+excluded.completion_tokens END;
END;
INSERT OR IGNORE INTO storage_migrations VALUES ('usage_totals_v1');
COMMIT;"#;
const SUMMARY: &str = r#"SELECT k.id AS key_id, k.name, u.provider, u.model,u.requests,u.completed,
u.unknown_prompt_requests,u.unknown_completion_requests,u.prompt_tokens,u.completion_tokens
FROM usage_totals u JOIN access_keys k ON k.id=u.key_id
ORDER BY k.name,k.id,u.provider,u.model"#;

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
        let writer = UsageWriter::new(store.clone());
        let guard = StreamAccounting {
            parser: UsageParser::default(),
            ticket: Some(writer.reserve().await),
            event: Some(Event {
                key: identity("secret", &BTreeMap::new()),
                provider: "p".into(),
                model: "m".into(),
                usage: Value::Null,
                completed: true,
            }),
        };
        drop(guard);
        writer.flush().await;
        assert_eq!(store.summary().unwrap()["data"][0]["completed"], 0);
        assert_eq!(store.summary().unwrap()["data"][0]["prompt_tokens"], Value::Null);
    }

    #[test]
    fn legacy_events_backfill_once_and_old_writers_update_totals() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch(
            "INSERT INTO access_keys VALUES ('k','old');
            INSERT INTO usage_events(key_id,provider,model,prompt_tokens,completed) VALUES ('k','p','m',4,1);
            INSERT INTO usage_events(key_id,provider,model,completion_tokens,completed) VALUES ('k','p','m',8,0);",
        )
        .unwrap();
        db.execute_batch(MIGRATION).unwrap();
        db.execute_batch("INSERT INTO usage_events(key_id,provider,model,completed) VALUES ('k','p','m',0);")
            .unwrap();
        db.execute_batch(MIGRATION).unwrap();
        let store = Store(Mutex::new(db));
        store.register(&[Identity("k".into(), "renamed".into())]).unwrap();
        let data = store.summary().unwrap();
        let row = &data["data"][0];
        assert_eq!(row["requests"], 3);
        assert_eq!(row["name"], "renamed");
        assert_eq!(row["prompt_tokens"], 4);
        assert_eq!(row["completion_tokens"], 8);
        assert_eq!(row["unknown_prompt_requests"], 2);
        assert_eq!(row["unknown_completion_requests"], 2);
        // The summary continues to work when access to historical events is removed.
        store.0.lock().unwrap().execute_batch("DROP TABLE usage_events;").unwrap();
        assert_eq!(store.summary().unwrap(), data);
    }

    #[tokio::test]
    async fn writer_retains_capacity_until_commit_and_flushes() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let writer = UsageWriter::with_capacity(store.clone(), 2);
        let lock = store.0.lock().unwrap();
        for _ in 0..2 {
            writer.reserve().await.record(Event {
                key: Identity("k".into(), "n".into()),
                provider: "p".into(),
                model: "m".into(),
                usage: json!({"prompt_tokens":3}),
                completed: true,
            });
        }
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), writer.reserve()).await.is_err());
        drop(lock);
        writer.flush().await;
        let data = store.summary().unwrap();
        assert_eq!(data["data"][0]["requests"], 2);
        assert_eq!(data["data"][0]["prompt_tokens"], 6);
        // Abandoning a reservation (upstream error/cancellation) frees it.
        let ticket = writer.reserve().await;
        drop(ticket);
        assert_eq!(writer.slots.available_permits(), 2);
    }
}
