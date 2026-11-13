//! End-to-end routing against fake providers served by a local axum server.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http_body_util::BodyExt;
use freeway::app::build_app;
use freeway::config::{ScoresConfig, Settings};
use freeway::http::build_client;
use freeway::scores::ScoreBook;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once};
use tower::ServiceExt;

fn ok_body() -> Value {
    json!({ "id": "x", "object": "chat.completion", "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi" } }] })
}

#[derive(Clone)]
enum Reply {
    Json(u16, Value, Vec<(&'static str, &'static str)>),
    Raw(u16, &'static str, &'static str),
}

fn limited(retry_after: &'static str) -> Reply {
    Reply::Json(429, json!({ "error": "slow down" }), vec![("retry-after", retry_after)])
}

#[derive(Default)]
struct Fake {
    listings: HashMap<String, Vec<String>>,
    behaviour: Mutex<HashMap<(String, String), Reply>>,
    calls: Mutex<Vec<(String, String)>>,
}

impl Fake {
    fn new(listings: &[(&str, &[&str])]) -> Self {
        Self {
            listings: listings.iter().map(|(h, ms)| (h.to_string(), ms.iter().map(|m| m.to_string()).collect())).collect(),
            ..Default::default()
        }
    }
    fn on(self, host: &str, model: &str, reply: Reply) -> Self {
        self.behaviour.lock().unwrap().insert((host.into(), model.into()), reply);
        self
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

async fn fake_models(State(f): State<Arc<Fake>>, Path(host): Path<String>) -> Json<Value> {
    let ids = f.listings.get(&host).cloned().unwrap_or_default();
    Json(json!({ "data": ids.iter().map(|m| json!({ "id": m })).collect::<Vec<_>>() }))
}

async fn fake_chat(State(f): State<Arc<Fake>>, Path(host): Path<String>, Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or("").to_string();
    f.calls.lock().unwrap().push((host.clone(), model.clone()));
    let reply = f.behaviour.lock().unwrap().get(&(host, model)).cloned().unwrap_or(Reply::Json(200, ok_body(), vec![]));
    match reply {
        Reply::Json(status, body, headers) => {
            let mut resp = (StatusCode::from_u16(status).unwrap(), Json(body)).into_response();
            for (k, v) in headers {
                resp.headers_mut().insert(k, v.parse().unwrap());
            }
            resp
        }
        Reply::Raw(status, body, content_type) => {
            Response::builder().status(status).header("content-type", content_type).body(Body::from(body)).unwrap()
        }
    }
}

/// Serves `fake` on a free local port; returns the port.
async fn serve_fake(fake: Arc<Fake>) -> u16 {
    let app = axum::Router::new()
        .route("/{host}/v1/models", get(fake_models))
        .route("/{host}/v1/chat/completions", post(fake_chat))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Safe here: runs once, before any test reads these variables.
        unsafe {
            std::env::set_var("KEY_A", "ka");
            std::env::set_var("KEY_B", "kb");
            std::env::set_var("KEY_AA", "kaa");
        }
    });
}

struct Harness {
    app: axum::Router,
    fake: Arc<Fake>,
}

const DEFAULT_MODELS: &str = "  big-500b: { score: 90 }\n  small-1b: { score: 10 }\n";

async fn start(fake: Fake, models_yaml: &str, tokens: &[&str]) -> Harness {
    init_env();
    let fake = Arc::new(fake);
    let port = serve_fake(fake.clone()).await;
    let yaml = format!(
        "database_file: ':memory:'
scores: null
health_file: null
tiers:
  - {{ name: smart, min_score: 80, fallback: [fast] }}
  - {{ name: fast, min_score: 0 }}
providers:
  - {{ name: a, base_url: 'http://127.0.0.1:{port}/a/v1', api_key_env: KEY_A, priority: 1 }}
  - {{ name: b, base_url: 'http://127.0.0.1:{port}/b/v1', api_key_env: KEY_B, priority: 2 }}
models:
{models_yaml}"
    );
    let settings = Arc::new(Settings::from_yaml(&yaml).unwrap());
    let client = build_client(settings.connect_timeout);
    let (app, _catalog) = build_app(settings, tokens.iter().map(|t| t.to_string()).collect(), client).await;
    Harness { app, fake }
}

