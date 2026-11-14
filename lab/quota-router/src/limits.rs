//! Usage-limit handling: cooldowns learned from upstream refusals, the single
//! final error returned when nothing can serve, and the stable account labels
//! that appear in anything a client may read.
//!
//! Why a 429 with `x-should-retry: false` (checked against client source, 2026-11-14):
//!
//! - Codex CLI (`openai/codex`, `codex-rs/model-provider-info/src/lib.rs`): the HTTP
//!   retry policy has `retry_429: false, retry_5xx: true`, and the retry loop sleeps
//!   until any `Retry-After` deadline (`codex-rs/codex-client/src/retry.rs`). A 503
//!   with a long `Retry-After` is therefore the silent hang. A 429 whose body has
//!   `error.type == "usage_limit_reached"` maps to `UsageLimitReached`
//!   (`codex-rs/codex-api/src/api_bridge.rs`), whose retry policy is `Never`
//!   (`codex-rs/protocol/src/error.rs`); `error.resets_at` (unix seconds) is shown
//!   to the user. Any other 429 maps to `RetryLimit`, retried at the server's
//!   `Retry-After`.
//! - Anthropic TypeScript SDK (`anthropics/anthropic-sdk-typescript`, `src/client.ts`
//!   `shouldRetry`): `x-should-retry: false` is obeyed before the status check, so
//!   the 429 is not retried; without it a 429 is retried after `Retry-After`.
//!   Claude Code is closed source: that it uses this retry rule, and that it reads
//!   the `anthropic-ratelimit-unified-*` headers to show its limit message, is an
//!   estimate from its use of the SDK, not confirmed from its source.

use crate::quota::{self, Window};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Body and error codes of the final answer. Bump `ERROR_SCHEMA_VERSION` on any
/// breaking change to the body.
pub const ERROR_SCHEMA_VERSION: u32 = 1;
pub const ERROR_CODE: &str = "quota_router_exhausted";
/// Longest cooldown accepted from an upstream: anything beyond is a bad value.
const MAX_COOLDOWN_SECS: f64 = 31.0 * 86_400.0;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cooldown {
    /// Unix seconds until which the account is not offered.
    pub until: f64,
    /// Unix seconds when the refusal was seen.
    pub set_at: f64,
    /// Where `until` came from: `body.resets_at`, `body.resets_in_seconds`,
    /// `header.unified_reset`, `header.window_reset`, `header.retry_after`, or
    /// `default` (no reset was reported; `estimated` is then true).
    pub source: String,
    pub estimated: bool,
}

impl Cooldown {
    pub fn active(&self, now: f64) -> bool {
        self.until > now
    }
}

fn number(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The cooldown an upstream refusal implies, and the model scope it covers
/// (empty for every model). `None` when the failure is not a usage limit.
pub fn cooldown_from_failure(
    status: i64,
    headers: &BTreeMap<String, String>,
    body: &str,
    now: f64,
    default_secs: u64,
) -> Option<(String, Cooldown)> {
    if status != 429 {
        return None;
    }
    let parsed: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    let field = |name: &str| {
        [&parsed["error"][name], &parsed[name]]
            .into_iter()
            .find_map(Value::as_f64)
    };
    let header = |name: &str| headers.get(name).and_then(|v| number(v));
    let windows = quota::parse_windows(headers, now);
    let exhausted: Vec<(&String, &Window)> = windows
        .iter()
        .filter(|(_, w)| w.used >= 1.0 && w.reset_at > now)
        .collect();
    let unified_rejected = headers
        .get("anthropic-ratelimit-unified-status")
        .is_none_or(|s| s.trim().eq_ignore_ascii_case("rejected"));
    let candidates = [
        ("body.resets_at", field("resets_at")),
        (
            "body.resets_in_seconds",
            field("resets_in_seconds").map(|s| now + s),
        ),
        (
            "header.unified_reset",
            header("anthropic-ratelimit-unified-reset").filter(|_| unified_rejected),
        ),
        (
            "header.window_reset",
            exhausted.iter().map(|(_, w)| w.reset_at).reduce(f64::max),
        ),
        ("header.retry_after", header("retry-after").map(|s| now + s)),
    ];
    let found = candidates.into_iter().find_map(|(source, at)| {
        at.filter(|t| *t > now && *t - now <= MAX_COOLDOWN_SECS)
            .map(|t| (source, t))
    });
    let (source, until, estimated) = match found {
        Some((s, t)) => (s, t, false),
        None => ("default", now + default_secs as f64, true),
    };
    // A refusal caused only by model-scoped windows (`7d_opus`) leaves other models open.
    let scopes: Vec<&str> = exhausted
        .iter()
        .map(|(n, _)| quota::model_scope(n))
        .collect();
    let scope = if !scopes.is_empty() && scopes.iter().all(|s| !s.is_empty() && *s == scopes[0]) {
        scopes[0].to_string()
    } else {
        String::new()
    };
    Some((
        scope,
        Cooldown {
            until,
            set_at: now,
            source: source.to_string(),
            estimated,
        },
    ))
}

/// One model that can serve right now, offered in the final error.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alternative {
    pub model: String,
    pub provider: String,
}

