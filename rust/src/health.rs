//! Cooldowns: which provider endpoints are currently not worth trying.

use crate::util::{now, truncate};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
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
/// Seconds between disk writes caused by successes alone (failures and pauses are written at once).
const SAVE_EVERY: f64 = 30.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stat {
    pub ok: u64,
    pub failed: u64,
    pub strikes: u32, // consecutive failures
    pub last_error: Option<String>,
    pub last_latency: Option<f64>,
    /// Failures per kind (rate_limit, server, ...).
    #[serde(default)]
    pub by_kind: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_ok_at: Option<f64>,
    #[serde(default)]
    pub last_failed_at: Option<f64>,
}

/// Why an endpoint (or a whole provider) is paused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pause {
    pub until: f64,
    pub kind: String,
    pub reason: String,
    pub since: f64,
}

/// One active pause, as shown by `/admin/info`.
#[derive(Debug, Clone, Serialize)]
pub struct PauseInfo {
    pub provider: String,
    /// None = the whole provider.
    pub endpoint: Option<String>,
    pub kind: String,
    pub reason: String,
    pub since: f64,
    pub until: f64,
    pub remaining_seconds: i64,
}

#[derive(Serialize, Deserialize)]
struct PauseRow {
    provider: String,
    endpoint: Option<String>,
    #[serde(flatten)]
    pause: Pause,
}

#[derive(Serialize, Deserialize)]
struct StatRow {
    provider: String,
    endpoint: String,
    #[serde(flatten)]
    stat: Stat,
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    #[serde(default)]
    saved_at: f64,
    #[serde(default)]
    pauses: Vec<PauseRow>,
    #[serde(default)]
    stats: Vec<StatRow>,
}

#[derive(Default)]
struct State {
    /// (provider, Some(endpoint)) or (provider, None) for the whole provider.
    until: HashMap<(String, Option<String>), Pause>,
    stats: HashMap<(String, String), Stat>,
    dirty: bool,
    saved_at: f64,
}

