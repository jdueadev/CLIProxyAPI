//! The routing decision. Pure: same inputs, same answer, no clock and no I/O.
//!
//! Order of concerns for one request:
//!
//! 0. An account in cooldown (an upstream refused it) or at `block_at` of any
//!    applicable window is never chosen. When that leaves nothing, the answer is
//!    `Exhausted` and the caller returns the final error instead of trying.
//! 1. A thread stays on the account it started on while that account is offered
//!    and has room (provider prompt caches are per account). A thread on an
//!    account at `preempt_at` moves to an account with room *before* the limit
//!    is hit; it stays only when no account has room. Every move is reported.
//! 2. Otherwise prefer the account most at risk of reaching its reset with more
//!    than `1 - drain_target` unspent.
//! 3. Otherwise prefer accounts holding a free reset (spending them first is free).
//! 4. Otherwise prefer the account that needs the highest burn rate to drain.
//!
//! Banked resets never raise an account's rank: they are kept for large tasks.

use crate::config::Config;
use crate::quota::{applies_to_model, Window};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct AccountView {
    pub id: String,
    pub windows: BTreeMap<String, Window>,
    /// Measured burn of the budget window while in use, fraction per hour. 0 when unknown.
    pub burn_per_hour: f64,
    pub free_resets: u32,
    pub banked_resets: u32,
    /// Unix seconds of the last request routed here. 0 when never.
    pub last_used: f64,
    /// Unix seconds until which an upstream refusal applies to this model. 0 when none.
    pub cooldown_until: f64,
    /// True when `cooldown_until` is the default rather than a reported reset.
    pub cooldown_estimated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    AtRisk,
    FreeReset,
    Normal,
    RateLimited,
    /// At or above `preempt_at`: used only when no account has room.
    NearLimit,
    /// At or above `block_at` of an applicable window: never used.
    Blocked,
    /// Refused by the upstream until its reset: never used.
    Cooldown,
}

/// What a client can expect from an account right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Ok,
    NearLimit,
    /// In cooldown or at its limit: not offered until `available_at`.
    Cooldown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Score {
    pub id: String,
    pub tier: Tier,
    pub availability: Availability,
    /// Budget window fraction used now. Unobserved accounts count as 0.
    pub budget_used: f64,
    /// Highest fraction used across applicable rate windows.
    pub rate_used: f64,
    /// Highest fraction used across every applicable window.
    pub limit_used: f64,
    /// Fraction still to spend to reach the drain target.
    pub need: f64,
    pub hours_left: f64,
    /// Burn (fraction per hour) required to reach the target by reset.
    pub required_burn: f64,
    pub observed: bool,
    pub free_resets: u32,
    pub banked_resets: u32,
    /// For an unavailable account, the unix second it becomes usable. For a
    /// near-limit one, when its full windows reset. 0 when unknown or not limited.
    pub available_at: f64,
    /// True when `available_at` rests on an assumed window end or default cooldown.
    pub available_at_estimated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Kept on the account the thread is bound to.
    Affinity,
    /// Chosen by quota policy.
    Policy,
    /// Every candidate is in cooldown or at its limit: nothing is sent upstream.
    Exhausted,
    /// No candidates were offered.
    Empty,
}

/// A thread was moved off its account, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AffinityBreak {
    pub from: String,
    /// `near_limit`, `at_limit` or `cooldown`.
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub choice: Option<String>,
    pub reason: Reason,
    pub scores: Vec<Score>,
    pub affinity_broken: Option<AffinityBreak>,
}

const DEFAULT_BUDGET_HOURS: f64 = 168.0;
const MIN_HOURS: f64 = 0.25;

/// When a window frees up: its reported reset, else the end of its length after
/// the observation (an estimate), else unknown (0).
fn window_end(w: &Window) -> (f64, bool) {
    if w.reset_at > 0.0 {
        (w.reset_at, false)
    } else if w.hours > 0.0 && w.observed_at > 0.0 {
        (w.observed_at + w.hours * 3600.0, true)
    } else {
        (0.0, false)
    }
}

