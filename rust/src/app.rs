//! OpenAI-compatible HTTP surface.

use crate::catalog::{Catalog, Model};
use crate::config::Settings;
use crate::fake;
use crate::health::Health;
use crate::router::{Attempt, CompleteError, Router, SuccessBody};
use crate::scores::ScoreBook;
use crate::systemone;
use crate::util::{now, truncate, truthy};
use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct AppState {
    settings: Arc<Settings>,
    catalog: Arc<Catalog>,
    health: Arc<Health>,
    router: Arc<Router>,
    tokens: Arc<Vec<String>>,
}

/// Tokens accepted from callers: ROUTER_TOKENS (comma separated) plus the older ROUTER_TOKEN.
pub fn load_tokens() -> Vec<String> {
    let raw = ["ROUTER_TOKENS", "ROUTER_TOKEN"].iter().map(|v| std::env::var(v).unwrap_or_default()).collect::<Vec<_>>().join(",");
    parse_tokens(&raw)
}

/// Comma separated list: trimmed, empties dropped, duplicates removed, order kept.
pub fn parse_tokens(raw: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for t in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !tokens.iter().any(|x| x == t) {
            tokens.push(t.to_string());
        }
    }
    tokens
}


/// Builds the app and loads the catalog once before returning.
pub async fn build_app(settings: Arc<Settings>, tokens: Vec<String>, client: reqwest::Client) -> (axum::Router, Arc<Catalog>) {
    let scores = settings.scores.clone().map(|cfg| Arc::new(ScoreBook::new(cfg, client.clone())));
    let catalog = Arc::new(Catalog::new(settings.clone(), client.clone(), scores));
    let health = Arc::new(Health::new());
    let router = Arc::new(Router::new(settings.clone(), catalog.clone(), health.clone(), client));
    if tokens.is_empty() {
        tracing::warn!("ROUTER_TOKENS is not set: the server is open to anyone who can reach it");
    } else {
        tracing::info!("{} access token(s) configured", tokens.len());
    }
    catalog.refresh().await;

    let state = AppState { settings, catalog: catalog.clone(), health, router, tokens: Arc::new(tokens) };
    let app = axum::Router::new()
        .route("/health", get(health_check))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{*id}", get(get_model))
        .route("/v1/chat/completions", post(chat))
        // Decision API (typed questions -> probabilities). The second path is OpenRouter's name for it.
        .route("/v1/systemone", post(systemone))
        .route("/api/alpha/decisions", post(systemone))
        .route("/admin/status", get(status))
        .route("/admin/refresh", post(refresh))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);
    (app, catalog)
}

/// Re-reads the provider lists every `minutes` minutes, forever.
pub fn spawn_refresh_loop(catalog: Arc<Catalog>, minutes: f64) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs_f64(minutes * 60.0)).await;
            catalog.refresh().await;
        }
    });
}

fn error_response(status: StatusCode, message: impl Into<String>, kind: &str) -> Response {
    (status, Json(json!({ "error": { "message": message.into(), "type": kind } }))).into_response()
}

fn bearer_or_key(headers: &HeaderMap) -> String {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    if auth.len() >= 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
        return auth[7..].to_string();
    }
    headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

async fn auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if !st.tokens.is_empty() {
        let given = bearer_or_key(req.headers());
        // Compare against every token (no early exit) so timing does not reveal which one was close.
        let ok = st.tokens.iter().fold(false, |acc, t| acc | bool::from(given.as_bytes().ct_eq(t.as_bytes())));
        if !ok {
            return error_response(StatusCode::UNAUTHORIZED, "invalid or missing token", "authentication_error");
        }
    }
    next.run(req).await
}

async fn health_check() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

fn tier_object(name: &str, created: i64) -> Value {
    json!({ "id": name, "object": "model", "created": created, "owned_by": "router-tier" })
}

fn fake_object(created: i64) -> Value {
    json!({ "id": fake::MODEL, "object": "model", "created": created, "owned_by": "router-fake" })
}

fn model_object(m: &Model, id: &str, created: i64) -> Value {
    json!({
        "id": id,
        "object": "model",
        "created": created,
        "owned_by": "router",
        "tier": m.tier,
        "score": m.score,
        "providers": m.endpoints.iter().map(|e| e.reference()).collect::<Vec<_>>(),
    })
}

async fn list_models(State(st): State<AppState>) -> Json<Value> {
    let created = now() as i64;
    let mut data: Vec<Value> = st.settings.tiers.iter().map(|t| tier_object(&t.name, created)).collect();
    data.extend(st.catalog.all_models().iter().map(|m| model_object(m, &m.name, created)));
    data.push(fake_object(created));
    Json(json!({ "object": "list", "data": data }))
}

