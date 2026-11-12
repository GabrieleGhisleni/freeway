//! "System One" decision API: typed questions about a state, answered with probabilities.
//!
//! The request and response shapes follow TypeSafe's `POST /v1/systemone` (also served by
//! OpenRouter as the Decisions API): `state` plus named `questions` of type `noul`, `choice`
//! or `score`; the answer for each is keyed by the same name.
//!
//! Here the answers come from the router's free chat models, asked for JSON. Their
//! probabilities are the model's own estimates, not calibrated like a dedicated decision
//! model's, and `confidence` is computed by this module from the spread of the probabilities.

use serde_json::{Map, Value, json};

pub const MAX_OPTIONS: usize = 255;
pub const MAX_LEVELS: usize = 10;

pub enum Kind {
    Noul { yes: Option<String>, no: Option<String> },
    Choice { options: Vec<(String, String)> },
    Score { levels: Vec<String> },
}

pub struct Question {
    pub id: String,
    pub instructions: String,
    pub kind: Kind,
}

pub struct Request {
    pub model: String,
    pub state: Value,
    pub questions: Vec<Question>,
}

/// Instructions, criteria and descriptions may be a string, an object or an array.
fn render(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_question(id: &str, q: &Value) -> Result<Question, String> {
    let at = |msg: &str| format!("questions.{id}: {msg}");
    let obj = q.as_object().ok_or_else(|| at("must be an object"))?;
    let instructions = match obj.get("instructions") {
        Some(v) if !v.is_null() && v != "" => render(v),
        _ => return Err(at("`instructions` is required")),
    };
    let criteria = obj.get("criteria").filter(|c| !c.is_null());
    let kind = match obj.get("type").and_then(Value::as_str) {
        Some("noul") => {
            let side = |k: &str| criteria.and_then(|c| c.get(k)).filter(|v| !v.is_null()).map(render);
            if criteria.is_some_and(|c| !c.is_object()) {
                return Err(at("noul `criteria` must be an object with `true` and `false`"));
            }
            Kind::Noul { yes: side("true"), no: side("false") }
        }
        Some("choice") => {
            let map = criteria.and_then(Value::as_object).ok_or_else(|| at("choice needs `criteria`: a map of option -> description"))?;
            if map.len() < 2 || map.len() > MAX_OPTIONS {
                return Err(at(&format!("choice needs between 2 and {MAX_OPTIONS} options")));
            }
            Kind::Choice { options: map.iter().map(|(k, v)| (k.clone(), render(v))).collect() }
        }
        Some("score") => {
            let list = criteria.and_then(Value::as_array).ok_or_else(|| at("score needs `criteria`: an ordered array of level descriptions"))?;
            if list.len() < 2 || list.len() > MAX_LEVELS {
                return Err(at(&format!("score needs between 2 and {MAX_LEVELS} levels")));
            }
            Kind::Score { levels: list.iter().map(render).collect() }
        }
        Some("box") => return Err(at("`box` questions are not supported by this server")),
        _ => return Err(at("`type` must be noul, choice or score")),
    };
    Ok(Question { id: id.to_string(), instructions, kind })
}

pub fn parse_request(body: &Value) -> Result<Request, String> {
    let obj = body.as_object().ok_or("body must be a JSON object")?;
    let model = obj.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()).ok_or("`model` is required")?.to_string();
    let state = match obj.get("state") {
        Some(s @ (Value::String(_) | Value::Object(_) | Value::Array(_))) => s.clone(),
        _ => return Err("`state` is required: a string, an object or an array".into()),
    };
    if obj.get("assets").is_some_and(|a| !a.is_null()) {
        return Err("`assets` are not supported: put images in `state` as {\"type\": \"image_url\", \"image_url\": {\"url\": ...}}".into());
    }
    let questions = obj.get("questions").and_then(Value::as_object).filter(|q| !q.is_empty()).ok_or("`questions` must be a non-empty object")?;
    let questions = questions.iter().map(|(id, q)| parse_question(id, q)).collect::<Result<Vec<_>, _>>()?;
    Ok(Request { model, state, questions })
}

fn is_image(v: &Value) -> bool {
    matches!(v["type"].as_str(), Some("image_url" | "input_image"))
}