pub fn score(now: f64, model: &str, acct: &AccountView, cfg: &Config) -> Score {
    let mut budget: Option<&Window> = None;
    let mut rate_used: f64 = 0.0;
    let mut limit_used: f64 = 0.0;
    let mut full_until = (0.0_f64, false);
    let mut near_until = (0.0_f64, false);
    for (name, w) in &acct.windows {
        if !applies_to_model(name, model) {
            continue;
        }
        let used = w.used_now(now);
        limit_used = limit_used.max(used);
        let end = window_end(w);
        if used >= cfg.block_at && end.0 > full_until.0 {
            full_until = end;
        }
        if used >= cfg.preempt_at && end.0 > near_until.0 {
            near_until = end;
        }
        if w.hours >= cfg.budget_min_hours {
            // With several budget windows, the fullest one is the binding constraint.
            if budget.is_none_or(|b| used > b.used_now(now)) {
                budget = Some(w);
            }
        } else {
            rate_used = rate_used.max(used);
        }
    }
    let any_full = limit_used >= cfg.block_at;
    let cooling = acct.cooldown_until > now;
    let (budget_used, hours_left) = match budget {
        Some(w) => (w.used_now(now), w.hours_left(now, DEFAULT_BUDGET_HOURS)),
        None => (0.0, DEFAULT_BUDGET_HOURS),
    };
    let need = (cfg.drain_target - budget_used).max(0.0);
    let required_burn = need / hours_left.max(MIN_HOURS);
    let sustainable = if acct.burn_per_hour > 0.0 {
        acct.burn_per_hour
    } else {
        cfg.assumed_burn_per_hour
    };
    let tier = if cooling {
        Tier::Cooldown
    } else if any_full {
        Tier::Blocked
    } else if limit_used >= cfg.preempt_at {
        Tier::NearLimit
    } else if rate_used >= cfg.rate_soft {
        Tier::RateLimited
    } else if need > 0.0 && required_burn >= cfg.risk_factor * sustainable {
        Tier::AtRisk
    } else if acct.free_resets > 0 {
        Tier::FreeReset
    } else {
        Tier::Normal
    };
    let (availability, (available_at, available_at_estimated)) = match tier {
        Tier::Cooldown => {
            // Usable again only when both the refusal and any full window have passed.
            let c = (acct.cooldown_until, acct.cooldown_estimated);
            let at = if full_until.0 > c.0 { full_until } else { c };
            (Availability::Cooldown, at)
        }
        Tier::Blocked => (Availability::Cooldown, full_until),
        Tier::NearLimit => (Availability::NearLimit, near_until),
        _ => (Availability::Ok, (0.0, false)),
    };
    Score {
        id: acct.id.clone(),
        tier,
        availability,
        budget_used,
        rate_used,
        limit_used,
        need,
        hours_left,
        required_burn,
        observed: budget.is_some(),
        free_resets: acct.free_resets,
        banked_resets: acct.banked_resets,
        available_at,
        available_at_estimated,
    }
}