struct Reply2 {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply2 {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> String {
        self.headers.get(name).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
    }
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> Reply2 {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        Reply2 { status: parts.status, headers: parts.headers, body: body.collect().await.unwrap().to_bytes().to_vec() }
    }

    async fn get(&self, path: &str, token: Option<&str>) -> Reply2 {
        let mut req = Request::builder().uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    async fn chat_with(&self, body: Value, extra: &[(&str, &str)]) -> Reply2 {
        let mut req = Request::builder().method("POST").uri("/v1/chat/completions").header("authorization", "Bearer secret");
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        self.send(req.body(Body::from(body.to_string())).unwrap()).await
    }

    async fn post_json(&self, path: &str, body: Value) -> Reply2 {
        let req = Request::builder().method("POST").uri(path).header("authorization", "Bearer secret");
        self.send(req.body(Body::from(body.to_string())).unwrap()).await
    }

    async fn chat(&self, model: &str) -> Reply2 {
        self.chat_with(json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] }), &[]).await
    }
}

#[tokio::test]
async fn requires_token() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    assert_eq!(h.get("/v1/models", None).await.status, 401);
    assert_eq!(h.get("/v1/models", Some("nope")).await.status, 401);
    assert_eq!(h.get("/v1/models", Some("secret")).await.status, 200);
    assert_eq!(h.get("/health", None).await.status, 401);
}

#[tokio::test]
async fn several_tokens_are_accepted() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &["alice-token", "bob-token"]).await;
    for ok in ["alice-token", "bob-token"] {
        assert_eq!(h.get("/v1/models", Some(ok)).await.status, 200);
    }
    let by_key = Request::builder().uri("/v1/models").header("x-api-key", "bob-token").body(Body::empty()).unwrap();
    assert_eq!(h.send(by_key).await.status, 200);
    for bad in ["alice", "alice-token-x", ""] {
        assert_eq!(h.get("/v1/models", Some(bad)).await.status, 401, "{bad:?}");
    }
}

#[tokio::test]
async fn no_tokens_means_open() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &[]).await;
    assert_eq!(h.get("/v1/models", None).await.status, 200);
}

#[tokio::test]
async fn same_model_is_merged_across_providers_and_falls_over() {
    let fake = Fake::new(&[("a", &["org/big-500b:free"]), ("b", &["big-500b"])]).on("a", "org/big-500b:free", limited("30"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;

    let listed = h.get("/v1/models", Some("secret")).await.json();
    let models: Vec<&Value> = listed["data"].as_array().unwrap().iter().filter(|m| m["owned_by"] == "router").collect();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["providers"].as_array().unwrap().len(), 2);

    let r = h.chat("big-500b").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-router-provider"), "b");
    assert_eq!(r.header("x-router-attempts"), "2");
    h.chat("big-500b").await;
    let calls: Vec<(&str, &str)> = vec![("a", "org/big-500b:free"), ("b", "big-500b"), ("b", "big-500b")];
    assert_eq!(h.fake.calls(), calls.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>()); // a is cooling down
}

#[tokio::test]
async fn tier_falls_through_to_lower_tier() {
    let fake = Fake::new(&[("a", &["big-500b", "small-1b"]), ("b", &[])])
        .on("a", "big-500b", Reply::Json(503, json!({ "error": "down" }), vec![]));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let r = h.chat("smart").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-router-model"), "small-1b");
    assert_eq!(r.header("x-router-tier"), "fast");
}

#[tokio::test]
async fn fallback_none_stays_in_tier() {
    let fake = Fake::new(&[("a", &["big-500b", "small-1b"]), ("b", &[])])
        .on("a", "big-500b", Reply::Json(503, json!({ "error": "down" }), vec![]));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let body = json!({ "model": "smart", "messages": [{ "role": "user", "content": "hi" }] });
    let r = h.chat_with(body, &[("x-router-fallback", "none")]).await;
    // 503, not 502: Cloudflare would replace an origin 502 with an HTML page.
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["error"]["type"], "all_providers_failed");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("1 server"), "{}", r.json());
    assert!(h.fake.calls().iter().all(|(_, m)| m == "big-500b"));
}