/// `GET /v1/models/{id}`: a tier, a model (any spelling the catalog knows) or `<provider>/<provider model id>`.
/// The id may contain slashes, as OpenRouter ids do.
async fn get_model(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let created = now() as i64;
    if st.settings.tier(&id).is_some() {
        return Json(tier_object(&id, created)).into_response();
    }
    if id == fake::MODEL {
        return Json(fake_object(created)).into_response();
    }
    if let Some((m, _)) = st.catalog.find_endpoint(&id) {
        return Json(model_object(&m, &id, created)).into_response();
    }
    if let Some(m) = st.catalog.find_model(&id) {
        return Json(model_object(&m, &m.name, created)).into_response();
    }
    let tiers: Vec<&str> = st.settings.tiers.iter().map(|t| t.name.as_str()).collect();
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": {
            "message": format!("The model {id:?} does not exist. Tiers: {}. See GET /v1/models.", tiers.join(", ")),
            "type": "invalid_request_error",
            "param": "model",
            "code": "model_not_found",
        }})),
    )
        .into_response()
}

async fn status(State(st): State<AppState>) -> Json<Value> {
    let providers = st.catalog.provider_status();
    let book = st.catalog.scores();
    let mut tiers = Map::new();
    for t in &st.settings.tiers {
        let models: Vec<Value> = st
            .catalog
            .in_tier(&t.name)
            .iter()
            .map(|m| {
                let endpoints: Vec<Value> = m
                    .endpoints
                    .iter()
                    .map(|e| {
                        let mut o = Map::new();
                        o.insert("ref".into(), json!(e.reference()));
                        o.insert("cooldown_seconds".into(), json!(st.health.cooldown(&e.provider, &e.raw_id).round() as i64));
                        if let Some(s) = st.health.stat(&e.provider, &e.raw_id) {
                            o.insert("ok".into(), json!(s.ok));
                            o.insert("failed".into(), json!(s.failed));
                            o.insert("last_error".into(), json!(s.last_error));
                            o.insert("last_latency".into(), json!(s.last_latency));
                        }
                        Value::Object(o)
                    })
                    .collect();
                json!({ "model": m.name, "score": m.score, "endpoints": endpoints })
            })
            .collect();
        tiers.insert(t.name.clone(), Value::Array(models));
    }
    Json(json!({
        "providers": { "active": providers.active, "skipped": providers.skipped, "discovery_errors": providers.errors },
        "scores": {
            "source": "artificial_analysis",
            "entries": book.map_or(0, |b| b.entries()),
            "fetched_at": book.and_then(|b| b.fetched_at()),
            "error": match book { Some(b) => b.error(), None => Some("scores disabled in config".to_string()) },
        },
        // Callable by name only: no benchmark score and no manual rating.
        "unrated": st.catalog.unrated().iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        "tiers": tiers,
    }))
}

async fn refresh(State(st): State<AppState>) -> Json<Value> {
    st.catalog.refresh().await;
    Json(json!({ "models": st.catalog.all_models().len() }))
}

fn with_headers(mut resp: Response, extra: Vec<(&'static str, String)>) -> Response {
    for (k, v) in extra {
        if let Ok(val) = HeaderValue::from_str(&v) {
            resp.headers_mut().insert(HeaderName::from_static(k), val);
        }
    }
    resp
}

/// `x-router-fallback` and `x-router-session`, validated.
fn routing_headers(headers: &HeaderMap) -> Result<(String, Option<String>), &'static str> {
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let fallback = header_str("x-router-fallback").unwrap_or_else(|| "auto".into()).to_lowercase();
    if !["auto", "none", "tier"].contains(&fallback.as_str()) {
        return Err("x-router-fallback must be auto, none or tier");
    }
    Ok((fallback, header_str("x-router-session")))
}

async fn chat(State(st): State<AppState>, headers: HeaderMap, raw: Bytes) -> Response {
    let invalid = |msg: &str| error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error");
    let Ok(mut body) = serde_json::from_slice::<Value>(&raw) else { return invalid("body is not valid JSON") };
    if !body.is_object() || !truthy(body.get("model")) || !body["messages"].is_array() {
        return invalid("`model` and `messages` are required");
    }
    let (fallback, session) = match routing_headers(&headers) {
        Ok(v) => v,
        Err(msg) => return invalid(msg),
    };
    if truthy(body.get("stream")) && body.get("stream_options").is_none() {
        // Needed so streamed answers still carry token counts when providers support it.
        body["stream_options"] = json!({ "include_usage": true });
    }
    if body["model"] == fake::MODEL {
        return fake_chat(&body);
    }

    match st.router.complete(body, &fallback, session.as_deref()).await {
        Ok(success) => {
            let extra = success.headers();
            match success.body {
                SuccessBody::Json(data) => with_headers(Json(data).into_response(), extra),
                SuccessBody::Stream(stream) => {
                    let resp = Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(Body::from_stream(stream))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                    with_headers(resp, extra)
                }
            }
        }
        Err(e) => complete_error(&st, e),
    }
}

/// The built-in `fake` model: made-up text, no provider contacted.
fn fake_chat(body: &Value) -> Response {
    let resp = if truthy(body.get("stream")) {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(fake::stream(body)))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        Json(fake::completion(body)).into_response()
    };
    with_headers(resp, fake::headers())
}

/// How many different models are asked before giving up on getting a usable decision.
const DECISION_TRIES: usize = 3;

