use crate::config::Config;
use crate::policy::Reason;
use crate::state::*;
use std::collections::BTreeMap;

const CC: &str = "claude-cli/2.1.321 (external, cli)";
const H: f64 = 3600.0;

fn cand(id: &str, provider: &str, subscription: bool) -> Candidate {
    Candidate { id: id.into(), provider: provider.into(), subscription }
}

fn input(session: &str, ua: &str, candidates: Vec<Candidate>) -> PickInput {
    PickInput { model: "claude-opus-5-5".into(), session: session.into(), parent_session: String::new(), user_agent: ua.into(), candidates }
}

fn claude_headers(week: f64, week_reset: f64, five: f64) -> BTreeMap<String, String> {
    [
        ("anthropic-ratelimit-unified-7d-utilization", week.to_string()),
        ("anthropic-ratelimit-unified-7d-reset", week_reset.to_string()),
        ("anthropic-ratelimit-unified-5h-utilization", five.to_string()),
        ("anthropic-ratelimit-unified-5h-reset", (1000.0 * H).to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn usage(auth: &str, session: &str, headers: BTreeMap<String, String>) -> Usage {
    Usage { auth_id: auth.into(), provider: "claude".into(), session: session.into(), headers, ..Default::default() }
}

fn chosen(r: &PickResult) -> &str {
    match &r.outcome {
        PickOutcome::Choose(id) => id,
        other => panic!("expected a choice, got {other:?}"),
    }
}

#[test]
fn other_clients_cannot_use_claude_subscriptions() {
    let cfg = Config::default();
    let mut s = State::default();
    let r = s.pick(0.0, &input("t", "curl/8.0", vec![cand("sub", "claude", true)]), &cfg);
    assert!(matches!(r.outcome, PickOutcome::Reject(_)));
    assert_eq!(r.gated, 1);
    // An API key for the same provider is still usable.
    let r = s.pick(0.0, &input("t", "curl/8.0", vec![cand("sub", "claude", true), cand("key", "claude", false)]), &cfg);
    assert_eq!(chosen(&r), "key");
    // Claude Code may use the subscription.
    let r = s.pick(0.0, &input("t2", CC, vec![cand("sub", "claude", true)]), &cfg);
    assert_eq!(chosen(&r), "sub");
    // Codex subscriptions are not gated.
    let r = s.pick(0.0, &input("t3", "curl/8.0", vec![cand("cx", "codex", true)]), &cfg);
    assert_eq!(chosen(&r), "cx");
}

#[test]
fn gate_can_be_switched_off() {
    let cfg = Config { gate_claude_subs: false, ..Config::default() };
    let mut s = State::default();
    let r = s.pick(0.0, &input("t", "curl/8.0", vec![cand("sub", "claude", true)]), &cfg);
    assert_eq!(chosen(&r), "sub");
}

#[test]
fn observe_only_mode_and_foreign_providers_defer() {
    let mut s = State::default();
    let off = Config { route: false, ..Config::default() };
    assert!(matches!(s.pick(0.0, &input("t", CC, vec![cand("a", "claude", true)]), &off).outcome, PickOutcome::Defer));
    let cfg = Config::default();
    assert!(matches!(s.pick(0.0, &input("t", CC, vec![cand("x", "xai", true)]), &cfg).outcome, PickOutcome::Defer));
    assert_eq!(s.deferred, 2);
}

#[test]
fn observe_only_mode_still_gates() {
    let mut s = State::default();
    let off = Config { route: false, ..Config::default() };
    let r = s.pick(0.0, &input("t", "curl/8.0", vec![cand("sub", "claude", true)]), &off);
    assert!(matches!(r.outcome, PickOutcome::Reject(_)));
}

#[test]
fn thread_keeps_its_account_until_the_ttl_passes() {
    let cfg = Config::default();
    let mut s = State::default();
    let pool = || vec![cand("a", "claude", true), cand("b", "claude", true)];
    // a is the at-risk account, so the thread starts there.
    s.observe(0.0, &usage("a", "", claude_headers(0.30, 12.0 * H, 0.1)), &cfg);
    s.observe(0.0, &usage("b", "", claude_headers(0.20, 160.0 * H, 0.1)), &cfg);
    assert_eq!(chosen(&s.pick(10.0, &input("t", CC, pool()), &cfg)), "a");
    // a is now nearly drained and b would win a fresh pick, but the thread stays.
    s.observe(20.0, &usage("a", "t", claude_headers(0.95, 12.0 * H, 0.1)), &cfg);
    let r = s.pick(30.0, &input("t", CC, pool()), &cfg);
    assert_eq!(chosen(&r), "a");
    assert_eq!(r.decision.unwrap().reason, Reason::Affinity);
    assert_eq!(chosen(&s.pick(30.0, &input("fresh", CC, pool()), &cfg)), "b");
    // After the idle TTL the cache is cold and policy decides again.
    let later = 30.0 + cfg.affinity_ttl_secs as f64 + 1.0;
    assert_eq!(chosen(&s.pick(later, &input("t", CC, pool()), &cfg)), "b");
    assert_eq!(s.rebinds, 0, "an expired binding is not a rebind");
}

#[test]
fn losing_the_bound_account_is_counted_as_a_rebind() {
    let cfg = Config::default();
    let mut s = State::default();
    assert_eq!(chosen(&s.pick(0.0, &input("t", CC, vec![cand("a", "claude", true)]), &cfg)), "a");
    assert_eq!(chosen(&s.pick(5.0, &input("t", CC, vec![cand("b", "claude", true)]), &cfg)), "b");
    assert_eq!(s.rebinds, 1);
}

#[test]
fn subagent_starts_on_the_parent_account() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(0.0, &usage("a", "", claude_headers(0.95, 100.0 * H, 0.1)), &cfg);
    s.observe(0.0, &usage("b", "", claude_headers(0.10, 100.0 * H, 0.1)), &cfg);
    assert_eq!(chosen(&s.pick(1.0, &input("parent", CC, vec![cand("a", "claude", true)]), &cfg)), "a");
    let mut child = input("child", CC, vec![cand("a", "claude", true), cand("b", "claude", true)]);
    child.parent_session = "parent".into();
    assert_eq!(chosen(&s.pick(2.0, &child, &cfg)), "a");
}

#[test]
fn burn_rate_is_measured_across_observations() {
    let cfg = Config::default();
    let mut s = State::default();
    let reset = 100.0 * H;
    s.observe(0.0, &usage("a", "", claude_headers(0.10, reset, 0.1)), &cfg);
    // Too soon to measure, but the fresh reading is used for scheduling.
    s.observe(60.0, &usage("a", "", claude_headers(0.11, reset, 0.1)), &cfg);
    assert_eq!(s.accounts["a"].burn_per_hour, 0.0);
    assert_eq!(s.accounts["a"].windows["7d"].used, 0.11);
    // One hour after the baseline: 4% burned.
    s.observe(3600.0, &usage("a", "", claude_headers(0.14, reset, 0.1)), &cfg);
    assert!((s.accounts["a"].burn_per_hour - 0.04).abs() < 1e-9);
}

#[test]
fn unspent_quota_is_recorded_when_the_week_rolls_over() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(0.0, &usage("a", "", claude_headers(0.60, 1000.0, 0.1)), &cfg);
    s.observe(2000.0, &usage("a", "", claude_headers(0.01, 1000.0 + 7.0 * 86400.0, 0.1)), &cfg);
    let wasted = &s.accounts["a"].wasted_at_reset;
    assert_eq!(wasted.len(), 1);
    assert!((wasted[0].1 - 0.40).abs() < 1e-9);
}

#[test]
fn warm_thread_with_no_cache_read_is_a_cache_miss() {
    let cfg = Config::default();
    let mut s = State::default();
    let pool = || vec![cand("a", "claude", true)];
    let big = |read: i64| Usage { input_tokens: 9000, cache_read_tokens: read, ..usage("a", "t", BTreeMap::new()) };
    s.pick(0.0, &input("t", CC, pool()), &cfg);
    assert!(!s.observe(1.0, &big(0), &cfg).cache_miss, "first turn has nothing cached");
    s.pick(60.0, &input("t", CC, pool()), &cfg);
    assert!(!s.observe(61.0, &big(8000), &cfg).cache_miss);
    s.pick(120.0, &input("t", CC, pool()), &cfg);
    assert!(s.observe(121.0, &big(0), &cfg).cache_miss);
    // A turn after the cache TTL is expected to miss and is not flagged.
    s.pick(520.0, &input("t", CC, pool()), &cfg);
    assert!(!s.observe(521.0, &big(0), &cfg).cache_miss);
    assert_eq!(s.accounts["a"].cache_misses, 1);
}

#[test]
fn announcement_grants_a_free_reset_once() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(0.0, &usage("a", "", claude_headers(0.60, 9000.0, 0.1)), &cfg);
    assert_eq!(s.apply_announcement("claude", 100.0, 1, true), 1);
    assert_eq!(s.apply_announcement("claude", 100.0, 1, true), 0);
    let a = &s.accounts["a"];
    assert_eq!(a.free_resets, 1);
    assert!(a.windows.is_empty(), "usage observed before the reset is stale");
    assert_eq!(s.apply_announcement("codex", 100.0, 1, false), 0);
}

#[test]
fn report_flags_underused_accounts() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(0.0, &usage("idle", "", claude_headers(0.05, 30.0 * H, 0.0)), &cfg);
    let report = s.report(10.0, &cfg);
    let row = &report["accounts"][0];
    assert_eq!(row["auth_id"], "idle");
    assert!(row["projected_waste"].as_f64().unwrap() > 0.8);
    assert_eq!(row["tier"], "at_risk");
    assert!(row["advice"].as_str().unwrap().contains("underused"));
}

#[test]
fn state_round_trips_through_json() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(0.0, &usage("a", "", claude_headers(0.60, 9000.0, 0.1)), &cfg);
    s.pick(1.0, &input("t", CC, vec![cand("a", "claude", true)]), &cfg);
    let back: State = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
    assert_eq!(back.accounts["a"].windows, s.accounts["a"].windows);
    assert_eq!(back.sessions["t"].auth_id, "a");
}
