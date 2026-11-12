//! Everything the plugin remembers: per-account quota, thread bindings, and the
//! counters behind the report. Mutated only through `pick` and `observe`.

use crate::config::Config;
use crate::policy::{self, AccountView, Decision, Reason};
use crate::quota::{self, Window};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const MAX_SAMPLES: usize = 400;
const SAMPLE_GAP_SECS: f64 = 900.0;
const BURN_MIN_GAP_SECS: f64 = 300.0;
const BURN_ALPHA: f64 = 0.3;
const MAX_SESSIONS: usize = 20_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    pub provider: String,
    pub windows: BTreeMap<String, Window>,
    pub burn_per_hour: f64,
    /// Baseline for the next burn measurement: (unix seconds, budget used, budget reset).
    pub burn_base: Option<(f64, f64, f64)>,
    pub free_resets: u32,
    pub banked_resets: u32,
    pub last_used: f64,
    pub last_observed: f64,
    /// (unix seconds, budget fraction used) at most every 15 minutes.
    pub samples: Vec<(f64, f64)>,
    pub requests: u64,
    pub failures: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_misses: u64,
    /// Budget fraction left unspent at each observed reset (most recent last).
    pub wasted_at_reset: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Binding {
    pub auth_id: String,
    pub last_seen: f64,
    /// Time of the previous request on the same account. 0 for a thread's first turn there.
    pub prev_seen: f64,
    pub turns: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub accounts: BTreeMap<String, Account>,
    pub sessions: BTreeMap<String, Binding>,
    pub rebinds: u64,
    pub gated: u64,
    pub picks: u64,
    pub deferred: u64,
    /// Provider -> unix seconds of the latest free-reset announcement applied.
    pub announcements: BTreeMap<String, f64>,
}

/// One candidate as offered by the host.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub provider: String,
    /// True for subscription (OAuth/file) credentials, false for API keys.
    pub subscription: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PickInput {
    pub model: String,
    pub session: String,
    pub parent_session: String,
    pub user_agent: String,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug)]
pub enum PickOutcome {
    /// Route to this credential.
    Choose(String),
    /// Leave the decision to the host's built-in scheduler.
    Defer,
    /// Refuse the request.
    Reject(String),
}

pub struct PickResult {
    pub outcome: PickOutcome,
    pub decision: Option<Decision>,
    pub bound_before: Option<String>,
    pub gated: usize,
}

/// One completed request, as reported by the host's usage pipeline.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub auth_id: String,
    pub provider: String,
    pub model: String,
    pub session: String,
    pub failed: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct Observed {
    pub windows: BTreeMap<String, Window>,
    /// Set when a warm thread sent a large prompt and read nothing from cache.
    pub cache_miss: bool,
}

fn client_allowed(cfg: &Config, user_agent: &str) -> bool {
    let ua = user_agent.to_ascii_lowercase();
    cfg.claude_clients.iter().any(|p| !p.is_empty() && ua.starts_with(&p.to_ascii_lowercase()))
}

impl State {
    fn live_binding(&self, session: &str, now: f64, cfg: &Config) -> Option<&Binding> {
        if session.is_empty() {
            return None;
        }
        self.sessions.get(session).filter(|b| now - b.last_seen <= cfg.affinity_ttl_secs as f64)
    }

    fn view(&self, id: &str) -> AccountView {
        match self.accounts.get(id) {
            Some(a) => AccountView {
                id: id.to_string(),
                windows: a.windows.clone(),
                burn_per_hour: a.burn_per_hour,
                free_resets: a.free_resets,
                banked_resets: a.banked_resets,
                last_used: a.last_used,
            },
            None => AccountView { id: id.to_string(), ..Default::default() },
        }
    }

