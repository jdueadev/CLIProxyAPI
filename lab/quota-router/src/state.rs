//! Everything the plugin remembers: per-account quota, thread bindings, and the
//! counters behind the report. Mutated only through `pick` and `observe`.

use crate::config::Config;
use crate::limits::{self, Alternative, Cooldown, Exhausted};
use crate::policy::{self, AccountView, Availability, Decision, Reason, Score, Tier};
use crate::quota::{self, Window};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Version of the `status` document (`GET /quota-router/status`, `quota-tools status`).
pub const STATUS_SCHEMA_VERSION: u32 = 1;
/// A pool member the host has not offered for this long is dropped.
const POOL_TTL_SECS: f64 = 7.0 * 86_400.0;
const MAX_ALTERNATIVES: usize = 20;
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
    /// Upstream refusals still in force, by model scope ("" covers every model).
    pub cooldowns: BTreeMap<String, Cooldown>,
}

/// One account the host has offered for a model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolMember {
    pub provider: String,
    pub subscription: bool,
    pub last_seen: f64,
}

/// The accounts the host offers for one model, learned from its picks. This is
/// what lets the router answer "nothing can serve" before the host tries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pool {
    /// Model name as the host sent it.
    pub model: String,
    pub members: BTreeMap<String, PoolMember>,
    /// Unix seconds of the last pick for this model.
    pub updated_at: f64,
    /// Set when a new account appeared for one of this pool's providers while
    /// the model was exhausted: the next request goes to the host to see it.
    pub stale: bool,
}

/// Lower-cased model name without a trailing `(...)` thinking suffix.
pub fn model_key(model: &str) -> String {
    let m = model.trim();
    let m = match (m.rfind('('), m.ends_with(')')) {
        (Some(i), true) if i > 0 => &m[..i],
        _ => m,
    };
    m.trim().to_ascii_lowercase()
}

fn scope_applies(scope: &str, model: &str) -> bool {
    scope.is_empty()
        || model
            .to_ascii_lowercase()
            .contains(&scope.to_ascii_lowercase())
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
    /// Model key -> the accounts the host offers for it.
    pub pools: BTreeMap<String, Pool>,
    /// Model key -> unix seconds since every account for it has been unavailable.
    pub exhausted: BTreeMap<String, f64>,
    /// Threads moved off their account before (or because of) a limit.
    pub affinity_breaks: u64,
    /// Requests answered with the final error instead of being sent upstream.
    pub refused: u64,
    /// Key for account labels. Generated once, kept private in this file.
    pub label_key: [u64; 2],
    /// Unix seconds this state was last written.
    pub saved_at: f64,
    /// The plugin config in force when this state was written, so offline
    /// readers (`quota-tools status`) apply the same thresholds.
    pub settings: Option<Config>,
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
    /// Every account for the model is unavailable: refuse without sending it upstream.
    Exhausted(Exhausted),
}

pub struct PickResult {
    pub outcome: PickOutcome,
    pub decision: Option<Decision>,
    pub bound_before: Option<String>,
    pub gated: usize,
    /// `exhausted` the first time a model has no account left, `recovered` the
    /// first time it is served again. Used to log transitions, not every request.
    pub transition: Option<&'static str>,
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
    /// HTTP status of a failure, 0 when none was reported.
    pub status: i64,
    /// Failure body or message (never logged; read for a reset time).
    pub failure_body: String,
}

#[derive(Debug, Default)]
pub struct Observed {
    pub windows: BTreeMap<String, Window>,
    /// Set when a warm thread sent a large prompt and read nothing from cache.
    pub cache_miss: bool,
    /// Set when this response put the account into cooldown: (model scope, cooldown).
    pub cooldown: Option<(String, Cooldown)>,
}

fn client_allowed(cfg: &Config, user_agent: &str) -> bool {
    let ua = user_agent.to_ascii_lowercase();
    cfg.claude_clients
        .iter()
        .any(|p| !p.is_empty() && ua.starts_with(&p.to_ascii_lowercase()))
}