/// `POST /v1/systemone` (alias `/api/alpha/decisions`): answer typed questions about a state.
async fn systemone(State(st): State<AppState>, headers: HeaderMap, raw: Bytes) -> Response {
    let invalid = |msg: &str| error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error");
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else { return invalid("body is not valid JSON") };
    let request = match systemone::parse_request(&body) {
        Ok(r) => r,
        Err(msg) => return invalid(&msg),
    };
    let (fallback, session) = match routing_headers(&headers) {
        Ok(v) => v,
        Err(msg) => return invalid(msg),
    };

    let messages = systemone::build_messages(&request);
    let mut last_problem = String::new();
    for _ in 0..DECISION_TRIES {
        let chat_body = json!({ "model": request.model, "messages": messages, "temperature": 0, "stream": false });
        let success = match st.router.complete(chat_body, &fallback, session.as_deref()).await {
            Ok(s) => s,
            Err(e) => return complete_error(&st, e),
        };
        let extra = success.headers();
        let SuccessBody::Json(data) = &success.body else { continue };
        let answers = systemone::completion_text(data)
            .ok_or_else(|| "empty answer".to_string())
            .and_then(|text| systemone::read_answers(&request.questions, &text));
        match answers {
            Ok(answers) => {
                let tokens = |k: &str| data["usage"][k].as_u64().unwrap_or(0);
                let out = json!({
                    "model": success.candidate.model.name,
                    "answers": answers,
                    "usage": { "input_tokens": tokens("prompt_tokens"), "output_tokens": tokens("completion_tokens") },
                });
                return with_headers(Json(out).into_response(), extra);
            }
            Err(problem) => {
                // The next try goes to another endpoint: this one is paused for a moment.
                tracing::info!("decision from {} unusable: {problem}", success.candidate.endpoint.reference());
                st.router.reject(&success.candidate, &format!("unusable decision: {problem}"));
                last_problem = problem;
            }
        }
    }
    error_response(
        StatusCode::SERVICE_UNAVAILABLE, // not 502: Cloudflare would replace the JSON with an HTML page
        format!("no model returned a usable decision after {DECISION_TRIES} tries: {last_problem}"),
        "all_providers_failed",
    )
}

/// The HTTP answer for a request the router could not serve.
fn complete_error(st: &AppState, err: CompleteError) -> Response {
    match err {
        CompleteError::UnknownModel(name) => {
            let tiers: Vec<&str> = st.settings.tiers.iter().map(|t| t.name.as_str()).collect();
            error_response(
                StatusCode::NOT_FOUND,
                format!("unknown model or tier {name:?}. Tiers: {}. See GET /v1/models.", tiers.join(", ")),
                "invalid_request_error",
            )
        }
        CompleteError::Exhausted(e) => {
            if let (Some(body), Some(status)) = (&e.last_body, e.last_status) {
                // Every endpoint rejected the request itself: pass the explanation through.
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
                return (code, Json(body.clone())).into_response();
            }
            // Keep the answer readable: the provider's error text is cut (the full text is in the server log).
            let short: Vec<Attempt> = e.attempts.iter().map(|a| Attempt { error: truncate(&a.error, 120), ..a.clone() }).collect();
            let tried = serde_json::to_value(&short).unwrap_or(Value::Null);
            if e.candidates == 0 && e.skipped > 0 {
                let retry = e.retry_in.unwrap_or(1.0).round().max(1.0) as u64;
                let mut resp = (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": {
                        "message": "every matching endpoint is cooling down",
                        "type": "all_providers_busy",
                        "retry_in_seconds": retry,
                        "tried": tried,
                    }})),
                )
                    .into_response();
                resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(retry));
                return resp;
            }
            if e.candidates == 0 {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no endpoint can serve this request (no providers loaded, or none supports the tools/images it needs)",
                    "no_endpoint",
                );
            }
            // 503, never 502/504: Cloudflare replaces an origin's 502 and 504 with its own HTML page,
            // so the caller would never see this JSON. The first line of the message is the whole story.
            let mut resp = (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": { "message": exhausted_message(&e.attempts), "type": "all_providers_failed", "tried": tried } })),
            )
                .into_response();
            // Come back when the first paused endpoint is free again.
            if let Some(wait) = e.attempts.iter().map(|a| a.cooldown_seconds).filter(|s| *s > 0).min() {
                resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait as u64));
            }
            resp
        }
    }
}

/// "all 8 endpoints tried failed (5 rate_limit, 2 server, 1 unavailable)".
fn exhausted_message(attempts: &[Attempt]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for a in attempts {
        match counts.iter_mut().find(|(k, _)| *k == a.kind) {
            Some((_, n)) => *n += 1,
            None => counts.push((a.kind, 1)),
        }
    }
    let detail: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
    format!("all {} endpoints tried failed ({}). Details in error.tried; GET /admin/status shows the pauses.", attempts.len(), detail.join(", "))
}

#[cfg(test)]
mod tests {
    use super::parse_tokens;

    #[test]
    fn token_list_parsing() {
        assert_eq!(parse_tokens("a, b ,,a,c"), ["a", "b", "c"]);
        assert!(parse_tokens(" , ").is_empty());
        assert!(parse_tokens("").is_empty());
    }
}