fn describe(q: &Question) -> String {
    let head = format!("Question \"{}\": {}", q.id, q.instructions);
    match &q.kind {
        Kind::Noul { yes, no } => {
            let mut s = format!("{head}\n  (yes/no) Answer with the probability that the answer is YES.");
            if let Some(y) = yes {
                s += &format!("\n  YES means: {y}");
            }
            if let Some(n) = no {
                s += &format!("\n  NO means: {n}");
            }
            s + "\n  Reply: {\"yes\": <number 0..1>}"
        }
        Kind::Choice { options } => {
            let list: Vec<String> = options.iter().map(|(k, d)| format!("  - \"{k}\": {d}")).collect();
            let keys: Vec<String> = options.iter().map(|(k, _)| format!("\"{k}\": <p>")).collect();
            format!("{head}\n  Options:\n{}\n  Reply: {{\"probabilities\": {{{}}}}} (sum 1)", list.join("\n"), keys.join(", "))
        }
        Kind::Score { levels } => {
            let list: Vec<String> = levels.iter().enumerate().map(|(i, d)| format!("  - {i}: {d}")).collect();
            let keys: Vec<String> = (0..levels.len()).map(|i| format!("\"{i}\": <p>")).collect();
            format!("{head}\n  Levels, ordered from low to high:\n{}\n  Reply: {{\"probabilities\": {{{}}}}} (sum 1)", list.join("\n"), keys.join(", "))
        }
    }
}

const SYSTEM: &str = "You are a decision engine, not a chat assistant. You read a STATE and answer typed questions about it. \
Reply with exactly one JSON object and nothing else: no prose, no explanation, no code fences. \
Its keys are the question ids; each value has the shape given in that question's `Reply`. \
Probabilities must reflect your real uncertainty: do not round everything to 0 or 1.";