impl State {
    fn live_binding(&self, session: &str, now: f64, cfg: &Config) -> Option<&Binding> {
        if session.is_empty() {
            return None;
        }
        self.sessions
            .get(session)
            .filter(|b| now - b.last_seen <= cfg.affinity_ttl_secs as f64)
    }

    /// What policy sees of one account for one model at `now`.
    fn view(&self, id: &str, model: &str, now: f64) -> AccountView {
        match self.accounts.get(id) {
            Some(a) => {
                let cooldown = a
                    .cooldowns
                    .iter()
                    .filter(|(scope, c)| c.active(now) && scope_applies(scope, model))
                    .map(|(_, c)| c)
                    .max_by(|x, y| x.until.total_cmp(&y.until));
                AccountView {
                    id: id.to_string(),
                    windows: a.windows.clone(),
                    burn_per_hour: a.burn_per_hour,
                    free_resets: a.free_resets,
                    banked_resets: a.banked_resets,
                    last_used: a.last_used,
                    cooldown_until: cooldown.map_or(0.0, |c| c.until),
                    cooldown_estimated: cooldown.is_some_and(|c| c.estimated),
                }
            }
            None => AccountView {
                id: id.to_string(),
                ..Default::default()
            },
        }
    }

    /// Stable, non-reversible label for an account (see `limits::label`).
    pub fn label(&self, auth_id: &str) -> String {
        limits::label(self.label_key, auth_id)
    }

    /// The account a label names, for routes that accept labels.
    pub fn auth_for_label(&self, label: &str) -> Option<String> {
        self.accounts
            .keys()
            .find(|id| self.label(id) == label)
            .cloned()
    }

    /// Records the candidates the host offered for `model`. A candidate never
    /// seen before marks every pool of its provider for a refresh, so an account
    /// added while a model was exhausted is found on the next request.
    fn learn_pool(&mut self, now: f64, model: &str, candidates: &[Candidate]) {
        let key = model_key(model);
        if key.is_empty() {
            return;
        }
        for c in candidates {
            let known = self.accounts.contains_key(&c.id)
                || self.pools.values().any(|p| p.members.contains_key(&c.id));
            if !known {
                for (k, pool) in self.pools.iter_mut() {
                    if self.exhausted.contains_key(k)
                        && pool
                            .members
                            .values()
                            .any(|m| m.provider.eq_ignore_ascii_case(&c.provider))
                    {
                        pool.stale = true;
                    }
                }
            }
        }
        let pool = self.pools.entry(key).or_default();
        pool.model = model.to_string();
        pool.updated_at = now;
        pool.stale = false;
        for c in candidates {
            let m = pool.members.entry(c.id.clone()).or_default();
            m.provider = c.provider.clone();
            m.subscription = c.subscription;
            m.last_seen = now;
        }
        pool.members
            .retain(|_, m| now - m.last_seen <= POOL_TTL_SECS);
    }

    /// Pool members this client may use, as (id, provider).
    fn usable_members(&self, pool: &Pool, user_agent: &str, cfg: &Config) -> Vec<(String, String)> {
        let allowed = !cfg.gate_claude_subs || client_allowed(cfg, user_agent);
        pool.members
            .iter()
            .filter(|(_, m)| cfg.schedules(&m.provider))
            .filter(|(_, m)| {
                allowed || !(m.subscription && m.provider.eq_ignore_ascii_case("claude"))
            })
            .map(|(id, m)| (id.clone(), m.provider.clone()))
            .collect()
    }