/// Picks an account for one request.
///
/// `candidates` are the accounts the host offers (already filtered for the
/// host's own cooldown, model support and client eligibility). `bound` is the
/// account this thread last ran on, if its binding is still live.
pub fn decide(
    now: f64,
    model: &str,
    candidates: &[AccountView],
    bound: Option<&str>,
    cfg: &Config,
) -> Decision {
    let mut scores: Vec<Score> = candidates
        .iter()
        .map(|a| score(now, model, a, cfg))
        .collect();
    if scores.is_empty() {
        return Decision {
            choice: None,
            reason: Reason::Empty,
            scores,
            affinity_broken: None,
        };
    }
    let any_room = scores.iter().any(|s| s.availability == Availability::Ok);
    let mut affinity_broken = None;
    if let Some(id) = bound {
        if let Some(b) = scores.iter().find(|s| s.id == id) {
            let stay = match b.availability {
                Availability::Ok => true,
                // Keep the warm cache when moving would not buy any room.
                Availability::NearLimit => !any_room,
                Availability::Cooldown => false,
            };
            if stay {
                return Decision {
                    choice: Some(id.to_string()),
                    reason: Reason::Affinity,
                    scores,
                    affinity_broken: None,
                };
            }
            affinity_broken = Some(AffinityBreak {
                from: id.to_string(),
                reason: match b.tier {
                    Tier::Cooldown => "cooldown",
                    Tier::Blocked => "at_limit",
                    _ => "near_limit",
                },
            });
        }
    }
    let last_used = |id: &str| {
        candidates
            .iter()
            .find(|a| a.id == id)
            .map_or(0.0, |a| a.last_used)
    };
    scores.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then(b.required_burn.total_cmp(&a.required_burn))
            .then(b.free_resets.cmp(&a.free_resets))
            .then(last_used(&a.id).total_cmp(&last_used(&b.id)))
            .then(a.id.cmp(&b.id))
    });
    if scores[0].availability == Availability::Cooldown {
        return Decision {
            choice: None,
            reason: Reason::Exhausted,
            scores,
            affinity_broken,
        };
    }
    Decision {
        choice: Some(scores[0].id.clone()),
        reason: Reason::Policy,
        scores,
        affinity_broken,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f64 = 3600.0;

    fn acct(id: &str, weekly_used: f64, weekly_hours_left: f64, five_h_used: f64) -> AccountView {
        let mut windows = BTreeMap::new();
        windows.insert(
            "7d".into(),
            Window {
                used: weekly_used,
                reset_at: weekly_hours_left * H,
                hours: 168.0,
                observed_at: 0.0,
            },
        );
        windows.insert(
            "5h".into(),
            Window {
                used: five_h_used,
                reset_at: 2.0 * H,
                hours: 5.0,
                observed_at: 0.0,
            },
        );
        AccountView {
            id: id.into(),
            windows,
            ..Default::default()
        }
    }

    fn pick(cands: &[AccountView], bound: Option<&str>) -> Decision {
        decide(0.0, "claude-opus-5-5", cands, bound, &Config::default())
    }

    #[test]
    fn empty_candidates() {
        let d = pick(&[], None);
        assert_eq!(d.reason, Reason::Empty);
        assert!(d.choice.is_none());
    }

    #[test]
    fn soonest_reset_with_most_unspent_goes_first() {
        // a: 58% used, resets in 24h -> needs 1.33%/h. b: 0% used, resets in 96h -> 0.94%/h.
        let d = pick(
            &[acct("b", 0.0, 96.0, 0.0), acct("a", 0.58, 24.0, 0.0)],
            None,
        );
        assert_eq!(d.choice.as_deref(), Some("a"));
        assert_eq!(d.reason, Reason::Policy);
        assert_eq!(d.scores[0].tier, Tier::AtRisk);
    }

    #[test]
    fn drained_account_goes_last() {
        let d = pick(
            &[
                acct("drained", 0.95, 10.0, 0.0),
                acct("fresh", 0.10, 150.0, 0.0),
            ],
            None,
        );
        assert_eq!(d.choice.as_deref(), Some("fresh"));
    }

    #[test]
    fn free_reset_beats_normal_but_not_at_risk() {
        // "calm" accounts need under 1%/h (risk threshold with defaults).
        let mut free = acct("free", 0.50, 160.0, 0.0);
        free.free_resets = 1;
        let calm = acct("calm", 0.20, 160.0, 0.0);
        let d = pick(&[calm.clone(), free.clone()], None);
        assert_eq!(d.choice.as_deref(), Some("free"));
        assert_eq!(d.scores[0].tier, Tier::FreeReset);

        let risky = acct("risky", 0.30, 12.0, 0.0);
        let d = pick(&[calm, free, risky], None);
        assert_eq!(d.choice.as_deref(), Some("risky"));
    }

    #[test]
    fn banked_resets_do_not_raise_rank() {
        let mut banked = acct("banked", 0.50, 160.0, 0.0);
        banked.banked_resets = 3;
        let plain = acct("plain", 0.20, 160.0, 0.0);
        let d = pick(&[banked, plain], None);
        assert_eq!(d.choice.as_deref(), Some("plain"));
    }

    #[test]
    fn bound_thread_stays_even_when_another_account_is_at_risk() {
        let d = pick(
            &[
                acct("home", 0.90, 100.0, 0.5),
                acct("risky", 0.30, 12.0, 0.0),
            ],
            Some("home"),
        );
        assert_eq!(d.choice.as_deref(), Some("home"));
        assert_eq!(d.reason, Reason::Affinity);
    }

    #[test]
    fn bound_account_not_offered_falls_to_policy() {
        let d = pick(&[acct("other", 0.30, 12.0, 0.0)], Some("gone"));
        assert_eq!(d.choice.as_deref(), Some("other"));
        assert_eq!(d.reason, Reason::Policy);
    }

    #[test]
    fn full_five_hour_window_blocks_new_threads() {
        let d = pick(
            &[
                acct("full5h", 0.30, 12.0, 0.99),
                acct("ok", 0.20, 160.0, 0.1),
            ],
            None,
        );
        assert_eq!(d.choice.as_deref(), Some("ok"));
        assert_eq!(d.scores.last().unwrap().tier, Tier::Blocked);
    }

    #[test]
    fn busy_five_hour_window_ranks_behind_open_accounts() {
        let d = pick(
            &[acct("busy", 0.30, 12.0, 0.93), acct("ok", 0.20, 160.0, 0.1)],
            None,
        );
        assert_eq!(d.choice.as_deref(), Some("ok"));
        // With nothing else available it is still used.
        let d = pick(&[acct("busy", 0.30, 12.0, 0.93)], None);
        assert_eq!(d.choice.as_deref(), Some("busy"));
    }

    #[test]
    fn all_at_limit_is_exhausted_with_the_earliest_reset() {
        let d = pick(
            &[acct("a", 0.99, 12.0, 0.0), acct("b", 0.2, 12.0, 1.0)],
            None,
        );
        assert_eq!(d.reason, Reason::Exhausted);
        assert!(d.choice.is_none());
        // b's 5h window resets at 2h, a's week at 12h.
        let earliest = d
            .scores
            .iter()
            .map(|s| s.available_at)
            .fold(f64::MAX, f64::min);
        assert_eq!(earliest, 2.0 * H);
    }

    #[test]
    fn near_limit_bound_thread_moves_before_the_limit() {
        // home is at 96% of its week: not yet refused, but inside the margin.
        let d = pick(
            &[
                acct("home", 0.96, 100.0, 0.1),
                acct("spare", 0.30, 100.0, 0.1),
            ],
            Some("home"),
        );
        assert_eq!(d.choice.as_deref(), Some("spare"));
        assert_eq!(
            d.affinity_broken,
            Some(AffinityBreak {
                from: "home".into(),
                reason: "near_limit"
            })
        );
        // With no account that has room, the thread keeps its warm cache.
        let d = pick(&[acct("home", 0.96, 100.0, 0.1)], Some("home"));
        assert_eq!(
            (d.choice.as_deref(), d.reason),
            (Some("home"), Reason::Affinity)
        );
    }

    #[test]
    fn cooldown_is_never_chosen_even_for_a_bound_thread() {
        let cfg = Config::default();
        let mut home = acct("home", 0.10, 100.0, 0.1);
        home.cooldown_until = 500.0;
        let other = acct("b", 0.5, 100.0, 0.1);
        let d = decide(100.0, "m", &[home.clone(), other], Some("home"), &cfg);
        assert_eq!(d.choice.as_deref(), Some("b"));
        assert_eq!(d.affinity_broken.unwrap().reason, "cooldown");
        let d = decide(100.0, "m", &[home.clone()], Some("home"), &cfg);
        assert_eq!(d.reason, Reason::Exhausted);
        assert_eq!(d.scores[0].available_at, 500.0);
        // After the cooldown it is offered again.
        let d = decide(600.0, "m", &[home], Some("home"), &cfg);
        assert_eq!(d.reason, Reason::Affinity);
    }

    #[test]
    fn unobserved_account_is_probed_before_a_calm_one() {
        // No headers seen yet: treated as a full, untouched week (0.9/168h = 0.54%/h).
        let unknown = AccountView {
            id: "unknown".into(),
            ..Default::default()
        };
        let calm = acct("calm", 0.60, 160.0, 0.0);
        let d = pick(&[calm, unknown], None);
        assert_eq!(d.choice.as_deref(), Some("unknown"));
        assert!(!d.scores[0].observed);
    }

    #[test]
    fn expired_window_counts_as_empty() {
        let mut a = acct("a", 0.99, 1.0, 0.99);
        a.windows.get_mut("5h").unwrap().reset_at = 1.0 * H;
        let d = decide(2.0 * H, "m", &[a], None, &Config::default());
        assert_eq!(d.reason, Reason::Policy);
        assert_eq!(d.scores[0].budget_used, 0.0);
    }

    #[test]
    fn model_scoped_window_only_blocks_that_model() {
        let mut a = acct("a", 0.2, 100.0, 0.0);
        a.windows.insert(
            "7d_opus".into(),
            Window {
                used: 1.0,
                reset_at: 100.0 * H,
                hours: 168.0,
                observed_at: 0.0,
            },
        );
        let cfg = Config::default();
        assert_eq!(
            decide(0.0, "claude-opus-5-5", &[a.clone()], None, &cfg).reason,
            Reason::Exhausted
        );
        assert_eq!(
            decide(0.0, "claude-sonnet-5-5", &[a], None, &cfg).reason,
            Reason::Policy
        );
    }

    #[test]
    fn measured_burn_changes_risk() {
        // Needs 0.9%/h. Default sustainable 2%/h -> threshold 1%/h -> not at risk.
        let mut a = acct("a", 0.0, 100.0, 0.0);
        assert_eq!(score(0.0, "m", &a, &Config::default()).tier, Tier::Normal);
        // Measured at 1%/h -> threshold 0.5%/h -> at risk.
        a.burn_per_hour = 0.01;
        assert_eq!(score(0.0, "m", &a, &Config::default()).tier, Tier::AtRisk);
    }

    #[test]
    fn ties_break_by_least_recently_used_then_id() {
        let mut a = acct("a", 0.2, 160.0, 0.0);
        let mut b = acct("b", 0.2, 160.0, 0.0);
        a.last_used = 50.0;
        b.last_used = 10.0;
        assert_eq!(
            pick(&[a.clone(), b.clone()], None).choice.as_deref(),
            Some("b")
        );
        b.last_used = 50.0;
        assert_eq!(pick(&[b, a], None).choice.as_deref(), Some("a"));
    }
}