/// Everything the final error says. Built by `State::preflight` and `State::pick`.
#[derive(Debug, Clone, PartialEq)]
pub struct Exhausted {
    pub model: String,
    pub providers: Vec<String>,
    /// Earliest unix second at which an account becomes usable again. 0 when no
    /// account reported one.
    pub earliest_reset: f64,
    /// True when `earliest_reset` rests on an assumed window length or the
    /// default cooldown rather than a reset time the upstream sent.
    pub reset_estimated: bool,
    /// Unix second at which the router will ask the host for candidates again.
    pub recheck_at: f64,
    pub accounts: usize,
    pub in_cooldown: usize,
    pub at_limit: usize,
    pub alternatives: Vec<Alternative>,
}

impl Exhausted {
    /// Seconds a client should wait before trying this model again.
    pub fn retry_after(&self, now: f64) -> u64 {
        let at = if self.earliest_reset > now {
            self.earliest_reset
        } else {
            self.recheck_at
        };
        (at - now).ceil().max(1.0) as u64
    }

    pub fn message(&self, now: f64) -> String {
        let when = if self.earliest_reset > now {
            format!(
                "earliest reset {} (in {}s{})",
                iso8601(self.earliest_reset),
                (self.earliest_reset - now).ceil() as u64,
                if self.reset_estimated {
                    ", estimate"
                } else {
                    ""
                }
            )
        } else {
            "no account reported a reset time".to_string()
        };
        let alternatives = if self.alternatives.is_empty() {
            "none".to_string()
        } else {
            self.alternatives
                .iter()
                .map(|a| a.model.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "No account can serve model {}: {} of {} in cooldown, {} at their limit. {}. Available now: {}. The router did not send this request upstream; retrying before the reset is refused the same way.",
            self.model, self.in_cooldown, self.accounts, self.at_limit, when, alternatives
        )
    }
}

/// The one final answer: status, headers and JSON body. `anthropic` picks the
/// error type Claude clients expect; other clients get Codex's terminal type.
pub fn final_error(
    ex: &Exhausted,
    now: f64,
    anthropic: bool,
) -> (u16, BTreeMap<String, Vec<String>>, Value) {
    let retry_after = ex.retry_after(now);
    let reset = (ex.earliest_reset > now).then_some(ex.earliest_reset);
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut set = |k: &str, v: String| {
        headers.insert(k.to_string(), vec![v]);
    };
    set("Content-Type", "application/json".into());
    set("Retry-After", retry_after.to_string());
    set("X-Should-Retry", "false".into());
    set("X-Quota-Router", "exhausted".into());
    if anthropic {
        set("Anthropic-Ratelimit-Unified-Status", "rejected".into());
        if let Some(r) = reset {
            set(
                "Anthropic-Ratelimit-Unified-Reset",
                format!("{}", r.ceil() as i64),
            );
        }
    }
    let body = json!({
        "type": "error",
        "error": {
            "type": if anthropic { "rate_limit_error" } else { "usage_limit_reached" },
            "code": ERROR_CODE,
            "message": ex.message(now),
            "schema_version": ERROR_SCHEMA_VERSION,
            "retryable": false,
            "model": ex.model,
            "providers": ex.providers,
            "resets_at": reset.map(|r| r.ceil() as i64),
            "reset_at": reset.map(iso8601),
            "resets_in_seconds": reset.map(|r| (r - now).ceil() as i64),
            "reset_estimated": ex.reset_estimated,
            "retry_after_seconds": retry_after,
            "accounts": {
                "total": ex.accounts,
                "in_cooldown": ex.in_cooldown,
                "at_limit": ex.at_limit,
            },
            "alternatives": ex.alternatives,
        }
    });
    (429, headers, body)
}

/// Stable, non-reversible label for an account: keyed SipHash of its id. The key
/// lives in the private state file, so a label cannot be traced back to an
/// account without it.
#[allow(deprecated)]
pub fn label(key: [u64; 2], auth_id: &str) -> String {
    use std::hash::{Hasher, SipHasher};
    let mut h = SipHasher::new_with_keys(key[0], key[1]);
    h.write(auth_id.as_bytes());
    format!("acct-{:012x}", h.finish() >> 16)
}

/// A fresh random label key.
pub fn new_label_key() -> [u64; 2] {
    use std::hash::{BuildHasher, Hasher};
    let a = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(a);
    [a, h.finish()]
}

/// Civil date and time (UTC) of a unix second, RFC 3339 with a `Z`.
pub fn iso8601(secs: f64) -> String {
    let total = secs.floor() as i64;
    let days = total.div_euclid(86_400);
    let tod = total.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod / 60) % 60,
        tod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn codex_usage_limit_body_sets_the_reset() {
        let body = r#"{"error":{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":3600}}"#;
        let (scope, c) = cooldown_from_failure(429, &BTreeMap::new(), body, 1000.0, 300).unwrap();
        assert_eq!(scope, "");
        assert_eq!(c.until, 4600.0);
        assert_eq!(c.source, "body.resets_in_seconds");
        assert!(!c.estimated);
        let body = r#"{"error":{"type":"usage_limit_reached","resets_at":9000}}"#;
        assert_eq!(
            cooldown_from_failure(429, &BTreeMap::new(), body, 1000.0, 300)
                .unwrap()
                .1
                .until,
            9000.0
        );
    }