    pub fn pick(&mut self, now: f64, input: &PickInput, cfg: &Config) -> PickResult {
        self.picks += 1;
        let total = input.candidates.len();
        let allowed = !cfg.gate_claude_subs || client_allowed(cfg, &input.user_agent);
        let eligible: Vec<&Candidate> = input
            .candidates
            .iter()
            .filter(|c| allowed || !(c.subscription && c.provider.eq_ignore_ascii_case("claude")))
            .collect();
        let gated = total - eligible.len();
        if gated > 0 {
            self.gated += 1;
        }
        if eligible.is_empty() && gated > 0 {
            return PickResult {
                outcome: PickOutcome::Reject("Claude subscription credentials are reserved for Claude Code and T3 Code clients".into()),
                decision: None,
                bound_before: None,
                gated,
            };
        }
        // Providers we do not schedule, and observe-only mode, go to the host. Gating
        // above still applied: if anything was removed we must answer ourselves.
        let ours = eligible.iter().all(|c| cfg.schedules(&c.provider));
        if (!cfg.route || !ours) && gated == 0 {
            self.deferred += 1;
            return PickResult { outcome: PickOutcome::Defer, decision: None, bound_before: None, gated };
        }

        let mut bound = self.live_binding(&input.session, now, cfg).map(|b| b.auth_id.clone());
        if bound.is_none() {
            // A subagent starts on its parent's account so it shares the parent's cache.
            bound = self.live_binding(&input.parent_session, now, cfg).map(|b| b.auth_id.clone());
        }
        let views: Vec<AccountView> = eligible.iter().map(|c| self.view(&c.id)).collect();
        let decision = policy::decide(now, &input.model, &views, bound.as_deref(), cfg);
        let choice = match (&decision.choice, decision.reason) {
            (Some(id), _) => id.clone(),
            // Everything is at its limit: pick the least bad ourselves when the host's
            // pool would include gated accounts, otherwise let the host decide.
            (None, Reason::AllBlocked) if gated > 0 => decision.scores[0].id.clone(),
            _ => {
                self.deferred += 1;
                return PickResult { outcome: PickOutcome::Defer, decision: Some(decision), bound_before: bound, gated };
            }
        };
        for c in &eligible {
            if c.id == choice {
                let a = self.accounts.entry(choice.clone()).or_default();
                a.provider = c.provider.clone();
                a.last_used = now;
            }
        }
        if !input.session.is_empty() {
            let moved = self.live_binding(&input.session, now, cfg).is_some_and(|b| b.auth_id != choice);
            if moved {
                self.rebinds += 1;
            }
            let b = self.sessions.entry(input.session.clone()).or_default();
            if b.auth_id == choice {
                b.prev_seen = b.last_seen;
            } else {
                // New thread, or moved to another account: its cache starts cold.
                b.prev_seen = 0.0;
                b.turns = 0;
            }
            b.auth_id = choice.clone();
            b.last_seen = now;
            b.turns += 1;
            self.trim_sessions(now, cfg);
        }
        PickResult { outcome: PickOutcome::Choose(choice), decision: Some(decision), bound_before: bound, gated }
    }

    fn trim_sessions(&mut self, now: f64, cfg: &Config) {
        if self.sessions.len() <= MAX_SESSIONS {
            return;
        }
        let ttl = cfg.affinity_ttl_secs as f64;
        self.sessions.retain(|_, b| now - b.last_seen <= ttl);
    }

    /// Folds one completed request into the account's quota and counters.
    pub fn observe(&mut self, now: f64, usage: &Usage, cfg: &Config) -> Observed {
        let mut out = Observed::default();
        if usage.auth_id.is_empty() {
            return out;
        }
        // Was this thread warm on this same account before this request?
        let warm = self
            .sessions
            .get(&usage.session)
            .is_some_and(|b| b.auth_id == usage.auth_id && b.prev_seen > 0.0 && b.last_seen - b.prev_seen <= cfg.cache_ttl_secs as f64);
        let a = self.accounts.entry(usage.auth_id.clone()).or_default();
        if a.provider.is_empty() {
            a.provider = usage.provider.clone();
        }
        a.requests += 1;
        if usage.failed {
            a.failures += 1;
        }
        a.input_tokens += usage.input_tokens;
        a.output_tokens += usage.output_tokens;
        a.cache_read_tokens += usage.cache_read_tokens;
        a.cache_write_tokens += usage.cache_write_tokens;
        let prompt = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
        if warm && !usage.failed && usage.cache_read_tokens == 0 && prompt >= cfg.cache_miss_min_tokens {
            a.cache_misses += 1;
            out.cache_miss = true;
        }

        let windows = quota::parse_windows(&usage.headers, now);
        if windows.is_empty() {
            return out;
        }
        let budget_of = |ws: &BTreeMap<String, Window>| {
            ws.iter()
                .filter(|(n, w)| w.hours >= cfg.budget_min_hours && quota::model_scope(n).is_empty())
                .map(|(_, w)| w.clone())
                .max_by(|x, y| x.hours.total_cmp(&y.hours))
        };
        let before = budget_of(&a.windows);
        let after = budget_of(&windows);
        if let Some(n) = &after {
            match a.burn_base {
                Some((t0, used0, reset0)) if (reset0 - n.reset_at).abs() < 120.0 => {
                    // Same budget period: measure burn once enough time has passed.
                    let dt = now - t0;
                    if dt >= BURN_MIN_GAP_SECS && n.used >= used0 {
                        let rate = (n.used - used0) / (dt / 3600.0);
                        a.burn_per_hour = if a.burn_per_hour > 0.0 { (1.0 - BURN_ALPHA) * a.burn_per_hour + BURN_ALPHA * rate } else { rate };
                        a.burn_base = Some((now, n.used, n.reset_at));
                    }
                }
                Some(_) => {
                    // The budget window rolled over: record what went unspent.
                    if let Some(b) = &before {
                        a.wasted_at_reset.push((now, (1.0 - b.used).max(0.0)));
                        if a.wasted_at_reset.len() > 52 {
                            a.wasted_at_reset.remove(0);
                        }
                    }
                    a.samples.clear();
                    a.burn_base = Some((now, n.used, n.reset_at));
                }
                None => a.burn_base = Some((now, n.used, n.reset_at)),
            }
        }
        for (name, w) in &windows {
            a.windows.insert(name.clone(), w.clone());
        }
        a.last_observed = now;
        if let Some(n) = after {
            if a.samples.last().is_none_or(|(t, _)| now - t >= SAMPLE_GAP_SECS) {
                a.samples.push((now, n.used));
                if a.samples.len() > MAX_SAMPLES {
                    a.samples.remove(0);
                }
            }
        }
        out.windows = windows;
        out
    }