#[tokio::test]
async fn named_model_does_not_leave_the_model_by_default() {
    let fake = Fake::new(&[("a", &["big-500b", "small-1b"]), ("b", &[])])
        .on("a", "big-500b", Reply::Json(503, json!({ "error": "down" }), vec![]));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    assert_eq!(h.chat("big-500b").await.status, 503);
    // ...unless the caller allows falling back through the tier.
    let body = json!({ "model": "big-500b", "messages": [{ "role": "user", "content": "hi" }] });
    let r = h.chat_with(body, &[("x-router-fallback", "tier")]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-router-model"), "small-1b");
}

#[tokio::test]
async fn single_model_lookup_by_tier_name_and_endpoint() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &["small-1b"])]), DEFAULT_MODELS, &["secret"]).await;
    let tier = h.get("/v1/models/smart", Some("secret")).await;
    assert_eq!(tier.status, 200);
    assert_eq!(tier.json()["owned_by"], "router-tier");
    let model = h.get("/v1/models/big-500b", Some("secret")).await;
    assert_eq!(model.status, 200);
    assert_eq!(model.json()["id"], "big-500b");
    assert_eq!(model.json()["tier"], "smart");
    let pinned = h.get("/v1/models/a/big-500b", Some("secret")).await;
    assert_eq!(pinned.status, 200);
    assert_eq!(pinned.json()["id"], "a/big-500b");
    let missing = h.get("/v1/models/nope", Some("secret")).await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["error"]["code"], "model_not_found");
    assert_eq!(h.get("/v1/models/smart", None).await.status, 401);
}

#[tokio::test]
async fn unknown_model_is_404() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    let r = h.chat("nope").await;
    assert_eq!(r.status, 404);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("smart"));
}

#[tokio::test]
async fn invalid_requests_are_400() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    let bad_json = Request::builder().method("POST").uri("/v1/chat/completions").header("authorization", "Bearer secret").body(Body::from("{nope")).unwrap();
    assert_eq!(h.send(bad_json).await.status, 400);
    assert_eq!(h.chat_with(json!({ "model": "smart" }), &[]).await.status, 400);
    let body = json!({ "model": "smart", "messages": [] });
    assert_eq!(h.chat_with(body, &[("x-router-fallback", "sideways")]).await.status, 400);
}

#[tokio::test]
async fn bad_request_is_passed_through_and_does_not_cool_down() {
    let err = json!({ "error": { "message": "context too long" } });
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &["big-500b"])])
        .on("a", "big-500b", Reply::Json(400, err.clone(), vec![]))
        .on("b", "big-500b", Reply::Json(400, err.clone(), vec![]));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let r = h.chat("big-500b").await;
    assert_eq!((r.status.as_u16(), r.json()), (400, err));
    assert_eq!(h.chat("big-500b").await.status, 400);
    assert_eq!(h.fake.calls().len(), 4); // tried both again: no cooldown for the caller's mistake
}

#[tokio::test]
async fn error_inside_200_body_triggers_fallback() {
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &["big-500b"])]).on(
        "a",
        "big-500b",
        Reply::Json(200, json!({ "error": { "code": 429, "message": "rate limited" } }), vec![]),
    );
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    assert_eq!(h.chat("big-500b").await.header("x-router-provider"), "b");
}

#[tokio::test]
async fn all_cooling_down_gives_503_with_retry_after() {
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &[])]).on("a", "big-500b", limited("45"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    // First call: the endpoint answers 429, so every attempt failed (503 all_providers_failed, retry-after = its pause).
    let first = h.chat("big-500b").await;
    assert_eq!(first.status, 503);
    assert_eq!(first.json()["error"]["type"], "all_providers_failed");
    assert!(first.json()["error"]["message"].as_str().unwrap().contains("1 rate_limit"), "{}", first.json());
    assert!((1..=45).contains(&first.header("retry-after").parse::<u32>().unwrap()));
    let r = h.chat("big-500b").await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["error"]["type"], "all_providers_busy");
    let retry: u32 = r.header("retry-after").parse().unwrap();
    assert!((1..=45).contains(&retry));
}

