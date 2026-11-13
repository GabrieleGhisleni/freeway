//! Picking which provider endpoints to try for a request, and trying them in order.

use crate::catalog::{Catalog, Endpoint, Model};
use crate::config::Settings;
use crate::health::{FailKind, Health};
use crate::util::{now, truncate, truthy};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;
type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

#[derive(Clone)]
pub struct Candidate {
    pub model: Arc<Model>,
    pub endpoint: Arc<Endpoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub provider: String,
    pub upstream_model: String,
    pub model: String,
    pub kind: &'static str,
    pub status: Option<u16>,
    pub error: String,
    pub cooldown_seconds: i64,
}

#[derive(Default)]
pub struct Plan {
    pub candidates: Vec<Candidate>,
    /// Soonest cooldown expiry among skipped endpoints.
    pub retry_in: Option<f64>,
    pub skipped: usize,
}

pub enum SuccessBody {
    Json(Value),
    Stream(ByteStream),
}

pub struct Success {
    pub candidate: Candidate,
    /// Failed attempts made before this one.
    pub failed_attempts: usize,
    pub body: SuccessBody,
}

impl Success {
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let c = &self.candidate;
        vec![
            ("x-router-model", c.model.name.clone()),
            ("x-router-tier", c.model.tier.clone().unwrap_or_else(|| "unrated".into())),
            ("x-router-provider", c.endpoint.provider.clone()),
            ("x-router-upstream-model", c.endpoint.raw_id.clone()),
            ("x-router-attempts", (self.failed_attempts + 1).to_string()),
        ]
    }
}

pub struct Exhausted {
    pub attempts: Vec<Attempt>,
    pub candidates: usize,
    pub skipped: usize,
    pub retry_in: Option<f64>,
    /// Upstream body of the last failure, when every failure was the request's fault.
    pub last_body: Option<Value>,
    pub last_status: Option<u16>,
}

pub enum CompleteError {
    UnknownModel(String),
    Exhausted(Box<Exhausted>),
}

struct UpstreamFailure {
    kind: FailKind,
    status: Option<u16>,
    message: String,
    retry_after: Option<f64>,
    body: Option<Value>,
}

impl UpstreamFailure {
    fn new(kind: FailKind, status: Option<u16>, message: impl Into<String>) -> Self {
        Self { kind, status, message: message.into(), retry_after: None, body: None,
        }
    }
}

pub fn classify(status: u16) -> FailKind {
    match status {
        429 => FailKind::RateLimit,
        401 => FailKind::Auth,
        402..=404 => FailKind::Unavailable,
        408 => FailKind::Server,
        s if s >= 500 => FailKind::Server,
        _ => FailKind::BadRequest,
    }
}

/// Read a Retry-After style value: delta seconds, or an epoch in s or ms.
fn seconds_from(raw: f64) -> Option<f64> {
    let n = if raw > 1e12 {
        raw / 1000.0 - now()
    } else if raw > 1e9 {
        raw - now()
    } else {
        raw
    };
    (n > 0.0).then_some(n)
}

fn value_seconds(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().and_then(seconds_from),
        Value::String(s) => s.trim().parse::<f64>().ok().and_then(seconds_from),
        _ => None,
    }
}

/// Some providers (Google) wrap error bodies in a one-element array.
fn unwrap_body(v: &Value) -> &Value {
    match v {
        Value::Array(a) if a.len() == 1 && a[0].is_object() => &a[0],
        _ => v,
    }
}

fn retry_after(headers: &HeaderMap, body: Option<&Value>) -> Option<f64> {
    let from_header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|s| value_seconds(&json!(s)));
    from_header("retry-after").or_else(|| from_header("x-ratelimit-reset")).or_else(|| {
        let hdrs = &unwrap_body(body?)["error"]["metadata"]["headers"];
        value_seconds(&hdrs["Retry-After"]).or_else(|| value_seconds(&hdrs["X-RateLimit-Reset"]))
    })
}

