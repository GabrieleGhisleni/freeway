//! The built-in `fake` model: realistic-looking answers made up locally, no provider involved.
//!
//! Useful to try a client (streaming, usage, headers, timeouts) without spending free quota or
//! needing keys. It is never part of a tier, so a tier or fallback can never pick it by accident,
//! and every answer says what it is: `system_fingerprint: "fake"` and `x-router-provider: fake`.

use crate::router::ByteStream;
use crate::util::{now, truthy};
use bytes::Bytes;
use serde_json::{Value, json};
use std::time::Duration;

pub const MODEL: &str = "fake";

/// Delay between streamed words, so a client sees a stream and not one burst.
const WORD_DELAY: Duration = Duration::from_millis(25);

const SENTENCES: &[&str] = &[
    "Questa è una risposta finta generata dal modello fake del router.",
    "Nessun provider è stato contattato e nessuna quota gratuita è stata consumata.",
    "Il testo dipende solo dall'ultimo messaggio che hai inviato: lo stesso messaggio dà sempre la stessa risposta.",
    "Puoi usarlo per provare lo streaming, il conteggio dei token e la gestione degli header del router.",
    "Se vedi questa frase in produzione, qualcuno ha scelto il modello fake al posto di un modello vero.",
    "Le risposte sono pensate per sembrare plausibili nella forma, ma il contenuto non ha alcun significato.",
    "Con max_tokens puoi accorciare la risposta e vedere come il client gestisce finish_reason length.",
    "Il router non sceglie mai questo modello da solo: va richiesto per nome.",
];

/// Small stable hash (FNV-1a) so the same prompt always gives the same text.
fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn last_user_text(body: &Value) -> String {
    let content = body["messages"].as_array().into_iter().flatten().rev().find(|m| m["role"] == "user").map(|m| &m["content"]);
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

fn token_limit(body: &Value) -> Option<usize> {
    ["max_completion_tokens", "max_tokens"].iter().find_map(|k| body[*k].as_u64()).map(|n| n as usize)
}

/// Rough token count: about four characters each.
fn tokens(text: &str) -> u64 {
    text.chars().count().div_ceil(4) as u64
}

pub struct Answer {
    pub words: Vec<String>,
    /// True when `max_tokens` cut the answer short.
    pub truncated: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

pub fn answer(body: &Value) -> Answer {
    let prompt = last_user_text(body);
    let h = hash(&prompt);
    let count = 2 + (h % 3) as usize;
    let sentences: Vec<&str> = (0..count).map(|i| SENTENCES[((h >> (8 * i)) as usize + i) % SENTENCES.len()]).collect();
    let mut words: Vec<String> = sentences.join(" ").split_whitespace().map(str::to_string).collect();
    // One word is about 1.3 tokens: stay under the limit the caller asked for.
    let truncated = token_limit(body).is_some_and(|limit| {
        let keep = (limit as f64 / 1.3) as usize;
        let cut = words.len() > keep;
        words.truncate(keep.max(1));
        cut
    });
    let text = words.join(" ");
    Answer {
        completion_tokens: tokens(&text).max(1),
        prompt_tokens: tokens(&body["messages"].to_string()),
        words,
        truncated,
    }
}

fn finish(a: &Answer) -> &'static str {
    if a.truncated { "length" } else { "stop" }
}

fn id() -> String {
    format!("chatcmpl-fake-{}", (now() * 1000.0) as u64)
}

/// A complete (non-streaming) chat completion.
pub fn completion(body: &Value) -> Value {
    let a = answer(body);
    json!({
        "id": id(),
        "object": "chat.completion",
        "created": now() as i64,
        "model": MODEL,
        "system_fingerprint": "fake",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": a.words.join(" ") },
            "logprobs": null,
            "finish_reason": finish(&a),
        }],
        "usage": {
            "prompt_tokens": a.prompt_tokens,
            "completion_tokens": a.completion_tokens,
            "total_tokens": a.prompt_tokens + a.completion_tokens,
        },
    })
}

/// The same answer as server-sent events, word by word, ending with `[DONE]`.
pub fn stream(body: &Value) -> ByteStream {
    let a = answer(body);
    let with_usage = truthy(body["stream_options"].get("include_usage"));
    let (id, created) = (id(), now() as i64);
    Box::pin(async_stream::stream! {
        let chunk = |delta: Value, finish: Value| {
            let c = json!({
                "id": id, "object": "chat.completion.chunk", "created": created, "model": MODEL, "system_fingerprint": "fake",
                "choices": [{ "index": 0, "delta": delta, "logprobs": null, "finish_reason": finish }],
            });
            Bytes::from(format!("data: {c}\n\n"))
        };
        yield Ok::<Bytes, std::io::Error>(chunk(json!({ "role": "assistant", "content": "" }), Value::Null));
        for (i, word) in a.words.iter().enumerate() {
            tokio::time::sleep(WORD_DELAY).await;
            let piece = if i == 0 { word.clone() } else { format!(" {word}") };
            yield Ok(chunk(json!({ "content": piece }), Value::Null));
        }
        yield Ok(chunk(json!({}), json!(finish(&a))));
        if with_usage {
            let u = json!({
                "id": id, "object": "chat.completion.chunk", "created": created, "model": MODEL, "system_fingerprint": "fake",
                "choices": [],
                "usage": {
                    "prompt_tokens": a.prompt_tokens,
                    "completion_tokens": a.completion_tokens,
                    "total_tokens": a.prompt_tokens + a.completion_tokens,
                },
            });
            yield Ok(Bytes::from(format!("data: {u}\n\n")));
        }
        yield Ok(Bytes::from("data: [DONE]\n\n"));
    })
}

pub fn headers() -> Vec<(&'static str, String)> {
    vec![
        ("x-router-model", MODEL.into()),
        ("x-router-tier", "fake".into()),
        ("x-router-provider", "fake".into()),
        ("x-router-upstream-model", MODEL.into()),
        ("x-router-attempts", "1".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str) -> Value {
        json!({ "model": "fake", "messages": [{ "role": "user", "content": text }] })
    }

    #[test]
    fn same_prompt_same_answer_different_prompt_usually_different() {
        let a = answer(&req("ciao"));
        assert_eq!(a.words, answer(&req("ciao")).words);
        let texts: std::collections::HashSet<String> = ["a", "b", "c", "d", "e", "f"].iter().map(|p| answer(&req(p)).words.join(" ")).collect();
        assert!(texts.len() > 1);
    }

    #[test]
    fn max_tokens_cuts_the_answer_and_says_so() {
        let full = answer(&req("ciao"));
        assert!(!full.truncated);
        let mut body = req("ciao");
        body["max_tokens"] = json!(5);
        let cut = answer(&body);
        assert!(cut.truncated && cut.words.len() < full.words.len() && !cut.words.is_empty());
        assert_eq!(completion(&body)["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn completion_has_the_openai_shape() {
        let c = completion(&req("ciao"));
        assert_eq!(c["object"], "chat.completion");
        assert_eq!(c["system_fingerprint"], "fake");
        let usage = &c["usage"];
        assert_eq!(usage["total_tokens"].as_u64(), Some(usage["prompt_tokens"].as_u64().unwrap() + usage["completion_tokens"].as_u64().unwrap()));
    }
}