/// Cooldowns and counters, optionally persisted to a JSON file so a restart does not forget them.
#[derive(Default)]
pub struct Health {
    state: Mutex<State>,
    path: Option<PathBuf>,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the file when it exists (a missing or corrupt file means a clean start).
    pub fn with_file(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut state = State::default();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Saved>(&text) {
                Ok(saved) => {
                    let t = now();
                    for r in saved.pauses.into_iter().filter(|r| r.pause.until > t) {
                        state.until.insert((r.provider, r.endpoint), r.pause);
                    }
                    for r in saved.stats {
                        state.stats.insert((r.provider, r.endpoint), r.stat);
                    }
                }
                Err(e) => tracing::warn!("could not read {} ({e}): starting with empty health", path.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("could not read {} ({e}): starting with empty health", path.display()),
        }
        Self { state: Mutex::new(state), path: Some(path) }
    }

    /// Writes the file now (atomic: a crash never leaves half a file).
    pub fn save(&self) {
        let mut st = self.state.lock().unwrap();
        self.write(&mut st);
    }

    fn write(&self, st: &mut State) {
        let Some(path) = &self.path else { return };
        let t = now();
        let saved = Saved {
            saved_at: t,
            pauses: st
                .until
                .iter()
                .filter(|(_, p)| p.until > t)
                .map(|((provider, endpoint), p)| PauseRow { provider: provider.clone(), endpoint: endpoint.clone(), pause: p.clone() })
                .collect(),
            stats: st
                .stats
                .iter()
                .map(|((provider, endpoint), s)| StatRow { provider: provider.clone(), endpoint: endpoint.clone(), stat: s.clone() })
                .collect(),
        };
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&saved)?)?;
            std::fs::rename(tmp, path)
        })();
        match result {
            Ok(()) => {
                st.dirty = false;
                st.saved_at = t;
            }
            Err(e) => tracing::warn!("could not write {}: {e}", path.display()),
        }
    }

    fn touch(&self, st: &mut State, urgent: bool) {
        st.dirty = true;
        if self.path.is_some() && (urgent || now() - st.saved_at >= SAVE_EVERY) {
            self.write(st);
        }
    }

    /// Seconds until this endpoint may be tried again (0 = usable now).
    pub fn cooldown(&self, provider: &str, raw_id: &str) -> f64 {
        let st = self.state.lock().unwrap();
        let until = |e: Option<String>| st.until.get(&(provider.to_string(), e)).map_or(0.0, |p| p.until);
        (until(Some(raw_id.to_string())).max(until(None)) - now()).max(0.0)
    }

    /// Active pauses, soonest to expire first.
    pub fn pauses(&self) -> Vec<PauseInfo> {
        let t = now();
        let st = self.state.lock().unwrap();
        let mut rows: Vec<PauseInfo> = st
            .until
            .iter()
            .filter(|(_, p)| p.until > t)
            .map(|((provider, endpoint), p)| PauseInfo {
                provider: provider.clone(),
                endpoint: endpoint.clone(),
                kind: p.kind.clone(),
                reason: p.reason.clone(),
                since: p.since,
                until: p.until,
                remaining_seconds: (p.until - t).round() as i64,
            })
            .collect();
        rows.sort_by(|a, b| a.until.total_cmp(&b.until));
        rows
    }

    pub fn ok(&self, provider: &str, raw_id: &str, latency: f64) {
        let mut st = self.state.lock().unwrap();
        let s = st.stats.entry((provider.to_string(), raw_id.to_string())).or_default();
        s.ok += 1;
        s.strikes = 0;
        s.last_latency = Some((latency * 1000.0).round() / 1000.0);
        s.last_ok_at = Some(now());
        let had_pause = st.until.remove(&(provider.to_string(), Some(raw_id.to_string()))).is_some();
        self.touch(&mut st, had_pause);
    }

    /// Pause every endpoint of a provider (its limit is on the account, not on one model).
    pub fn cool_provider(&self, provider: &str, seconds: f64) {
        if seconds > 0.0 {
            let mut st = self.state.lock().unwrap();
            let t = now();
            let slot = st.until.entry((provider.to_string(), None)).or_insert_with(|| Pause {
                until: 0.0,
                kind: "rate_limit".into(),
                reason: "rate limit hit on several models of this provider".into(),
                since: t,
            });
            slot.until = slot.until.max(t + seconds);
            self.touch(&mut st, true);
        }
    }

    /// Records a failure and returns the pause (seconds) it earned.
    pub fn fail(&self, provider: &str, raw_id: &str, kind: FailKind, error: &str, retry_after: Option<f64>) -> f64 {
        let mut st = self.state.lock().unwrap();
        let t = now();
        let strikes = {
            let s = st.stats.entry((provider.to_string(), raw_id.to_string())).or_default();
            s.failed += 1;
            s.strikes += 1;
            s.last_error = Some(truncate(error, 300));
            s.last_failed_at = Some(t);
            *s.by_kind.entry(kind.as_str().to_string()).or_default() += 1;
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
            let pause = Pause { until: t + seconds, kind: kind.as_str().into(), reason: truncate(error, 300), since: t };
            st.until.insert((provider.to_string(), target), pause);
        }
        self.touch(&mut st, true);
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

    #[test]
    fn pauses_and_counters_survive_a_restart() {
        let path = std::env::temp_dir().join(format!("freeway-health-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let h = Health::with_file(&path);
        h.fail("p", "m1", FailKind::RateLimit, "slow down", Some(300.0));
        h.ok("p", "m2", 0.5);
        h.fail("p", "m2", FailKind::Server, "boom", None);

        let again = Health::with_file(&path);
        assert!((again.cooldown("p", "m1") - 300.0).abs() < 5.0);
        let s = again.stat("p", "m2").unwrap();
        assert_eq!((s.ok, s.failed, s.by_kind.get("server").copied()), (1, 1, Some(1)));
        let pause = again.pauses().into_iter().find(|p| p.endpoint.as_deref() == Some("m1")).unwrap();
        assert_eq!(pause.kind, "rate_limit");

        again.ok("p", "m1", 0.1); // success clears the pause on disk as well
        assert_eq!(Health::with_file(&path).cooldown("p", "m1"), 0.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let path = std::env::temp_dir().join(format!("freeway-health-bad-{}.json", std::process::id()));
        std::fs::write(&path, "{not json").unwrap();
        assert!(Health::with_file(&path).pauses().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
