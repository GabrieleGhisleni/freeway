use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Shell-style match supporting `*` (any run) and `?` (one char). Case sensitive.
pub fn fnmatch(name: &str, pattern: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < n.len() {
        if j < p.len() && (p[j] == '?' || p[j] == n[i]) && p[j] != '*' {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '*' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(s) = star {
            j = s + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '*' {
        j += 1;
    }
    j == p.len()
}

/// JSON truthiness, the way the request fields are read: null, false, 0, "", [] and {} are all empty.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Keep at most `n` characters.
pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_wildcards() {
        assert!(fnmatch("gemini-3.1-pro-preview", "*-pro*"));
        assert!(fnmatch("gemini-flash-latest", "*-latest"));
        assert!(fnmatch("gemini-4-carbon", "gemini-4-*"));
        assert!(!fnmatch("gemini-3.5-flash", "*-pro*"));
        assert!(fnmatch("abc", "a?c"));
        assert!(!fnmatch("abcd", "a?c"));
        assert!(fnmatch("anything", "*"));
        assert!(fnmatch("text-embed-v2", "*embed*"));
        assert!(!fnmatch("", "a*"));
        assert!(fnmatch("", "*"));
    }

    #[test]
    fn truthiness() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&serde_json::json!([]))));
        assert!(truthy(Some(&serde_json::json!([1]))));
        assert!(truthy(Some(&serde_json::json!(true))));
        assert!(!truthy(Some(&serde_json::json!(0))));
    }
}