fn error_text(body: Option<&Value>, fallback: &str) -> String {
    let Some(body) = body.map(unwrap_body).filter(|b| b.is_object()) else { return fallback.to_string();
    };
    let err = body.get("error").unwrap_or(body);
    match err {
        Value::Object(o) => match o.get("message") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => err.to_string(),
        },
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Some providers answer 200 and put the real status inside `error.code`.
fn failure_from_error_body(body: &Value, headers: &HeaderMap) -> UpstreamFailure {
    let body = unwrap_body(body);
    let code = body["error"]["code"].as_u64().map(|c| c as u16);
    let kind = match code {
        Some(c) if c >= 400 => classify(c),
        _ => FailKind::Server,
    };
    UpstreamFailure {
        kind,
        status: Some(code.unwrap_or(200)),
        message: error_text(Some(body), "error"),
        retry_after: retry_after(headers, Some(body)),
        body: Some(body.clone()),
    }
}

/// (needs tool calling, needs image input) for an OpenAI chat request.
pub fn needs(body: &Value) -> (bool, bool) {
    let tools = truthy(body.get("tools"));
    let vision = body["messages"].as_array().into_iter().flatten().any(|m| {
        m["content"].as_array().into_iter().flatten().any(|p| matches!(p["type"].as_str(), Some("image_url" | "input_image")))
    });
    (tools, vision)
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Ok,
    Empty,
    Error(Value),
}

/// Look at the complete SSE lines received so far.
///
/// None while no `data:` line has fully arrived.
fn first_event(buf: &[u8]) -> Option<Verdict> {
    let mut lines: Vec<&[u8]> = buf.split(|b| *b == b'\n').collect();
    lines.pop(); // the last piece has no newline yet
    for line in lines {
        let line = line.trim_ascii();
        let Some(payload) = line.strip_prefix(b"data:") else { continue };
        let payload = payload.trim_ascii();
        if payload == b"[DONE]" {
            return Some(Verdict::Empty);
        }
        return Some(match serde_json::from_slice::<Value>(payload) {
            Ok(obj) if obj.is_object() && truthy(obj.get("error")) => Verdict::Error(obj),
            _ => Verdict::Ok,
        });
    }
    None
}

fn network_failure(e: reqwest::Error) -> UpstreamFailure {
    if e.is_timeout() {
        UpstreamFailure::new(FailKind::Server, None, format!("timeout ({e})"))
    } else {
        UpstreamFailure::new(FailKind::Server, None, format!("network error: {e}"))
    }
}

fn interrupted_event(ep: &Endpoint) -> Bytes {
    let err = json!({ "error": { "message": format!("upstream stream interrupted ({})", ep.reference()), "type": "upstream_error" } });
    Bytes::from(format!("data: {err}\n\n"))
}

pub struct Router {
    settings: Arc<Settings>,
    catalog: Arc<Catalog>,
    health: Arc<Health>,
    client: reqwest::Client,
    /// session id -> (model name, expiry)
    sessions: Mutex<HashMap<String, (String, f64)>>,
}

impl Router {
    pub fn new(settings: Arc<Settings>, catalog: Arc<Catalog>, health: Arc<Health>, client: reqwest::Client) -> Self {
        Self { settings, catalog, health, client, sessions: Mutex::new(HashMap::new()),
        }
    }

    // ---- choosing -------------------------------------------------------

    /// Models with no score and no manual rating, in a stable order: the last resort of every tier.
    fn unrated_tail(&self) -> Vec<Arc<Model>> {
        let mut found = self.catalog.unrated();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }

    fn chain(&self, tier: &str, with_fallback: bool) -> Vec<Arc<Model>> {
        let mut tiers = vec![tier.to_string()];
        if with_fallback
            && let Some(cfg) = self.settings.tier(tier) {
                tiers.extend(cfg.fallback.iter().cloned());
            }
        tiers.iter().flat_map(|t| self.catalog.in_tier(t)).collect()
    }

    fn sticky(&self, session: Option<&str>) -> Option<String> {
        let session = session?;
        let mut sessions = self.sessions.lock().unwrap();
        let (name, expires) = sessions.get(session)?.clone();
        if expires < now() {
            sessions.remove(session);
            return None;
        }
        Some(name)
    }

    fn remember(&self, session: Option<&str>, model: &Model) {
        let Some(session) = session else { return };
        let t = now();
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= 5000 {
            sessions.retain(|_, v| v.1 > t);
        }
        if !sessions.contains_key(session) && sessions.len() >= 5000 {
            if let Some(oldest) = sessions.iter().min_by(|a, b| a.1.1.total_cmp(&b.1.1)).map(|(k, _)| k.clone()) {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(session.to_string(), (model.name.clone(), t + self.settings.session_ttl_minutes * 60.0));
    }

    /// Ordered endpoints to try for `requested`.
    ///
    /// `requested` is a tier name, a model name (any spelling the catalog knows), or
    /// `<provider>/<provider model id>` to pin one endpoint.
    /// fallback: "auto" (tiers fall through their chain, named models do not), "none"
    /// (never leave the requested tier/model), "tier" (a named model falls back to the
    /// rest of its tier chain).
    pub fn plan(&self, requested: &str, fallback: &str, session: Option<&str>, needs_tools: bool, needs_vision: bool) -> Result<Plan, String> {
        let mut pairs: Vec<(Arc<Model>, Arc<Endpoint>)> = Vec::new();
        if self.settings.tier(requested).is_some() {
            let mut models = self.chain(requested, fallback != "none");
            let sticky = self.sticky(session);
            // A conversation keeps the model that answered it last time, while it is usable.
            models.sort_by_key(|m| Some(&m.name) != sticky.as_ref());
            if fallback != "none" {
                // Last resort: models with no score rather than an error. Never ahead of a rated one.
                models.extend(self.unrated_tail());
            }
            for m in models {
                for e in &m.endpoints {
                    pairs.push((m.clone(), e.clone()));
                }
            }
        } else {
            let pinned = self.catalog.find_endpoint(requested);
            let model = match &pinned {
                Some((m, _)) => m.clone(),
                None => self.catalog.find_model(requested).ok_or_else(|| requested.to_string())?,
            };
            match &pinned {
                Some((m, e)) => {
                    pairs.push((m.clone(), e.clone()));
                    if fallback == "tier" {
                        pairs.extend(m.endpoints.iter().filter(|x| x.reference() != e.reference()).map(|x| (m.clone(), x.clone())));
                    }
                }
                None => pairs.extend(model.endpoints.iter().map(|e| (model.clone(), e.clone()))),
            }
            if fallback == "tier" {
                // An unrated model has no tier to fall back through, only the other unrated ones.
                if let Some(tier) = &model.tier {
                    for m in self.chain(tier, true).into_iter().filter(|m| m.name != model.name) {
                        pairs.extend(m.endpoints.iter().map(|e| (m.clone(), e.clone())));
                    }
                }
                for m in self.unrated_tail().into_iter().filter(|m| m.name != model.name) {
                    pairs.extend(m.endpoints.iter().map(|e| (m.clone(), e.clone())));
                }
            }
        }

        let mut plan = Plan::default();
        for (model, ep) in pairs {
            if (needs_tools && ep.tools == Some(false)) || (needs_vision && ep.vision == Some(false)) {
                continue;
            }
            let wait = self.health.cooldown(&ep.provider, &ep.raw_id);
            if wait > 0.0 {
                plan.skipped += 1;
                plan.retry_in = Some(plan.retry_in.map_or(wait, |r| r.min(wait)));
                continue;
            }
            plan.candidates.push(Candidate { model, endpoint: ep });
        }
        Ok(plan)
    }

    /// The endpoint answered, but with something the caller cannot use: pause it like a failed call.
    pub fn reject(&self, cand: &Candidate, message: &str) {
        let ep = &cand.endpoint;
        self.health.fail(&ep.provider, &ep.raw_id, FailKind::Server, message, None);
    }

    // ---- calling --------------------------------------------------------

    fn request_parts(&self, cand: &Candidate) -> (String, HeaderMap) {
        let ep = &cand.endpoint;
        let url = format!("{}/chat/completions", ep.base_url.trim_end_matches('/'));
        let mut headers = HeaderMap::new();
        if let Some(p) = self.settings.providers.iter().find(|p| p.name == ep.provider) {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", p.api_key().unwrap_or_default())) {
                headers.insert(AUTHORIZATION, v);
            }
            for (k, v) in &p.headers {
                if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                    headers.insert(n, val);
                }
            }
        }
        (url, headers)
    }

    async fn call_json(&self, cand: &Candidate, body: &Value) -> Result<Value, UpstreamFailure> {
        let (url, headers) = self.request_parts(cand);
        let resp = self
            .client
            .post(url)
            .headers(headers)
            .json(body)
            .timeout(Duration::from_secs_f64(self.settings.request_timeout))
            .send()
            .await
            .map_err(network_failure)?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        let text = resp.text().await.map_err(network_failure)?;
        let data: Option<Value> = serde_json::from_str(&text).ok();
        if status != 200 {
            return Err(UpstreamFailure {
                kind: classify(status),
                status: Some(status),
                message: error_text(data.as_ref(), &truncate(&text, 300)),
                retry_after: retry_after(&resp_headers, data.as_ref()),
                body: data,
            });
        }
        let Some(data) = data.filter(Value::is_object) else {
            return Err(UpstreamFailure::new(FailKind::Server, Some(200), "answer is not JSON"));
        };
        if truthy(data.get("error")) {
            return Err(failure_from_error_body(&data, &resp_headers));
        }
        if !truthy(data.get("choices")) {
            return Err(UpstreamFailure::new(FailKind::Server, Some(200), "answer has no choices"));
        }
        Ok(data)
    }

    /// Open the stream and wait for the first real chunk before committing.
    ///
    /// Once bytes have gone to the caller there is no switching provider, so
    /// everything that can be detected early is detected here.
    async fn call_stream(&self, cand: &Candidate, body: &Value) -> Result<ByteStream, UpstreamFailure> {
        let (url, headers) = self.request_parts(cand);
        let first_token = Duration::from_secs_f64(self.settings.first_token_timeout);
        let idle = Duration::from_secs_f64(self.settings.stream_idle_timeout);

        let opening = async {
            let resp = self.client.post(url).headers(headers).json(body).send().await.map_err(network_failure)?;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            if status != 200 {
                let raw = resp.bytes().await.map_err(network_failure)?;
                let data: Option<Value> = serde_json::from_slice(&raw).ok();
                return Err(UpstreamFailure {
                    kind: classify(status),
                    status: Some(status),
                    message: error_text(data.as_ref(), &truncate(&String::from_utf8_lossy(&raw), 300)),
                    retry_after: retry_after(&resp_headers, data.as_ref()),
                    body: data,
                });
            }
            let mut stream: UpstreamStream = Box::pin(resp.bytes_stream());
            let mut buf: Vec<u8> = Vec::new();
            let mut verdict = None;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(network_failure)?;
                if buf.len() + chunk.len() > 1048576 {
                    return Err(UpstreamFailure::new(FailKind::Server, Some(200), "stream preamble exceeds 1 MiB"));
                }
                buf.extend_from_slice(&chunk);
                if let Some(v) = first_event(&buf) {
                    verdict = Some(v);
                    break;
                }
            }
            Ok((stream, buf, verdict, resp_headers))
        };
        let (mut stream, buf, verdict, resp_headers) = match tokio::time::timeout(first_token, opening).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(UpstreamFailure::new(
                    FailKind::Server,
                    None,
                    format!("no first token within {}s", self.settings.first_token_timeout),
                ));
            }
        };
        match verdict {
            None => {
                // The stream ended without a single data line.
                return Err(match serde_json::from_slice::<Value>(&buf) {
                    Ok(data) if data.is_object() && truthy(data.get("error")) => failure_from_error_body(&data, &resp_headers),
                    _ => UpstreamFailure::new(FailKind::Server, Some(200), "stream ended before any data"),
                });
            }
            Some(Verdict::Error(obj)) => return Err(failure_from_error_body(&obj, &resp_headers)),
            Some(Verdict::Empty) => return Err(UpstreamFailure::new(FailKind::Server, Some(200), "stream finished with no content")),
            Some(Verdict::Ok) => {}
        }

        let health = self.health.clone();
        let ep = cand.endpoint.clone();
        Ok(Box::pin(async_stream::stream! {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(buf));
            loop {
                match tokio::time::timeout(idle, stream.next()).await {
                    Ok(Some(Ok(chunk))) => yield Ok(chunk),
                    Ok(None) => break,
                    Ok(Some(Err(e))) => {
                        health.fail(&ep.provider, &ep.raw_id, FailKind::Server, &format!("stream broke: {e}"), None);
                        tracing::warn!("stream from {} broke mid-answer: {e}", ep.reference());
                        yield Ok(interrupted_event(&ep));
                        break;
                    }
                    Err(_) => {
                        health.fail(&ep.provider, &ep.raw_id, FailKind::Server, "stream went silent", None);
                        tracing::warn!("stream from {} went silent", ep.reference());
                        yield Ok(interrupted_event(&ep));
                        break;
                    }
                }
            }
        }))
    }

    pub async fn complete(&self, mut body: Value, fallback: &str, session: Option<&str>) -> Result<Success, CompleteError> {
        let requested = body["model"].as_str().unwrap_or("").to_string();
        let stream = truthy(body.get("stream"));
        let (needs_tools, needs_vision) = needs(&body);
        let plan = self.plan(&requested, fallback, session, needs_tools, needs_vision).map_err(CompleteError::UnknownModel)?;

        let mut attempts: Vec<Attempt> = Vec::new();
        let mut last: Option<UpstreamFailure> = None;
        let mut rate_limited: HashMap<String, u32> = HashMap::new();
        for cand in plan.candidates.iter().take(self.settings.max_attempts) {
            let ep = &cand.endpoint;
            if self.health.cooldown(&ep.provider, &ep.raw_id) > 0.0 {
                continue; // paused earlier in this very request (provider-wide limit)
            }
            let started = Instant::now();
            body["model"] = json!(ep.raw_id);
            let result = if stream {
                self.call_stream(cand, &body).await.map(SuccessBody::Stream)
            } else {
                self.call_json(cand, &body).await.map(SuccessBody::Json)
            };
            match result {
                Err(f) => {
                    let cooldown = self.health.fail(&ep.provider, &ep.raw_id, f.kind, &f.message, f.retry_after);
                    if f.kind == FailKind::RateLimit {
                        // Two different models limited in a row: the limit is the account's, so stop asking this provider.
                        let n = rate_limited.entry(ep.provider.clone()).or_insert(0);
                        *n += 1;
                        if *n >= 2 {
                            self.health.cool_provider(&ep.provider, cooldown);
                        }
                    }
                    tracing::info!(
                        "{requested} -> {} failed: {} {} ({}){}",
                        ep.reference(),
                        f.kind.as_str(),
                        f.status.map_or("-".to_string(), |s| s.to_string()),
                        truncate(&f.message, 120),
                        if cooldown > 0.0 { format!(", cooling down {cooldown:.0}s") } else { String::new() }
                    );
                    attempts.push(Attempt {
                        provider: ep.provider.clone(),
                        upstream_model: ep.raw_id.clone(),
                        model: cand.model.name.clone(),
                        kind: f.kind.as_str(),
                        status: f.status,
                        error: truncate(&f.message, 300),
                        cooldown_seconds: cooldown.round() as i64,
                    });
                    last = Some(f);
                }
                Ok(body_out) => {
                    let elapsed = started.elapsed().as_secs_f64();
                    self.health.ok(&ep.provider, &ep.raw_id, elapsed);
                    self.remember(session, &cand.model);
                    tracing::info!(
                        "{requested} -> {} [{}, tier {}] ok in {elapsed:.1}s after {} failed attempt(s)",
                        ep.reference(),
                        cand.model.name,
                        cand.model.tier.as_deref().unwrap_or("unrated"),
                        attempts.len()
                    );
                    return Ok(Success { candidate: cand.clone(), failed_attempts: attempts.len(), body: body_out,
                    });
                }
            }
        }
        let client_fault = !attempts.is_empty() && attempts.iter().all(|a| a.kind == FailKind::BadRequest.as_str());
        let (last_body, last_status) = match (&last, client_fault) {
            (Some(f), true) => (f.body.clone(), f.status),
            _ => (None, None),
        };
        Err(CompleteError::Exhausted(Box::new(Exhausted {
            attempts,
            candidates: plan.candidates.len(),
            skipped: plan.skipped,
            retry_in: plan.retry_in,
            last_body,
            last_status,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_statuses() {
        assert_eq!(classify(429), FailKind::RateLimit);
        assert_eq!(classify(401), FailKind::Auth);
        assert_eq!(classify(403), FailKind::Unavailable);
        assert_eq!(classify(404), FailKind::Unavailable);
        assert_eq!(classify(503), FailKind::Server);
        assert_eq!(classify(408), FailKind::Server);
        assert_eq!(classify(400), FailKind::BadRequest);
        assert_eq!(classify(422), FailKind::BadRequest);
    }

    #[test]
    fn seconds_accept_delta_epoch_and_millis() {
        assert_eq!(seconds_from(30.0), Some(30.0));
        assert_eq!(seconds_from(0.0), None);
        let in_a_minute = now() + 60.0;
        assert!((seconds_from(in_a_minute).unwrap() - 60.0).abs() < 2.0);
        assert!((seconds_from(in_a_minute * 1000.0).unwrap() - 60.0).abs() < 2.0);
    }

    #[test]
    fn retry_after_from_headers_then_body() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("45"));
        assert_eq!(retry_after(&h, None), Some(45.0));
        let body = json!({ "error": { "metadata": { "headers": { "X-RateLimit-Reset": "90" } } } });
        assert_eq!(retry_after(&HeaderMap::new(), Some(&body)), Some(90.0));
        assert_eq!(retry_after(&HeaderMap::new(), Some(&json!({}))), None);
    }

    #[test]
    fn error_text_reads_wrapped_and_plain_bodies() {
        assert_eq!(error_text(Some(&json!({ "error": { "message": "slow down" } })), "x"), "slow down");
        assert_eq!(error_text(Some(&json!([{ "error": { "message": "wrapped" } }])), "x"), "wrapped");
        assert_eq!(error_text(Some(&json!({ "error": "plain" })), "x"), "plain");
        assert_eq!(error_text(None, "fallback"), "fallback");
    }

    #[test]
    fn error_inside_a_200_body_is_classified_by_its_code() {
        let f = failure_from_error_body(&json!({ "error": { "code": 429, "message": "limited" } }), &HeaderMap::new());
        assert_eq!((f.kind, f.status), (FailKind::RateLimit, Some(429)));
        let f = failure_from_error_body(&json!({ "error": { "message": "odd" } }), &HeaderMap::new());
        assert_eq!((f.kind, f.status), (FailKind::Server, Some(200)));
    }

    #[test]
    fn needs_detects_tools_and_images() {
        assert_eq!(needs(&json!({ "tools": [{}], "messages": [] })), (true, false));
        assert_eq!(needs(&json!({ "tools": [], "messages": [] })), (false, false));
        let img = json!({ "messages": [{ "role": "user", "content": [{ "type": "image_url", "image_url": { "url": "x" } }] }] });
        assert_eq!(needs(&img), (false, true));
        assert_eq!(needs(&json!({ "messages": [{ "role": "user", "content": "plain" }] })), (false, false));
    }

    #[test]
    fn first_event_waits_for_a_whole_data_line() {
        assert_eq!(first_event(b"data: {\"choices\":[]}"), None); // no newline yet
        assert_eq!(first_event(b": keep-alive\n\n"), None);
        assert_eq!(first_event(b"data: {\"choices\":[{}]}\n\n"), Some(Verdict::Ok));
        assert_eq!(first_event(b"data: [DONE]\n\n"), Some(Verdict::Empty));
        assert_eq!(first_event(b"data: not json\n"), Some(Verdict::Ok));
        match first_event(b"data: {\"error\":{\"code\":429}}\n\n") {
            Some(Verdict::Error(v)) => assert_eq!(v["error"]["code"], 429),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn session_cache_has_a_hard_limit() {
        use crate::{catalog::Catalog, config::Settings, http};
        let settings = Arc::new(Settings::from_yaml("scores: null\ntiers: []\nproviders: []").unwrap());
        let client = http::build_client(1.0);
        let catalog = Arc::new(Catalog::new(settings.clone(), client.clone(), None));
        let router = Router::new(settings, catalog, Arc::new(Health::new()), client);
        let model = Model {
            name: "m".into(),
            keys: Default::default(),
            score: None,
            tier: None,
            endpoints: vec![],
        };
        for n in 0..5100 {
            router.remember(Some(&n.to_string()), &model);
        }
        assert_eq!(router.sessions.lock().unwrap().len(), 5000);
        assert_eq!(router.sticky(Some("5099")).as_deref(), Some("m"));
        router.remember(Some("5099"), &model);
        assert_eq!(router.sessions.lock().unwrap().len(), 5000);
    }
}