    /// Other models this client could use right now, with the provider of the
    /// account the router would pick for each.
    fn alternatives(
        &self,
        now: f64,
        model: &str,
        user_agent: &str,
        cfg: &Config,
    ) -> Vec<Alternative> {
        let key = model_key(model);
        let mut out = Vec::new();
        for (k, pool) in &self.pools {
            if *k == key {
                continue;
            }
            let members = self.usable_members(pool, user_agent, cfg);
            let views: Vec<AccountView> = members
                .iter()
                .map(|(id, _)| self.view(id, &pool.model, now))
                .collect();
            let d = policy::decide(now, &pool.model, &views, None, cfg);
            if let Some(id) = d.choice {
                let provider = members
                    .iter()
                    .find(|(m, _)| *m == id)
                    .map(|(_, p)| p.clone())
                    .unwrap_or_default();
                out.push(Alternative {
                    model: pool.model.clone(),
                    provider,
                });
            }
            if out.len() >= MAX_ALTERNATIVES {
                break;
            }
        }
        out
    }

    fn exhausted_from(
        &self,
        now: f64,
        model: &str,
        scores: &[Score],
        providers: Vec<String>,
        user_agent: &str,
        cfg: &Config,
    ) -> Exhausted {
        let earliest = scores
            .iter()
            .filter(|s| s.available_at > now)
            .min_by(|a, b| a.available_at.total_cmp(&b.available_at));
        let probe = cfg.probe_after_secs as f64;
        let recheck_at = self
            .pools
            .get(&model_key(model))
            .map_or(now + probe, |p| (p.updated_at + probe).max(now));
        let mut providers = providers;
        providers.sort();
        providers.dedup();
        Exhausted {
            model: model.to_string(),
            providers,
            earliest_reset: earliest.map_or(0.0, |s| s.available_at),
            reset_estimated: earliest.is_some_and(|s| s.available_at_estimated),
            recheck_at,
            accounts: scores.len(),
            in_cooldown: scores.iter().filter(|s| s.tier == Tier::Cooldown).count(),
            at_limit: scores.iter().filter(|s| s.tier == Tier::Blocked).count(),
            alternatives: self.alternatives(now, model, user_agent, cfg),
        }
    }