#[tokio::test]
async fn streaming_switches_provider_before_first_byte() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &["big-500b"])])
        .on("a", "big-500b", Reply::Raw(200, "data: {\"error\":{\"code\":429,\"message\":\"limit\"}}\n\n", "text/event-stream"))
        .on("b", "big-500b", Reply::Raw(200, sse, "text/event-stream"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let body = json!({ "model": "big-500b", "stream": true, "messages": [{ "role": "user", "content": "hi" }] });
    let r = h.chat_with(body, &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-router-provider"), "b");
    assert_eq!(r.header("content-type"), "text/event-stream");
    assert!(String::from_utf8_lossy(&r.body).contains("hi"));
}

#[tokio::test]
async fn stream_that_ends_with_no_data_is_a_failure() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &["big-500b"])])
        .on("a", "big-500b", Reply::Raw(200, "data: [DONE]\n\n", "text/event-stream"))
        .on("b", "big-500b", Reply::Raw(200, sse, "text/event-stream"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let body = json!({ "model": "big-500b", "stream": true, "messages": [{ "role": "user", "content": "hi" }] });
    assert_eq!(h.chat_with(body, &[]).await.header("x-router-provider"), "b");
}

#[tokio::test]
async fn account_wide_rate_limit_stops_asking_that_provider() {
    let names = ["big-500b", "big2-500b", "big3-500b"];
    let mut fake = Fake::new(&[("a", &names), ("b", &["small-1b"])]);
    for m in names {
        fake = fake.on("a", m, limited("30"));
    }
    let models = "  big-500b: { score: 90 }\n  big2-500b: { score: 89 }\n  big3-500b: { score: 88 }\n  small-1b: { score: 10 }\n";
    let h = start(fake, models, &["secret"]).await;
    let r = h.chat("smart").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-router-model"), "small-1b");
    let calls: Vec<(&str, &str)> = vec![("a", "big-500b"), ("a", "big2-500b"), ("b", "small-1b")];
    assert_eq!(h.fake.calls(), calls.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>());
}

#[tokio::test]
async fn tools_request_skips_endpoints_known_not_to_support_tools() {
    // Providers listed through the plain OpenAI kind do not say, so they stay candidates.
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    let body = json!({ "model": "big-500b", "tools": [{ "type": "function" }], "messages": [{ "role": "user", "content": "hi" }] });
    assert_eq!(h.chat_with(body, &[]).await.status, 200);
}

#[tokio::test]
async fn status_endpoint_reports_tiers_and_unrated() {
    let h = start(Fake::new(&[("a", &["big-500b", "mystery"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    h.chat("big-500b").await;
    let s = h.get("/admin/status", Some("secret")).await.json();
    assert_eq!(s["providers"]["active"], json!(["a", "b"]));
    assert_eq!(s["unrated"], json!(["mystery"]));
    assert_eq!(s["tiers"]["smart"][0]["model"], "big-500b");
    assert_eq!(s["tiers"]["smart"][0]["endpoints"][0]["ok"], 1);
    assert_eq!(s["scores"]["error"], "scores disabled in config");
    let keys: Vec<&String> = s["tiers"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["smart", "fast"]); // config order is kept
}

// ---- scores ----------------------------------------------------------------

fn temp_cache(name: &str) -> String {
    std::env::temp_dir().join(format!("freeway-test-{}-{name}.json", std::process::id())).to_string_lossy().to_string()
}

async fn serve_scores() -> u16 {
    async fn pages(Query(q): Query<HashMap<String, String>>, headers: axum::http::HeaderMap) -> Response {
        if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("kaa") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let page: u32 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
        let row = json!({ "slug": format!("model-{page}"), "evaluations": { "artificial_analysis_intelligence_index": 10 * page } });
        Json(json!({ "pagination": { "has_more": page < 3 }, "data": [row] })).into_response()
    }
    let app = axum::Router::new().route("/aa", get(pages)).route("/aa-fail", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

fn scores_cfg(port: u16, route: &str, cache: &str) -> ScoresConfig {
    ScoresConfig {
        api_key_env: "KEY_AA".into(),
        cache_file: cache.into(),
        url: Some(format!("http://127.0.0.1:{port}/{route}")),
        ..Default::default()
    }
}

#[tokio::test]
async fn scores_follow_pagination_and_write_a_cache() {
    init_env();
    let port = serve_scores().await;
    let cache = temp_cache("paged");
    let _ = std::fs::remove_file(&cache);
    let _ = std::fs::remove_file(std::path::Path::new(&cache).with_extension("sqlite3"));
    let book = ScoreBook::new(scores_cfg(port, "aa", &cache), build_client(10.0));
    assert!(book.stale());
    book.ensure_fresh().await;
    assert_eq!((book.entries(), book.error()), (3, None));
    // a fresh instance starts from the cache and does not need the network
    let again = ScoreBook::new(scores_cfg(1, "aa", &cache), build_client(1.0));
    assert_eq!(again.entries(), 3);
    assert!(!again.stale());
    let _ = std::fs::remove_file(&cache);
    let _ = std::fs::remove_file(std::path::Path::new(&cache).with_extension("sqlite3"));
}

#[tokio::test]
async fn failed_refresh_keeps_cached_scores() {
    init_env();
    let port = serve_scores().await;
    let cache = temp_cache("stale");
    std::fs::write(&cache, json!({ "fetched_at": 1.0, "table": { "modelx": 33.0 } }).to_string()).unwrap();
    let book = ScoreBook::new(scores_cfg(port, "aa-fail", &cache), build_client(10.0));
    assert!(book.stale());
    book.ensure_fresh().await;
    assert_eq!(book.entries(), 1);
    assert!(book.error().is_some());
    let _ = std::fs::remove_file(&cache);
    let _ = std::fs::remove_file(std::path::Path::new(&cache).with_extension("sqlite3"));
}

#[tokio::test]
async fn the_score_cache_is_readable() {
    // Reads a cache file written earlier (data/scores.json).
    let py = format!("{}/../data/scores.json", env!("CARGO_MANIFEST_DIR"));
    if std::path::Path::new(&py).exists() {
        let book = ScoreBook::new(ScoresConfig { cache_file: py, ..Default::default() }, build_client(10.0));
        assert!(book.entries() > 10, "expected the cache to load");
    }
}

fn says(content: &str) -> Reply {
    Reply::Json(
        200,
        json!({ "choices": [{ "index": 0, "message": { "role": "assistant", "content": content } }], "usage": { "prompt_tokens": 120, "completion_tokens": 12 } }),
        vec![],
    )
}

fn decision_request(model: &str) -> Value {
    json!({
        "model": model,
        "state": "I was charged twice. Please refund the extra charge.",
        "questions": {
            "billing": { "type": "noul", "instructions": "Is this about billing?" },
            "team": { "type": "choice", "instructions": "Which team?", "criteria": { "billing": "charges", "shipping": "delivery" } },
            "anger": { "type": "score", "instructions": "How upset?", "criteria": ["calm", "annoyed", "furious"] }
        }
    })
}

const GOOD_DECISION: &str = r#"{"billing": {"yes": 0.97}, "team": {"probabilities": {"billing": 0.9, "shipping": 0.1}}, "anger": {"probabilities": {"0": 0.2, "1": 0.7, "2": 0.1}}}"#;

#[tokio::test]
async fn decision_answers_typed_questions() {
    let fake = Fake::new(&[("a", &["small-1b"]), ("b", &[])]).on("a", "small-1b", says(GOOD_DECISION));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    for path in ["/v1/systemone", "/api/alpha/decisions"] {
        let r = h.post_json(path, decision_request("fast")).await;
        assert_eq!(r.status, 200, "{path}: {}", String::from_utf8_lossy(&r.body));
        let body = r.json();
        assert_eq!(body["model"], "small-1b");
        assert_eq!(body["answers"]["billing"], json!({ "type": "noul", "noul": 0.97 }));
        assert_eq!(body["answers"]["team"]["choice"], "billing");
        assert_eq!(body["answers"]["team"]["probabilities"]["shipping"], json!(0.1));
        assert_eq!(body["answers"]["anger"]["score"], json!(0.9));
        assert_eq!(body["answers"]["anger"]["legend"]["2"], "furious");
        assert_eq!(body["usage"], json!({ "input_tokens": 120, "output_tokens": 12 }));
        assert_eq!(r.header("x-router-provider"), "a");
    }
}

#[tokio::test]
async fn decision_goes_to_another_model_when_the_answer_is_unusable() {
    let fake = Fake::new(&[("a", &["small-1b"]), ("b", &["small-1b"])])
        .on("a", "small-1b", says("I think it is about billing, yes!"))
        .on("b", "small-1b", says(&format!("```json\n{GOOD_DECISION}\n```")));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let r = h.post_json("/v1/systemone", decision_request("small-1b")).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("x-router-provider"), "b");
    // The unusable endpoint was paused, so it shows in the status as cooling down.
    let status = h.get("/admin/status", Some("secret")).await.json();
    let cooling = status["tiers"]["fast"][0]["endpoints"].as_array().unwrap().iter().filter(|e| e["cooldown_seconds"].as_i64().unwrap() > 0).count();
    assert_eq!(cooling, 1);
}

#[tokio::test]
async fn decision_gives_503_when_no_model_answers_usably() {
    let fake = Fake::new(&[("a", &["small-1b"]), ("b", &[])]).on("a", "small-1b", says("no json at all"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let r = h.post_json("/v1/systemone", decision_request("small-1b")).await;
    // Each unusable answer pauses its endpoint, so the last try can find everything paused.
    assert_eq!(r.status, 503);
    assert!(["all_providers_failed", "all_providers_busy"].contains(&r.json()["error"]["type"].as_str().unwrap()), "{}", r.json());
}

#[tokio::test]
async fn decision_validates_the_request() {
    let h = start(Fake::new(&[("a", &["small-1b"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    let mut no_state = decision_request("fast");
    no_state.as_object_mut().unwrap().remove("state");
    assert_eq!(h.post_json("/v1/systemone", no_state).await.status, 400);
    let boxed = json!({ "model": "fast", "state": "x", "questions": { "q": { "type": "box", "instructions": "where" } } });
    let r = h.post_json("/v1/systemone", boxed).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("box"));
    assert_eq!(h.post_json("/v1/systemone", decision_request("no-such-model")).await.status, 404);
    let anon = Request::builder().method("POST").uri("/v1/systemone").body(Body::from(decision_request("fast").to_string())).unwrap();
    assert_eq!(h.send(anon).await.status, 401);
    assert!(h.fake.calls().is_empty());
}

#[tokio::test]
async fn fake_model_answers_without_touching_any_provider() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    let r = h.chat("fake").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-router-provider"), "fake");
    let body = r.json();
    assert_eq!(body["system_fingerprint"], "fake");
    assert!(body["choices"][0]["message"]["content"].as_str().unwrap().len() > 20);
    // Same prompt, same text.
    assert_eq!(h.chat("fake").await.json()["choices"][0]["message"], body["choices"][0]["message"]);
    assert!(h.fake.calls().is_empty());
    // Listed and retrievable, but never part of a tier.
    let listed = h.get("/v1/models", Some("secret")).await.json();
    assert!(listed["data"].as_array().unwrap().iter().any(|m| m["id"] == "fake" && m["owned_by"] == "router-fake"));
    assert_eq!(h.get("/v1/models/fake", Some("secret")).await.status, 200);
    let status = h.get("/admin/status", Some("secret")).await.json();
    assert!(!status["tiers"].to_string().contains("\"fake\""));
    // Still behind the token.
    let anon = Request::builder().method("POST").uri("/v1/chat/completions").body(Body::from(json!({ "model": "fake", "messages": [] }).to_string())).unwrap();
    assert_eq!(h.send(anon).await.status, 401);
}

#[tokio::test]
async fn fake_model_streams_words_then_usage_then_done() {
    let h = start(Fake::new(&[("a", &["big-500b"]), ("b", &[])]), DEFAULT_MODELS, &["secret"]).await;
    let body = json!({ "model": "fake", "stream": true, "messages": [{ "role": "user", "content": "ciao" }] });
    let r = h.chat_with(body, &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), "text/event-stream");
    let text = String::from_utf8(r.body.clone()).unwrap();
    let events: Vec<&str> = text.split("\n\n").filter(|e| !e.is_empty()).collect();
    assert_eq!(*events.last().unwrap(), "data: [DONE]");
    let chunks: Vec<Value> = events.iter().filter_map(|e| e.strip_prefix("data: ")).filter_map(|d| serde_json::from_str(d).ok()).collect();
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let streamed: String = chunks.iter().filter_map(|c| c["choices"][0]["delta"]["content"].as_str()).collect();
    assert!(streamed.split_whitespace().count() > 5);
    // stream=true makes the server ask for usage, so the last chunk before [DONE] carries it.
    let usage = chunks.iter().find(|c| c["usage"].is_object()).expect("usage chunk");
    assert_eq!(usage["choices"], json!([]));
    assert_eq!(chunks.iter().filter(|c| c["choices"][0]["finish_reason"] == "stop").count(), 1);
}

#[tokio::test]
async fn probe_keeps_only_listed_models_that_answer_a_chat() {
    init_env();
    let fake = Arc::new(
        Fake::new(&[("a", &["good", "gone", "not-chat", "flaky"])])
            .on("a", "gone", Reply::Json(404, json!({ "error": "no such function" }), vec![]))
            .on("a", "not-chat", Reply::Json(400, json!({ "error": "no text input" }), vec![]))
            .on("a", "flaky", limited("1")),
    );
    let port = serve_fake(fake.clone()).await;
    let yaml = format!(
        "database_file: ':memory:'
scores: null
health_file: null
tiers: [{{ name: fast, min_score: 0 }}]
providers:
  - {{ name: a, base_url: 'http://127.0.0.1:{port}/a/v1', api_key_env: KEY_A, probe: true }}
"
    );
    let settings = Arc::new(Settings::from_yaml(&yaml).unwrap());
    let client = build_client(settings.connect_timeout);
    let (app, catalog) = build_app(settings, vec!["secret".into()], client).await;
    let h = Harness { app, fake: fake.clone() };

    // The listing also carries the tiers and the built-in `fake` model; look only at provider `a`.
    let listed = |r: Reply2| -> Vec<String> {
        let all: Vec<String> = r.json()["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
        ["good", "gone", "not-chat", "flaky"].iter().filter(|m| all.iter().any(|a| a == *m)).map(|m| m.to_string()).collect()
    };
    // 404 and 400 are final; a rate limit proves nothing, so the model stays out until it answers.
    assert_eq!(listed(h.get("/v1/models", Some("secret")).await), ["good"]);

    // Verdicts are remembered, the inconclusive one included until its retry time: no new probes.
    let before = fake.calls().len();
    catalog.refresh().await;
    assert_eq!(fake.calls().len() - before, 0);
    assert_eq!(listed(h.get("/v1/models", Some("secret")).await), ["good"]);
}

#[tokio::test]
async fn info_reports_pauses_and_counts() {
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &["big-500b"])]).on("a", "big-500b", limited("45"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    assert_eq!(h.chat("big-500b").await.status, 200); // a limits, b answers
    let info = h.get("/admin/info", Some("secret")).await.json();
    let paused = info["paused"].as_array().unwrap();
    assert_eq!(paused.len(), 1);
    assert_eq!((paused[0]["provider"].as_str(), paused[0]["kind"].as_str()), (Some("a"), Some("rate_limit")));
    let secs = paused[0]["remaining_seconds"].as_i64().unwrap();
    assert!((40..=45).contains(&secs), "{secs}");
    let row = |ep: &str| info["models"].as_array().unwrap().iter().find(|r| r["endpoint"] == ep).unwrap().clone();
    assert_eq!(row("a/big-500b")["failures_by_kind"]["rate_limit"], 1);
    assert_eq!((row("b/big-500b")["ok"].as_u64(), row("b/big-500b")["success_rate"].as_f64()), (Some(1), Some(1.0)));
    assert_eq!(info["providers"]["a"]["failed"], 1);
}

#[tokio::test]
async fn unrated_models_are_the_last_resort_of_a_tier() {
    // `mystery` has no score: it answers only when every rated endpoint is paused or failing.
    let fake = Fake::new(&[("a", &["small-1b", "mystery"]), ("b", &[])]).on("a", "small-1b", limited("60"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let r = h.chat("fast").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-router-model"), "mystery");
    assert_eq!(r.header("x-router-tier"), "unrated");

    // With a rated model available, the unrated one is never picked.
    let fake = Fake::new(&[("a", &["small-1b", "mystery"]), ("b", &[])]);
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    assert_eq!(h.chat("fast").await.header("x-router-model"), "small-1b");

    // fallback: none stays inside the tier.
    let fake = Fake::new(&[("a", &["small-1b", "mystery"]), ("b", &[])]).on("a", "small-1b", limited("60"));
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let r = h.chat_with(json!({ "model": "fast", "messages": [{ "role": "user", "content": "hi" }] }), &[("x-router-fallback", "none")]).await;
    assert_eq!(r.status, 503);
}

#[tokio::test]
async fn usage_is_authenticated_and_tracks_json_and_streams() {
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &[])]).on(
        "a",
        "big-500b",
        Reply::Json(200, json!({"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":4,"completion_tokens":9}}), vec![]),
    );
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    assert_eq!(h.get("/admin/usage", None).await.status, StatusCode::UNAUTHORIZED);
    h.chat_with(json!({"model":"big-500b","messages":[]}), &[]).await;
    h.fake.behaviour.lock().unwrap().insert(("a".into(),"big-500b".into()), Reply::Raw(200, "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n", "text/event-stream"));
    let stream = h.chat_with(json!({"model":"big-500b","messages":[],"stream":true}), &[]).await;
    assert_eq!(stream.status, StatusCode::OK);
    let summary = h.get("/admin/usage", Some("secret")).await.json();
    let rows = summary["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["requests"], 2);
    assert_eq!(rows[0]["prompt_tokens"], 6);
    assert_eq!(rows[0]["completion_tokens"], 12);
    assert_eq!(rows[0]["completed"], 2);
    assert!(!summary.to_string().contains("secret"));
}

#[tokio::test]
async fn sqlite_cache_migration_survives_restart_and_ignores_old_json() {
    use freeway::storage::{Store, Event, identity};
    let legacy = temp_cache("migration");
    let path = std::path::Path::new(&legacy).with_extension("sqlite3");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&legacy, json!({"fetched_at":1.0,"table":{"modelx":33.0}}).to_string()).unwrap();
    let cfg = ScoresConfig { cache_file: legacy.clone(), ..Default::default() };
    {
        let store = Arc::new(Store::open(path.to_str().unwrap()).unwrap());
        let book = ScoreBook::with_store(cfg.clone(), build_client(1.0), store.clone());
        assert_eq!(book.entries(), 1);
        store.save_scores(&legacy, &json!({"modely":40.0}), 2.0).unwrap();
        store.record(Event { key:identity("secret", &Default::default()), provider:"p".into(), model:"m".into(), usage:json!({"prompt_tokens":8}), completed:true }).unwrap();
    }
    let store = Arc::new(Store::open(path.to_str().unwrap()).unwrap());
    let book = ScoreBook::with_store(cfg, build_client(1.0), store.clone());
    assert_eq!(book.fetched_at(), Some(2.0));
    assert_eq!(book.lookup(&["modely".to_string()].into_iter().collect()), Some(40.0));
    assert_eq!(book.lookup(&["modelx".to_string()].into_iter().collect()), None);
    assert_eq!(store.summary().unwrap()["data"][0]["prompt_tokens"], 8);
    assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(&legacy).unwrap()).unwrap()["table"]["modelx"],33.0);
    drop(book);
    drop(store);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(legacy);
}


#[tokio::test]
async fn oversized_stream_preamble_falls_back() {
    let oversized: &'static str = Box::leak(":".repeat(1048577).into_boxed_str());
    let fake = Fake::new(&[("a", &["big-500b"]), ("b", &["big-500b"])])
        .on("a", "big-500b", Reply::Raw(200, oversized, "text/event-stream"))
        .on(
            "b",
            "big-500b",
            Reply::Raw(200, "data: {\"choices\":[]}\n\ndata: [DONE]\n\n", "text/event-stream"),
        );
    let h = start(fake, DEFAULT_MODELS, &["secret"]).await;
    let reply = h.chat_with(json!({"model":"big-500b","messages":[],"stream":true}), &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["x-router-provider"], "b");
    assert_eq!(h.fake.calls().len(), 2);
}