    #[test]
    fn claude_rejection_uses_the_unified_reset() {
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "5000"),
            ("retry-after", "30"),
        ]);
        let (_, c) = cooldown_from_failure(429, &headers, "", 1000.0, 300).unwrap();
        assert_eq!(
            (c.until, c.source.as_str()),
            (5000.0, "header.unified_reset")
        );
    }

    #[test]
    fn exhausted_scoped_window_scopes_the_cooldown() {
        let headers = h(&[
            ("anthropic-ratelimit-unified-7d_opus-utilization", "1.0"),
            ("anthropic-ratelimit-unified-7d_opus-reset", "8000"),
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
        ]);
        let (scope, c) = cooldown_from_failure(429, &headers, "", 1000.0, 300).unwrap();
        assert_eq!(scope, "opus");
        assert_eq!(
            (c.until, c.source.as_str()),
            (8000.0, "header.window_reset")
        );
    }

    #[test]
    fn no_reset_reported_is_a_marked_estimate() {
        let (_, c) = cooldown_from_failure(429, &BTreeMap::new(), "busy", 1000.0, 300).unwrap();
        assert_eq!(c.until, 1300.0);
        assert!(c.estimated);
        assert_eq!(c.source, "default");
        assert!(cooldown_from_failure(500, &BTreeMap::new(), "", 1000.0, 300).is_none());
    }

    #[test]
    fn labels_are_stable_and_keyed() {
        let a = label([1, 2], "claude-someone@example.com.json");
        assert_eq!(a, label([1, 2], "claude-someone@example.com.json"));
        assert_ne!(a, label([3, 4], "claude-someone@example.com.json"));
        assert!(a.starts_with("acct-") && a.len() == 17);
        assert!(!a.contains("someone"));
    }

    #[test]
    fn iso_times() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_794_355_200.0 + 3_723.0), "2026-11-11T01:02:03Z");
    }
}
