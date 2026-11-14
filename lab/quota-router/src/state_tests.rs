use crate::config::Config;
use crate::policy::Reason;
use crate::state::*;
use std::collections::BTreeMap;

const CC: &str = "claude-cli/2.1.321 (external, cli)";
const H: f64 = 3600.0;

fn cand(id: &str, provider: &str, subscription: bool) -> Candidate {
    Candidate {
        id: id.into(),
        provider: provider.into(),
        subscription,
    }
}

fn input(session: &str, ua: &str, candidates: Vec<Candidate>) -> PickInput {
    PickInput {
        model: "claude-opus-5-5".into(),
        session: session.into(),
        parent_session: String::new(),
        user_agent: ua.into(),
        candidates,
    }
}

fn claude_headers(week: f64, week_reset: f64, five: f64) -> BTreeMap<String, String> {
    [
        (
            "anthropic-ratelimit-unified-7d-utilization",
            week.to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-reset",
            week_reset.to_string(),
        ),
        (
            "anthropic-ratelimit-unified-5h-utilization",
            five.to_string(),
        ),
        (
            "anthropic-ratelimit-unified-5h-reset",
            (1000.0 * H).to_string(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn usage(auth: &str, session: &str, headers: BTreeMap<String, String>) -> Usage {
    Usage {
        auth_id: auth.into(),
        provider: "claude".into(),
        session: session.into(),
        headers,
        ..Default::default()
    }
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
    let r = s.pick(
        0.0,
        &input("t", "curl/8.0", vec![cand("sub", "claude", true)]),
        &cfg,
    );
    assert!(matches!(r.outcome, PickOutcome::Reject(_)));
    assert_eq!(r.gated, 1);
    // An API key for the same provider is still usable.
    let r = s.pick(
        0.0,
        &input(
            "t",
            "curl/8.0",
            vec![cand("sub", "claude", true), cand("key", "claude", false)],
        ),
        &cfg,
    );
    assert_eq!(chosen(&r), "key");
    // Claude Code may use the subscription.
    let r = s.pick(
        0.0,
        &input("t2", CC, vec![cand("sub", "claude", true)]),
        &cfg,
    );
    assert_eq!(chosen(&r), "sub");
    // Codex subscriptions are not gated.
    let r = s.pick(
        0.0,
        &input("t3", "curl/8.0", vec![cand("cx", "codex", true)]),
        &cfg,
    );
    assert_eq!(chosen(&r), "cx");
}

#[test]
fn gate_can_be_switched_off() {
    let cfg = Config {
        gate_claude_subs: false,
        ..Config::default()
    };
    let mut s = State::default();
    let r = s.pick(
        0.0,
        &input("t", "curl/8.0", vec![cand("sub", "claude", true)]),
        &cfg,
    );
    assert_eq!(chosen(&r), "sub");
}

#[test]
fn observe_only_mode_and_foreign_providers_defer() {
    let mut s = State::default();
    let off = Config {
        route: false,
        ..Config::default()
    };
    assert!(matches!(
        s.pick(0.0, &input("t", CC, vec![cand("a", "claude", true)]), &off)
            .outcome,
        PickOutcome::Defer
    ));
    let cfg = Config::default();
    assert!(matches!(
        s.pick(0.0, &input("t", CC, vec![cand("x", "xai", true)]), &cfg)
            .outcome,
        PickOutcome::Defer
    ));
    assert_eq!(s.deferred, 2);
}

#[test]
fn observe_only_mode_still_gates() {
    let mut s = State::default();
    let off = Config {
        route: false,
        ..Config::default()
    };
    let r = s.pick(
        0.0,
        &input("t", "curl/8.0", vec![cand("sub", "claude", true)]),
        &off,
    );
    assert!(matches!(r.outcome, PickOutcome::Reject(_)));
}

#[test]
fn thread_keeps_its_account_until_the_ttl_passes() {
    let cfg = Config::default();
    let mut s = State::default();
    let pool = || vec![cand("a", "claude", true), cand("b", "claude", true)];
    // a is the at-risk account, so the thread starts there.
    s.observe(
        0.0,
        &usage("a", "", claude_headers(0.30, 12.0 * H, 0.1)),
        &cfg,
    );
    s.observe(
        0.0,
        &usage("b", "", claude_headers(0.20, 160.0 * H, 0.1)),
        &cfg,
    );
    assert_eq!(chosen(&s.pick(10.0, &input("t", CC, pool()), &cfg)), "a");
    // a is now nearly drained and b would win a fresh pick, but the thread stays.
    s.observe(
        20.0,
        &usage("a", "t", claude_headers(0.93, 12.0 * H, 0.1)),
        &cfg,
    );
    let r = s.pick(30.0, &input("t", CC, pool()), &cfg);
    assert_eq!(chosen(&r), "a");
    assert_eq!(r.decision.unwrap().reason, Reason::Affinity);
    assert_eq!(
        chosen(&s.pick(30.0, &input("fresh", CC, pool()), &cfg)),
        "b"
    );
    // After the idle TTL the cache is cold and policy decides again.
    let later = 30.0 + cfg.affinity_ttl_secs as f64 + 1.0;
    assert_eq!(chosen(&s.pick(later, &input("t", CC, pool()), &cfg)), "b");
    assert_eq!(s.rebinds, 0, "an expired binding is not a rebind");
}

#[test]
fn losing_the_bound_account_is_counted_as_a_rebind() {
    let cfg = Config::default();
    let mut s = State::default();
    assert_eq!(
        chosen(&s.pick(0.0, &input("t", CC, vec![cand("a", "claude", true)]), &cfg)),
        "a"
    );
    assert_eq!(
        chosen(&s.pick(5.0, &input("t", CC, vec![cand("b", "claude", true)]), &cfg)),
        "b"
    );
    assert_eq!(s.rebinds, 1);
}

#[test]
fn subagent_starts_on_the_parent_account() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(
        0.0,
        &usage("a", "", claude_headers(0.93, 100.0 * H, 0.1)),
        &cfg,
    );
    s.observe(
        0.0,
        &usage("b", "", claude_headers(0.10, 100.0 * H, 0.1)),
        &cfg,
    );
    assert_eq!(
        chosen(&s.pick(
            1.0,
            &input("parent", CC, vec![cand("a", "claude", true)]),
            &cfg
        )),
        "a"
    );
    let mut child = input(
        "child",
        CC,
        vec![cand("a", "claude", true), cand("b", "claude", true)],
    );
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
    s.observe(
        60.0,
        &usage("a", "", claude_headers(0.11, reset, 0.1)),
        &cfg,
    );
    assert_eq!(s.accounts["a"].burn_per_hour, 0.0);
    assert_eq!(s.accounts["a"].windows["7d"].used, 0.11);
    // One hour after the baseline: 4% burned.
    s.observe(
        3600.0,
        &usage("a", "", claude_headers(0.14, reset, 0.1)),
        &cfg,
    );
    assert!((s.accounts["a"].burn_per_hour - 0.04).abs() < 1e-9);
}

#[test]
fn unspent_quota_is_recorded_when_the_week_rolls_over() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(
        0.0,
        &usage("a", "", claude_headers(0.60, 1000.0, 0.1)),
        &cfg,
    );
    s.observe(
        2000.0,
        &usage("a", "", claude_headers(0.01, 1000.0 + 7.0 * 86400.0, 0.1)),
        &cfg,
    );
    let wasted = &s.accounts["a"].wasted_at_reset;
    assert_eq!(wasted.len(), 1);
    assert!((wasted[0].1 - 0.40).abs() < 1e-9);
}

#[test]
fn warm_thread_with_no_cache_read_is_a_cache_miss() {
    let cfg = Config::default();
    let mut s = State::default();
    let pool = || vec![cand("a", "claude", true)];
    let big = |read: i64| Usage {
        input_tokens: 9000,
        cache_read_tokens: read,
        ..usage("a", "t", BTreeMap::new())
    };
    s.pick(0.0, &input("t", CC, pool()), &cfg);
    assert!(
        !s.observe(1.0, &big(0), &cfg).cache_miss,
        "first turn has nothing cached"
    );
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
    s.observe(
        0.0,
        &usage("a", "", claude_headers(0.60, 9000.0, 0.1)),
        &cfg,
    );
    assert_eq!(s.apply_announcement("claude", 100.0, 1, true), 1);
    assert_eq!(s.apply_announcement("claude", 100.0, 1, true), 0);
    let a = &s.accounts["a"];
    assert_eq!(a.free_resets, 1);
    assert!(
        a.windows.is_empty(),
        "usage observed before the reset is stale"
    );
    assert_eq!(s.apply_announcement("codex", 100.0, 1, false), 0);
}

#[test]
fn report_flags_underused_accounts() {
    let cfg = Config::default();
    let mut s = State::default();
    s.observe(
        0.0,
        &usage("idle", "", claude_headers(0.05, 30.0 * H, 0.0)),
        &cfg,
    );
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
    s.observe(
        0.0,
        &usage("a", "", claude_headers(0.60, 9000.0, 0.1)),
        &cfg,
    );
    s.pick(1.0, &input("t", CC, vec![cand("a", "claude", true)]), &cfg);
    let back: State = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
    assert_eq!(back.accounts["a"].windows, s.accounts["a"].windows);
    assert_eq!(back.sessions["t"].auth_id, "a");
}

// ------------------------------------------------------- limits and pre-emption

const CODEX_UA: &str = "codex_cli_rs/0.200.0 (Windows 11)";

fn codex_input(session: &str, model: &str, ids: &[&str]) -> PickInput {
    PickInput {
        model: model.into(),
        session: session.into(),
        parent_session: String::new(),
        user_agent: CODEX_UA.into(),
        candidates: ids.iter().map(|id| cand(id, "codex", true)).collect(),
    }
}

fn codex_headers(week: f64, week_reset: f64) -> BTreeMap<String, String> {
    [
        ("x-codex-secondary-used-percent", (week * 100.0).to_string()),
        ("x-codex-secondary-window-minutes", "10080".to_string()),
        ("x-codex-secondary-reset-at", week_reset.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn codex_usage(auth: &str, model: &str, headers: BTreeMap<String, String>) -> Usage {
    Usage {
        auth_id: auth.into(),
        provider: "codex".into(),
        model: model.into(),
        headers,
        ..Default::default()
    }
}

/// An upstream 429 `usage_limit_reached` with the reset in its body.
fn refused(auth: &str, model: &str, resets_at: f64) -> Usage {
    Usage {
        failed: true,
        status: 429,
        failure_body: format!(
            r#"{{"error":{{"type":"usage_limit_reached","message":"You have hit your usage limit.","resets_at":{resets_at}}}}}"#
        ),
        ..codex_usage(auth, model, BTreeMap::new())
    }
}

#[test]
fn near_limit_switch_happens_before_a_429() {
    let cfg = Config::default();
    let mut s = State::default();
    let model = "gpt-5.5-codex";
    let ab = || codex_input("t", model, &["a", "b"]);
    s.observe(
        0.0,
        &codex_usage("a", model, codex_headers(0.30, 100.0 * H)),
        &cfg,
    );
    s.observe(
        0.0,
        &codex_usage("b", model, codex_headers(0.60, 100.0 * H)),
        &cfg,
    );
    // The thread starts on a.
    assert_eq!(chosen(&s.pick(1.0, &ab(), &cfg)), "a");
    // a's responses report 96% of its week: still serving, inside the 5% margin.
    s.observe(
        2.0,
        &codex_usage("a", model, codex_headers(0.96, 100.0 * H)),
        &cfg,
    );
    let r = s.pick(3.0, &ab(), &cfg);
    assert_eq!(
        chosen(&r),
        "b",
        "moved before the limit, not after a refusal"
    );
    let broke = r.decision.unwrap().affinity_broken.unwrap();
    assert_eq!((broke.from.as_str(), broke.reason), ("a", "near_limit"));
    assert_eq!(s.affinity_breaks, 1);
    assert_eq!(s.accounts["a"].failures, 0, "no 429 was needed to move");
    // The thread now lives on b.
    assert_eq!(chosen(&s.pick(4.0, &ab(), &cfg)), "b");
}

#[test]
fn a_refusal_cools_the_account_until_its_reset_and_a_success_clears_it() {
    let cfg = Config::default();
    let mut s = State::default();
    let model = "gpt-5.5-codex";
    let o = s.observe(100.0, &refused("a", model, 5000.0), &cfg);
    let (scope, c) = o.cooldown.unwrap();
    assert_eq!((scope.as_str(), c.until, c.estimated), ("", 5000.0, false));
    // Bound or not, a is skipped while it cools.
    let r = s.pick(200.0, &codex_input("t", model, &["a", "b"]), &cfg);
    assert_eq!(chosen(&r), "b");
    // After the reset a is offered again, and a served request clears the record.
    let r = s.pick(5001.0, &codex_input("n", model, &["a"]), &cfg);
    assert_eq!(chosen(&r), "a");
    s.observe(5002.0, &codex_usage("a", model, BTreeMap::new()), &cfg);
    assert!(s.accounts["a"].cooldowns.is_empty());
}

#[test]
fn cooldown_is_respected_across_a_simulated_restart() {
    let cfg = Config::default();
    let mut s = State::default();
    let model = "gpt-5.5-codex";
    s.pick(10.0, &codex_input("t", model, &["a"]), &cfg);
    s.observe(20.0, &refused("a", model, 9000.0), &cfg);
    // What the plugin writes to state.json, read back by a fresh process.
    let raw = serde_json::to_vec(&s).unwrap();
    let mut back: State = serde_json::from_slice(&raw).unwrap();
    let r = back.pick(30.0, &codex_input("t", model, &["a"]), &cfg);
    match r.outcome {
        PickOutcome::Exhausted(ex) => assert_eq!(ex.earliest_reset, 9000.0),
        other => panic!("a restarted router must still refuse, got {other:?}"),
    }
    assert!(back.preflight(31.0, model, CODEX_UA, &cfg).is_some());
}

#[test]
fn all_exhausted_returns_the_final_error_with_the_earliest_reset() {
    let cfg = Config::default();
    let mut s = State::default();
    let model = "gpt-5.5-codex";
    let now = 1_000.0;
    s.pick(now, &codex_input("t", model, &["a", "b"]), &cfg);
    s.pick(now, &codex_input("o", "gpt-5.5-mini", &["c"]), &cfg);
    s.observe(now, &refused("a", model, now + 7200.0), &cfg);
    s.observe(now, &refused("b", model, now + 3600.0), &cfg);
    // Interceptor path: known pool, everything cooling.
    let (ex, first) = s.preflight(now + 1.0, model, CODEX_UA, &cfg).unwrap();
    assert!(first);
    assert_eq!(ex.earliest_reset, now + 3600.0);
    assert!(!ex.reset_estimated);
    assert_eq!((ex.accounts, ex.in_cooldown, ex.at_limit), (2, 2, 0));
    assert_eq!(
        ex.alternatives,
        vec![crate::limits::Alternative {
            model: "gpt-5.5-mini".into(),
            provider: "codex".into()
        }]
    );
    let (status, headers, body) = crate::limits::final_error(&ex, now + 1.0, false);
    assert_eq!(status, 429);
    assert_eq!(headers["Retry-After"], vec!["3599".to_string()]);
    assert_eq!(headers["X-Should-Retry"], vec!["false".to_string()]);
    assert_eq!(body["error"]["type"], "usage_limit_reached");
    assert_eq!(body["error"]["code"], "quota_router_exhausted");
    assert_eq!(body["error"]["resets_at"], (now + 3600.0) as i64);
    assert_eq!(body["error"]["model"], model);
    // Claude clients get Claude's error type and the unified rejection headers.
    let (_, headers, body) = crate::limits::final_error(&ex, now + 1.0, true);
    assert_eq!(body["error"]["type"], "rate_limit_error");
    let reset = ((now + 3600.0) as i64).to_string();
    assert_eq!(headers["Anthropic-Ratelimit-Unified-Reset"], vec![reset]);
    // Pick path (the host asked anyway): refused, nothing chosen.
    let r = s.pick(now + 2.0, &codex_input("t", model, &["a", "b"]), &cfg);
    assert!(matches!(r.outcome, PickOutcome::Exhausted(_)));
    assert_eq!(r.transition, None, "already logged as exhausted once");
    // After b's reset it serves again, and the transition is reported once.
    let r = s.pick(now + 3601.0, &codex_input("t", model, &["a", "b"]), &cfg);
    assert_eq!(chosen(&r), "b");
    assert_eq!(r.transition, Some("recovered"));
}

#[test]
fn preflight_passes_unknown_models_and_lets_a_probe_through() {
    let cfg = Config::default();
    let mut s = State::default();
    let model = "gpt-5.5-codex";
    assert!(
        s.preflight(0.0, model, CODEX_UA, &cfg).is_none(),
        "no pool yet"
    );
    s.pick(0.0, &codex_input("t", model, &["a"]), &cfg);
    s.observe(0.0, &refused("a", model, 100_000.0), &cfg);
    assert!(s.preflight(1.0, model, CODEX_UA, &cfg).is_some());
    let probe = cfg.probe_after_secs as f64;
    assert!(
        s.preflight(probe, model, CODEX_UA, &cfg).is_none(),
        "periodic probe"
    );
    // An account never seen before, offered for another model of the same
    // provider, sends the next request for the exhausted model to the host.
    s.pick(2.0, &codex_input("t2", model, &["a"]), &cfg);
    assert!(s.preflight(3.0, model, CODEX_UA, &cfg).is_some());
    s.pick(4.0, &codex_input("x", "gpt-5.5-mini", &["new"]), &cfg);
    assert!(s.preflight(5.0, model, CODEX_UA, &cfg).is_none());
}

#[test]
fn final_error_respects_the_claude_client_gate() {
    let cfg = Config::default();
    let mut s = State::default();
    let model = "claude-opus-5-5";
    s.pick(
        0.0,
        &input("t", CC, vec![cand("sub", "claude", true)]),
        &cfg,
    );
    let refusal = Usage {
        failed: true,
        status: 429,
        ..usage("sub", "", BTreeMap::new())
    };
    s.observe(0.0, &refusal, &cfg);
    assert!(s.preflight(1.0, model, CC, &cfg).is_some());
    // A client that may not use the subscription is left to the gate's own answer.
    assert!(s.preflight(1.0, model, "curl/8.0", &cfg).is_none());
}

#[test]
fn status_output_shape_is_stable() {
    let cfg = Config::default();
    let mut s = State::default();
    let now = 1_794_355_200.0; // 2026-11-11T00:00:00Z
    let model = "gpt-5.5-codex";
    let (a, b) = ("acct-file-a.json", "acct-file-b.json");
    s.pick(now, &codex_input("t", model, &[a, b]), &cfg);
    let near = codex_headers(0.96, now + 86_400.0);
    s.observe(now, &codex_usage(a, model, near), &cfg);
    s.observe(now, &refused(b, model, now + 3600.0), &cfg);
    s.saved_at = now - 10.0;
    let got = s.status(now + 60.0, &cfg);
    let text = serde_json::to_string(&got).unwrap();
    assert!(
        !text.contains("acct-file"),
        "no account identifier may appear: {text}"
    );
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("testdata/status_golden.json")).unwrap();
    assert_eq!(
        got,
        golden,
        "status changed shape; update testdata/status_golden.json (and STATUS_SCHEMA_VERSION if a field moved):\n{}",
        serde_json::to_string_pretty(&got).unwrap()
    );
}