    /// Before the host picks: when the router already knows every account for
    /// this model is unavailable, the final error to return instead of trying.
    /// The second value is true the first time the model is seen exhausted.
    pub fn preflight(
        &mut self,
        now: f64,
        model: &str,
        user_agent: &str,
        cfg: &Config,
    ) -> Option<(Exhausted, bool)> {
        if !cfg.route || !cfg.final_error {
            return None;
        }
        let key = model_key(model);
        let pool = self.pools.get(&key)?;
        // Let one request through now and then so the host can offer accounts
        // added since the last pick; the pick refuses it too if nothing changed.
        if pool.stale || now - pool.updated_at >= cfg.probe_after_secs as f64 {
            return None;
        }
        let members = self.usable_members(pool, user_agent, cfg);
        if members.is_empty() {
            return None;
        }
        let scores: Vec<Score> = members
            .iter()
            .map(|(id, _)| policy::score(now, model, &self.view(id, model, now), cfg))
            .collect();
        if scores
            .iter()
            .any(|s| s.availability != Availability::Cooldown)
        {
            return None;
        }
        let providers = members.into_iter().map(|(_, p)| p).collect();
        let ex = self.exhausted_from(now, model, &scores, providers, user_agent, cfg);
        self.refused += 1;
        let first = !self.exhausted.contains_key(&key);
        self.exhausted.entry(key).or_insert(now);
        Some((ex, first))
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
                transition: None,
            };
        }
        // Providers we do not schedule, and observe-only mode, go to the host. Gating
        // above still applied: if anything was removed we must answer ourselves.
        let ours = eligible.iter().all(|c| cfg.schedules(&c.provider));
        if (!cfg.route || !ours) && gated == 0 {
            self.deferred += 1;
            return PickResult {
                outcome: PickOutcome::Defer,
                decision: None,
                bound_before: None,
                gated,
                transition: None,
            };
        }

        let mut bound = self
            .live_binding(&input.session, now, cfg)
            .map(|b| b.auth_id.clone());
        if bound.is_none() {
            // A subagent starts on its parent's account so it shares the parent's cache.
            bound = self
                .live_binding(&input.parent_session, now, cfg)
                .map(|b| b.auth_id.clone());
        }
        if cfg.route {
            self.learn_pool(now, &input.model, &input.candidates);
        }
        let views: Vec<AccountView> = eligible
            .iter()
            .map(|c| self.view(&c.id, &input.model, now))
            .collect();
        let decision = policy::decide(now, &input.model, &views, bound.as_deref(), cfg);
        let key = model_key(&input.model);
        let choice = match (&decision.choice, decision.reason) {
            (Some(id), _) => id.clone(),
            // Every account is in cooldown or at its limit: sending the request
            // anywhere would only add to the limit. Refuse it here.
            (None, Reason::Exhausted) => {
                let providers = eligible.iter().map(|c| c.provider.clone()).collect();
                let ex = self.exhausted_from(
                    now,
                    &input.model,
                    &decision.scores,
                    providers,
                    &input.user_agent,
                    cfg,
                );
                self.refused += 1;
                let first = !self.exhausted.contains_key(&key);
                self.exhausted.entry(key).or_insert(now);
                return PickResult {
                    outcome: PickOutcome::Exhausted(ex),
                    decision: Some(decision),
                    bound_before: bound,
                    gated,
                    transition: first.then_some("exhausted"),
                };
            }
            _ => {
                self.deferred += 1;
                return PickResult {
                    outcome: PickOutcome::Defer,
                    decision: Some(decision),
                    bound_before: bound,
                    gated,
                    transition: None,
                };
            }
        };
        let transition = self.exhausted.remove(&key).map(|_| "recovered");
        if decision.affinity_broken.is_some() {
            self.affinity_breaks += 1;
        }
        for c in &eligible {
            if c.id == choice {
                let a = self.accounts.entry(choice.clone()).or_default();
                a.provider = c.provider.clone();
                a.last_used = now;
            }
        }
        if !input.session.is_empty() {
            let moved = self
                .live_binding(&input.session, now, cfg)
                .is_some_and(|b| b.auth_id != choice);
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
        PickResult {
            outcome: PickOutcome::Choose(choice),
            decision: Some(decision),
            bound_before: bound,
            gated,
            transition,
        }
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
        let warm = self.sessions.get(&usage.session).is_some_and(|b| {
            b.auth_id == usage.auth_id
                && b.prev_seen > 0.0
                && b.last_seen - b.prev_seen <= cfg.cache_ttl_secs as f64
        });
        let a = self.accounts.entry(usage.auth_id.clone()).or_default();
        if a.provider.is_empty() {
            a.provider = usage.provider.clone();
        }
        a.requests += 1;
        if usage.failed {
            a.failures += 1;
            if let Some((scope, c)) = limits::cooldown_from_failure(
                usage.status,
                &usage.headers,
                &usage.failure_body,
                now,
                cfg.cooldown_default_secs,
            ) {
                a.cooldowns.insert(scope.clone(), c.clone());
                out.cooldown = Some((scope, c));
            }
        } else {
            // A served request proves the account works for this model again.
            let model = usage.model.clone();
            a.cooldowns.retain(|scope, _| !scope_applies(scope, &model));
        }
        a.cooldowns.retain(|_, c| c.active(now));
        a.input_tokens += usage.input_tokens;
        a.output_tokens += usage.output_tokens;
        a.cache_read_tokens += usage.cache_read_tokens;
        a.cache_write_tokens += usage.cache_write_tokens;
        let prompt = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
        if warm
            && !usage.failed
            && usage.cache_read_tokens == 0
            && prompt >= cfg.cache_miss_min_tokens
        {
            a.cache_misses += 1;
            out.cache_miss = true;
        }

        let windows = quota::parse_windows(&usage.headers, now);
        if windows.is_empty() {
            return out;
        }
        let budget_of = |ws: &BTreeMap<String, Window>| {
            ws.iter()
                .filter(|(n, w)| {
                    w.hours >= cfg.budget_min_hours && quota::model_scope(n).is_empty()
                })
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
                        a.burn_per_hour = if a.burn_per_hour > 0.0 {
                            (1.0 - BURN_ALPHA) * a.burn_per_hour + BURN_ALPHA * rate
                        } else {
                            rate
                        };
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
            if a.samples
                .last()
                .is_none_or(|(t, _)| now - t >= SAMPLE_GAP_SECS)
            {
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
    pub fn apply_announcement(
        &mut self,
        provider: &str,
        at: f64,
        grant: u32,
        clears_usage: bool,
    ) -> usize {
        if self.announcements.get(provider).is_some_and(|t| *t >= at) {
            return 0;
        }
        self.announcements.insert(provider.to_string(), at);
        let mut n = 0;
        for a in self
            .accounts
            .values_mut()
            .filter(|a| a.provider.eq_ignore_ascii_case(provider))
        {
            a.free_resets += grant;
            if clears_usage {
                a.windows.retain(|_, w| w.observed_at > at);
            }
            n += 1;
        }
        n
    }

    /// Restores a cooldown recorded in the event log after the state file was
    /// last written. Returns false when it has already passed or is not newer.
    pub fn restore_cooldown(&mut self, now: f64, auth_id: &str, scope: &str, c: Cooldown) -> bool {
        if auth_id.is_empty() || !c.active(now) {
            return false;
        }
        let a = self.accounts.entry(auth_id.to_string()).or_default();
        if a.cooldowns
            .get(scope)
            .is_some_and(|old| old.until >= c.until)
        {
            return false;
        }
        a.cooldowns.insert(scope.to_string(), c);
        true
    }

    pub fn set_resets(&mut self, auth_id: &str, free: Option<u32>, banked: Option<u32>) -> bool {
        let Some(a) = self.accounts.get_mut(auth_id) else {
            return false;
        };
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
            let s = policy::score(now, "", &self.view(id, "", now), cfg);
            // Actual burn over the last 24h, idle time included.
            let day_ago = now - 86_400.0;
            let recent: Vec<&(f64, f64)> =
                a.samples.iter().filter(|(t, _)| *t >= day_ago).collect();
            let actual = match (recent.first(), recent.last()) {
                (Some(f), Some(l)) if l.0 - f.0 >= 3600.0 && l.1 >= f.1 => {
                    (l.1 - f.1) / ((l.0 - f.0) / 3600.0)
                }
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
        let live = self
            .sessions
            .values()
            .filter(|b| now - b.last_seen <= cfg.affinity_ttl_secs as f64)
            .count();
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

fn when(at: f64) -> Value {
    if at > 0.0 {
        json!(limits::iso8601(at))
    } else {
        Value::Null
    }
}

fn when_unix(at: f64) -> Value {
    if at > 0.0 {
        json!(at.ceil() as i64)
    } else {
        Value::Null
    }
}

fn availability_name(a: Availability) -> &'static str {
    match a {
        Availability::Ok => "ok",
        Availability::NearLimit => "near_limit",
        Availability::Cooldown => "cooldown",
    }
}

fn limit_reason(s: &Score) -> Value {
    match s.tier {
        Tier::Cooldown => json!("upstream_refused"),
        Tier::Blocked => json!("at_limit"),
        Tier::NearLimit => json!("near_limit"),
        _ => Value::Null,
    }
}

impl State {
    /// The read-only status document clients poll to pre-empt limits on their
    /// side. Accounts appear only by label; no identifier or secret is included.
    pub fn status(&self, now: f64, cfg: &Config) -> Value {
        let mut accounts: Vec<(String, Value)> = Vec::new();
        for (id, a) in &self.accounts {
            let s = policy::score(now, "", &self.view(id, "", now), cfg);
            let cooldown = a
                .cooldowns
                .values()
                .filter(|c| c.active(now))
                .map(|c| c.until)
                .fold(0.0, f64::max);
            let label = self.label(id);
            accounts.push((
                label.clone(),
                json!({
                    "label": label,
                    "provider": a.provider,
                    "state": availability_name(s.availability),
                    "reason": limit_reason(&s),
                    "used": round(s.limit_used),
                    "reset_at": when(s.available_at),
                    "reset_at_unix": when_unix(s.available_at),
                    "reset_estimated": s.available_at_estimated,
                    "cooldown_until": when(cooldown),
                    "free_resets": a.free_resets,
                    "banked_resets": a.banked_resets,
                    "windows": a.windows.iter().map(|(n, w)| json!({
                        "name": n,
                        "used": round(w.used_now(now)),
                        "reset_at": when(if w.used_now(now) > 0.0 { w.reset_at } else { 0.0 }),
                    })).collect::<Vec<_>>(),
                }),
            ));
        }
        accounts.sort_by(|x, y| x.0.cmp(&y.0));
        let mut models = Vec::new();
        for pool in self.pools.values() {
            let members = self.usable_members(
                pool,
                "",
                &Config {
                    gate_claude_subs: false,
                    ..cfg.clone()
                },
            );
            let views: Vec<AccountView> = members
                .iter()
                .map(|(id, _)| self.view(id, &pool.model, now))
                .collect();
            let d = policy::decide(now, &pool.model, &views, None, cfg);
            let count = |a: Availability| d.scores.iter().filter(|s| s.availability == a).count();
            let (ok, near, cool) = (
                count(Availability::Ok),
                count(Availability::NearLimit),
                count(Availability::Cooldown),
            );
            let state = if d.scores.is_empty() {
                "unknown"
            } else if ok > 0 {
                "ok"
            } else if near > 0 {
                "near_limit"
            } else {
                "exhausted"
            };
            let earliest = d
                .scores
                .iter()
                .filter(|s| s.availability == Availability::Cooldown && s.available_at > now)
                .map(|s| s.available_at)
                .fold(f64::INFINITY, f64::min);
            let earliest = if earliest.is_finite() && ok == 0 && near == 0 {
                earliest
            } else {
                0.0
            };
            let mut providers: Vec<&String> = members.iter().map(|(_, p)| p).collect();
            providers.sort();
            providers.dedup();
            let mut member_rows: Vec<Value> = d
                .scores
                .iter()
                .map(|s| {
                    json!({
                        "label": self.label(&s.id),
                        "state": availability_name(s.availability),
                        "reason": limit_reason(s),
                        "used": round(s.limit_used),
                        "reset_at": when(s.available_at),
                    })
                })
                .collect();
            member_rows.sort_by(|x, y| x["label"].as_str().cmp(&y["label"].as_str()));
            models.push(json!({
                "model": pool.model,
                "providers": providers,
                "state": state,
                "would_pick": d.choice.as_deref().map(|id| self.label(id)),
                "earliest_reset_at": when(earliest),
                "earliest_reset_unix": when_unix(earliest),
                "accounts": {"total": d.scores.len(), "ok": ok, "near_limit": near, "cooldown": cool},
                "members": member_rows,
            }));
        }
        json!({
            "schema_version": STATUS_SCHEMA_VERSION,
            "generated_at": limits::iso8601(now),
            "generated_at_unix": now.floor() as i64,
            "state_saved_at": when(self.saved_at),
            "routing": cfg.route,
            "final_error": cfg.final_error,
            "thresholds": {"preempt_at": cfg.preempt_at, "block_at": cfg.block_at},
            "refused": self.refused,
            "affinity_breaks": self.affinity_breaks,
            "accounts": accounts.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            "models": models,
        })
    }
}

fn round(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn ratio(n: f64, d: f64) -> f64 {
    if d > 0.0 {
        round(n / d)
    } else {
        0.0
    }
}