    /// A provider announced a free reset: every account of that provider gains one,
    /// and its recorded usage is cleared because the windows restarted.
    pub fn apply_announcement(&mut self, provider: &str, at: f64, grant: u32, clears_usage: bool) -> usize {
        if self.announcements.get(provider).is_some_and(|t| *t >= at) {
            return 0;
        }
        self.announcements.insert(provider.to_string(), at);
        let mut n = 0;
        for a in self.accounts.values_mut().filter(|a| a.provider.eq_ignore_ascii_case(provider)) {
            a.free_resets += grant;
            if clears_usage {
                a.windows.retain(|_, w| w.observed_at > at);
            }
            n += 1;
        }
        n
    }

    pub fn set_resets(&mut self, auth_id: &str, free: Option<u32>, banked: Option<u32>) -> bool {
        let Some(a) = self.accounts.get_mut(auth_id) else { return false };
        if let Some(v) = free {
            a.free_resets = v;
        }
        if let Some(v) = banked {
            a.banked_resets = v;
        }
        true
    }

    /// Where quota is going to waste, per account. Read by people and by the optimiser.
    pub fn report(&self, now: f64, cfg: &Config) -> Value {
        let mut rows = Vec::new();
        for (id, a) in &self.accounts {
            let s = policy::score(now, "", &self.view(id), cfg);
            // Actual burn over the last 24h, idle time included.
            let day_ago = now - 86_400.0;
            let recent: Vec<&(f64, f64)> = a.samples.iter().filter(|(t, _)| *t >= day_ago).collect();
            let actual = match (recent.first(), recent.last()) {
                (Some(f), Some(l)) if l.0 - f.0 >= 3600.0 && l.1 >= f.1 => (l.1 - f.1) / ((l.0 - f.0) / 3600.0),
                _ => 0.0,
            };
            let projected = (s.budget_used + actual * s.hours_left).min(1.0);
            let prompt = a.input_tokens + a.cache_read_tokens + a.cache_write_tokens;
            let advice = if !s.observed {
                "no quota headers seen yet"
            } else if projected >= cfg.drain_target {
                "on track"
            } else if a.free_resets > 0 {
                "underused; holds a free reset, so new threads start here first"
            } else if s.tier == policy::Tier::AtRisk {
                "underused and at risk; new threads start here first"
            } else {
                "underused; not yet urgent"
            };
            rows.push(json!({
                "auth_id": id,
                "provider": a.provider,
                "tier": s.tier,
                "budget_used": round(s.budget_used),
                "rate_used": round(s.rate_used),
                "hours_to_reset": round(s.hours_left),
                "required_burn_per_hour": round(s.required_burn),
                "burn_per_hour_in_use": round(a.burn_per_hour),
                "burn_per_hour_last_24h": round(actual),
                "projected_used_at_reset": round(projected),
                "projected_waste": round((cfg.drain_target - projected).max(0.0)),
                "free_resets": a.free_resets,
                "banked_resets": a.banked_resets,
                "idle_hours": if a.last_used > 0.0 { round((now - a.last_used) / 3600.0) } else { -1.0 },
                "requests": a.requests,
                "failure_rate": ratio(a.failures as f64, a.requests as f64),
                "cache_read_share": ratio(a.cache_read_tokens as f64, prompt as f64),
                "cache_misses": a.cache_misses,
                "wasted_at_last_reset": a.wasted_at_reset.last().map(|(_, w)| round(*w)),
                "advice": advice,
                "windows": a.windows.iter().map(|(n, w)| (n.clone(), json!({
                    "used": round(w.used_now(now)),
                    "hours_left": round(w.hours_left(now, 0.0)),
                    "hours": w.hours,
                }))).collect::<BTreeMap<_, _>>(),
            }));
        }
        let live = self.sessions.values().filter(|b| now - b.last_seen <= cfg.affinity_ttl_secs as f64).count();
        json!({
            "generated_at": now,
            "routing": cfg.route,
            "drain_target": cfg.drain_target,
            "picks": self.picks,
            "deferred_to_host": self.deferred,
            "gated_picks": self.gated,
            "rebinds": self.rebinds,
            "live_threads": live,
            "accounts": rows,
        })
    }
}

fn round(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn ratio(n: f64, d: f64) -> f64 {
    if d > 0.0 { round(n / d) } else { 0.0 }
}
