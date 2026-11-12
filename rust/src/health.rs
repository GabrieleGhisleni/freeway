//! Cooldowns: which provider endpoints are currently not worth trying.

use crate::util::{now, truncate};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    RateLimit,
    Auth,
    /// 402 / 403 / 404
    Unavailable,
    /// 5xx, timeouts, network, malformed answers
    Server,
    /// The request's fault, not the endpoint's.
    BadRequest,
}

impl FailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailKind::RateLimit => "rate_limit",
            FailKind::Auth => "auth",
            FailKind::Unavailable => "unavailable",
            FailKind::Server => "server",
            FailKind::BadRequest => "bad_request",
        }
    }

    /// (base seconds, cap seconds, doubles on repeat, pauses the whole provider)
    fn policy(self) -> (f64, f64, bool, bool) {
        match self {
            FailKind::RateLimit => (60.0, 900.0, true, false),
            FailKind::Auth => (600.0, 600.0, false, true),
            FailKind::Unavailable => (1800.0, 1800.0, false, false),
            FailKind::Server => (20.0, 600.0, true, false),
            FailKind::BadRequest => (0.0, 0.0, false, false),
        }
    }
}

const MAX_RETRY_AFTER: f64 = 6.0 * 3600.0;

#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub ok: u64,
    pub failed: u64,
    pub strikes: u32, // consecutive failures
    pub last_error: Option<String>,
    pub last_latency: Option<f64>,
}

#[derive(Default)]
struct State {
    /// (provider, Some(endpoint)) or (provider, None) for the whole provider.
    until: HashMap<(String, Option<String>), f64>,
    stats: HashMap<(String, String), Stat>,
}

#[derive(Default)]
pub struct Health {
    state: Mutex<State>,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seconds until this endpoint may be tried again (0 = usable now).
    pub fn cooldown(&self, provider: &str, raw_id: &str) -> f64 {
        let st = self.state.lock().unwrap();
        let one = st.until.get(&(provider.to_string(), Some(raw_id.to_string()))).copied().unwrap_or(0.0);
        let all = st.until.get(&(provider.to_string(), None)).copied().unwrap_or(0.0);
        (one.max(all) - now()).max(0.0)
    }

    pub fn ok(&self, provider: &str, raw_id: &str, latency: f64) {
        let mut st = self.state.lock().unwrap();
        let s = st.stats.entry((provider.to_string(), raw_id.to_string())).or_default();
        s.ok += 1;
        s.strikes = 0;
        s.last_latency = Some((latency * 1000.0).round() / 1000.0);
        st.until.remove(&(provider.to_string(), Some(raw_id.to_string())));
    }

    /// Pause every endpoint of a provider (its limit is on the account, not on one model).
    pub fn cool_provider(&self, provider: &str, seconds: f64) {
        if seconds > 0.0 {
            let mut st = self.state.lock().unwrap();
            let slot = st.until.entry((provider.to_string(), None)).or_insert(0.0);
            *slot = slot.max(now() + seconds);
        }
    }

    /// Records a failure and returns the pause (seconds) it earned.
    pub fn fail(&self, provider: &str, raw_id: &str, kind: FailKind, error: &str, retry_after: Option<f64>) -> f64 {
        let mut st = self.state.lock().unwrap();
        let strikes = {
            let s = st.stats.entry((provider.to_string(), raw_id.to_string())).or_default();
            s.failed += 1;
            s.strikes += 1;
            s.last_error = Some(truncate(error, 300));
            s.strikes
        };
        let (base, cap, doubles, whole_provider) = kind.policy();
        let mut seconds = if doubles { cap.min(base * 2f64.powi(strikes.saturating_sub(1).min(30) as i32)) } else { base };
        if let Some(ra) = retry_after.filter(|r| *r > 0.0) {
            // The provider knows its own limits better than our backoff does.
            seconds = ra.clamp(1.0, MAX_RETRY_AFTER);
        }
        if seconds > 0.0 {
            let target = if whole_provider { None } else { Some(raw_id.to_string()) };
            st.until.insert((provider.to_string(), target), now() + seconds);
        }
        seconds
    }

    pub fn stat(&self, provider: &str, raw_id: &str) -> Option<Stat> {
        self.state.lock().unwrap().stats.get(&(provider.to_string(), raw_id.to_string())).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_double_and_reset_on_success() {
        let h = Health::new();
        assert_eq!(h.fail("p", "m", FailKind::RateLimit, "x", None), 60.0);
        assert_eq!(h.fail("p", "m", FailKind::RateLimit, "x", None), 120.0);
        assert!(h.cooldown("p", "m") > 100.0);
        assert_eq!(h.cooldown("p", "other"), 0.0);
        h.ok("p", "m", 0.5);
        assert_eq!(h.cooldown("p", "m"), 0.0);
        assert_eq!(h.fail("p", "m", FailKind::RateLimit, "x", None), 60.0);
    }

    #[test]
    fn retry_after_wins_but_is_capped() {
        let h = Health::new();
        assert_eq!(h.fail("p", "m", FailKind::RateLimit, "x", Some(30.0)), 30.0);
        assert_eq!(h.fail("p", "n", FailKind::RateLimit, "x", Some(10.0 * 3600.0)), MAX_RETRY_AFTER);
    }

    #[test]
    fn auth_pauses_the_whole_provider_and_bad_requests_nothing() {
        let h = Health::new();
        h.fail("p", "m", FailKind::Auth, "bad key", None);
        assert!(h.cooldown("p", "anything") > 500.0);
        assert_eq!(h.cooldown("q", "m"), 0.0);
        assert_eq!(h.fail("q", "m", FailKind::BadRequest, "nope", None), 0.0);
        assert_eq!(h.cooldown("q", "m"), 0.0);
    }

    #[test]
    fn cool_provider_never_shortens_a_longer_pause() {
        let h = Health::new();
        h.cool_provider("p", 500.0);
        h.cool_provider("p", 10.0);
        assert!(h.cooldown("p", "m") > 400.0);
    }
}