/// The `messages` for the chat model. State images are passed through as image parts.
pub fn build_messages(req: &Request) -> Value {
    let questions = req.questions.iter().map(describe).collect::<Vec<_>>().join("\n\n");
    let tail = format!("\n\nQUESTIONS:\n{questions}\n\nReturn one JSON object keyed by question id.");
    let items: Vec<Value> = match &req.state {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let user: Value = if items.iter().any(is_image) {
        let mut parts = vec![json!({ "type": "text", "text": "STATE:" })];
        for it in &items {
            parts.push(if is_image(it) { it.clone() } else { json!({ "type": "text", "text": render(it) }) });
        }
        parts.push(json!({ "type": "text", "text": tail.trim_start() }));
        Value::Array(parts)
    } else {
        let text = items.iter().map(render).collect::<Vec<_>>().join("\n");
        Value::String(format!("STATE:\n{text}{tail}"))
    };
    json!([{ "role": "system", "content": SYSTEM }, { "role": "user", "content": user }])
}

/// The text of a chat completion, whether `content` is a string or a list of parts.
pub fn completion_text(data: &Value) -> Option<String> {
    match &data["choices"][0]["message"]["content"] {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(parts) => {
            let text: String = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// Models sometimes wrap the object in prose or a code fence: take the outermost braces.
fn extract_json(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return Some(v);
    }
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    (start < end).then(|| serde_json::from_str(&text[start..=end]).ok()).flatten()
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches('%').parse().ok().map(|n: f64| if s.contains('%') { n / 100.0 } else { n }),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// 1 when all the probability sits on one outcome, 0 when it is spread evenly.
fn confidence(p: &[f64]) -> f64 {
    if p.len() < 2 {
        return 1.0;
    }
    let entropy: f64 = p.iter().filter(|x| **x > 0.0).map(|x| -x * x.ln()).sum();
    (1.0 - entropy / (p.len() as f64).ln()).clamp(0.0, 1.0)
}

/// Scale to sum 1. None when nothing positive was given.
fn normalized(p: Vec<f64>) -> Option<Vec<f64>> {
    let p: Vec<f64> = p.into_iter().map(|x| x.max(0.0)).collect();
    let sum: f64 = p.iter().sum();
    (sum > 0.0).then(|| p.into_iter().map(|x| x / sum).collect())
}

fn read_noul(raw: &Value) -> Option<Value> {
    let p = number(raw).or_else(|| ["yes", "p_yes", "noul", "probability", "true"].iter().find_map(|k| raw.get(*k).and_then(number)))?;
    // A 0..100 scale is a common slip; anything else out of range is clamped.
    let p = if p > 1.0 && p <= 100.0 { p / 100.0 } else { p };
    Some(json!({ "type": "noul", "noul": round(p.clamp(0.0, 1.0), 4) }))
}

fn read_choice(options: &[(String, String)], raw: &Value) -> Option<Value> {
    let table = raw.get("probabilities").filter(|p| p.is_object()).unwrap_or(raw);
    let mut p: Vec<f64> = options.iter().map(|(k, _)| table.get(k).and_then(number).unwrap_or(0.0)).collect();
    if p.iter().all(|x| *x <= 0.0) {
        // Only a name was given ("choice": "billing"): all the probability goes to it.
        let picked = raw.get("choice").or(Some(raw)).and_then(Value::as_str)?;
        p = options.iter().map(|(k, _)| if k == picked { 1.0 } else { 0.0 }).collect();
    }
    let p = normalized(p)?;
    let best = p.iter().enumerate().fold(0, |b, (i, x)| if *x > p[b] { i } else { b });
    let probabilities: Map<String, Value> = options.iter().zip(&p).map(|((k, _), x)| (k.clone(), json!(round(*x, 4)))).collect();
    Some(json!({ "type": "choice", "choice": options[best].0, "confidence": round(confidence(&p), 2), "probabilities": probabilities }))
}

fn read_score(levels: &[String], raw: &Value) -> Option<Value> {
    let n = levels.len();
    let table = raw.get("probabilities").unwrap_or(raw);
    let from_table: Vec<f64> = match table {
        Value::Array(a) if a.len() == n => a.iter().map(|v| number(v).unwrap_or(0.0)).collect(),
        Value::Object(_) => (0..n).map(|i| table.get(i.to_string()).and_then(number).unwrap_or(0.0)).collect(),
        _ => vec![0.0; n],
    };
    let p = match normalized(from_table) {
        Some(p) => p,
        None => {
            // Only a position was given: spread it over the two neighbouring levels.
            let s = raw.get("score").and_then(number).or_else(|| number(raw))?.clamp(0.0, (n - 1) as f64);
            let (lo, frac) = (s.floor() as usize, s.fract());
            let mut p = vec![0.0; n];
            p[lo] = 1.0 - frac;
            if frac > 0.0 {
                p[lo + 1] = frac;
            }
            p
        }
    };
    let score: f64 = p.iter().enumerate().map(|(i, x)| i as f64 * x).sum();
    let legend: Map<String, Value> = levels.iter().enumerate().map(|(i, d)| (i.to_string(), json!(d))).collect();
    let probabilities: Map<String, Value> = p.iter().enumerate().map(|(i, x)| (i.to_string(), json!(round(*x, 4)))).collect();
    Some(json!({ "type": "score", "score": round(score, 2), "confidence": round(confidence(&p), 2), "legend": legend, "probabilities": probabilities }))
}

/// Turn a chat answer into the `answers` object, or say what was wrong with it.
pub fn read_answers(questions: &[Question], text: &str) -> Result<Map<String, Value>, String> {
    let parsed = extract_json(text).filter(Value::is_object).ok_or("answer is not a JSON object")?;
    let mut answers = Map::new();
    for q in questions {
        let raw = parsed.get(&q.id).ok_or_else(|| format!("no answer for question {:?}", q.id))?;
        let answer = match &q.kind {
            Kind::Noul { .. } => read_noul(raw),
            Kind::Choice { options } => read_choice(options, raw),
            Kind::Score { levels } => read_score(levels, raw),
        };
        answers.insert(q.id.clone(), answer.ok_or_else(|| format!("unusable answer for question {:?}", q.id))?);
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(questions: Value) -> Request {
        parse_request(&json!({ "model": "fast", "state": "hello", "questions": questions })).unwrap()
    }

    #[test]
    fn request_validation() {
        let bad = |b: Value| parse_request(&b).err().unwrap_or_default();
        assert!(bad(json!({ "state": "x", "questions": {} })).contains("model"));
        assert!(bad(json!({ "model": "m", "questions": { "a": {} } })).contains("state"));
        assert!(bad(json!({ "model": "m", "state": "x", "questions": {} })).contains("questions"));
        let q = |v: Value| bad(json!({ "model": "m", "state": "x", "questions": { "a": v } }));
        assert!(q(json!({ "type": "box", "instructions": "where" })).contains("not supported"));
        assert!(q(json!({ "type": "noul" })).contains("instructions"));
        assert!(q(json!({ "type": "choice", "instructions": "i", "criteria": { "only": "one" } })).contains("2 and"));
        assert!(q(json!({ "type": "score", "instructions": "i", "criteria": ["one"] })).contains("levels"));
        assert!(bad(json!({ "model": "m", "state": "x", "assets": {}, "questions": { "a": { "type": "noul", "instructions": "i" } } })).contains("assets"));
        assert!(parse_request(&json!({ "model": "m", "state": ["a", "b"], "questions": { "a": { "type": "noul", "instructions": "i" } } })).is_ok());
    }

    #[test]
    fn noul_reads_numbers_objects_and_percentages() {
        let qs = req(json!({ "a": { "type": "noul", "instructions": "i" } }));
        for (raw, want) in [(r#"{"a": 0.9}"#, 0.9), (r#"{"a": {"yes": 0.25}}"#, 0.25), (r#"{"a": "80%"}"#, 0.8), (r#"{"a": 95}"#, 0.95), (r#"{"a": true}"#, 1.0)] {
            let got = read_answers(&qs.questions, raw).unwrap();
            assert_eq!(got["a"]["noul"], json!(want), "{raw}");
            assert_eq!(got["a"]["type"], "noul");
        }
    }

    #[test]
    fn choice_is_normalised_and_ignores_unknown_options() {
        let qs = req(json!({ "d": { "type": "choice", "instructions": "i", "criteria": { "billing": "b", "shipping": "s", "returns": "r" } } }));
        let got = read_answers(&qs.questions, r#"{"d": {"probabilities": {"billing": 3, "shipping": 1, "invented": 9}}}"#).unwrap();
        assert_eq!(got["d"]["choice"], "billing");
        assert_eq!(got["d"]["probabilities"]["billing"], json!(0.75));
        assert_eq!(got["d"]["probabilities"]["returns"], json!(0.0));
        let c = got["d"]["confidence"].as_f64().unwrap();
        assert!(c > 0.0 && c < 1.0);
        // A bare name is a certain answer.
        let named = read_answers(&qs.questions, r#"{"d": {"choice": "returns"}}"#).unwrap();
        assert_eq!((named["d"]["choice"].as_str(), named["d"]["confidence"].as_f64()), (Some("returns"), Some(1.0)));
        assert!(read_answers(&qs.questions, r#"{"d": {"choice": "nonsense"}}"#).is_err());
    }

    #[test]
    fn score_is_the_probability_weighted_level() {
        let qs = req(json!({ "s": { "type": "score", "instructions": "i", "criteria": ["low", "mid", "high"] } }));
        let got = read_answers(&qs.questions, r#"{"s": {"probabilities": {"0": 0, "1": 0.57, "2": 0.43}}}"#).unwrap();
        assert_eq!(got["s"]["score"], json!(1.43));
        assert_eq!(got["s"]["legend"]["2"], "high");
        let from_position = read_answers(&qs.questions, r#"{"s": {"score": 1.5}}"#).unwrap();
        assert_eq!(from_position["s"]["score"], json!(1.5));
        let from_array = read_answers(&qs.questions, r#"{"s": [0.5, 0.5, 0]}"#).unwrap();
        assert_eq!(from_array["s"]["score"], json!(0.5));
    }

    #[test]
    fn answers_may_come_wrapped_in_prose_or_fences() {
        let qs = req(json!({ "a": { "type": "noul", "instructions": "i" } }));
        assert!(read_answers(&qs.questions, "```json\n{\"a\": 0.5}\n```").is_ok());
        assert!(read_answers(&qs.questions, "Sure! {\"a\": 0.5} Hope that helps").is_ok());
        assert!(read_answers(&qs.questions, "no json here").is_err());
        assert!(read_answers(&qs.questions, r#"{"other": 0.5}"#).unwrap_err().contains("no answer"));
    }

    #[test]
    fn confidence_runs_from_even_spread_to_single_peak() {
        assert!((confidence(&[0.5, 0.5])).abs() < 1e-9);
        assert!((confidence(&[1.0, 0.0, 0.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn images_in_the_state_become_image_parts() {
        let r = parse_request(&json!({
            "model": "m",
            "state": ["a bike", { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }],
            "questions": { "a": { "type": "noul", "instructions": "Is it red?" } }
        }))
        .unwrap();
        let user = &build_messages(&r)[1]["content"];
        assert!(user.as_array().unwrap().iter().any(|p| p["type"] == "image_url"));
        let text = build_messages(&req(json!({ "a": { "type": "noul", "instructions": "Is it red?" } })));
        assert!(text[1]["content"].as_str().unwrap().contains("STATE:\nhello"));
    }
}
