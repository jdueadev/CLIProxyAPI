//! Turns upstream rate-limit response headers into quota windows.
//!
//! Header families (the same ones the host keeps in `quota_signals.go`):
//!   Claude: `anthropic-ratelimit-unified-<window>-utilization` and `...-<window>-reset`
//!   Codex:  `x-codex-<name>-used-percent`, `-window-minutes`, `-reset-at`,
//!           `-reset-after-seconds`
//! Parsing is by suffix, so a new window name needs no code change.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Fraction used, 0.0 to 1.0.
    pub used: f64,
    /// Unix seconds when the window resets. 0 when the upstream did not say.
    pub reset_at: f64,
    /// Window length in hours. 0 when unknown.
    pub hours: f64,
    /// Unix seconds of the response that carried these values.
    pub observed_at: f64,
}

impl Window {
    /// Fraction used right now: a window whose reset has passed is empty.
    pub fn used_now(&self, now: f64) -> f64 {
        if self.reset_at > 0.0 && now >= self.reset_at {
            0.0
        } else {
            self.used.clamp(0.0, 1.0)
        }
    }

    /// Hours until reset. After a reset, or when the reset is unknown, the
    /// whole window length (or `fallback_hours`) is still ahead.
    pub fn hours_left(&self, now: f64, fallback_hours: f64) -> f64 {
        if self.reset_at > 0.0 && now < self.reset_at {
            (self.reset_at - now) / 3600.0
        } else if self.hours > 0.0 {
            self.hours
        } else {
            fallback_hours
        }
    }
}

const CLAUDE_PREFIX: &str = "anthropic-ratelimit-unified-";
const CODEX_PREFIX: &str = "x-codex-";

/// Length in hours encoded in names such as `5h`, `7d`, `7d_sonnet`.
pub fn hours_from_name(name: &str) -> f64 {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    let Ok(n) = digits.parse::<f64>() else {
        return 0.0;
    };
    match name[digits.len()..].chars().next() {
        Some('h') => n,
        Some('d') => n * 24.0,
        Some('w') => n * 168.0,
        Some('m') => n / 60.0,
        _ => 0.0,
    }
}

/// Model family a window is scoped to: `7d_sonnet` -> `sonnet`. Empty when the
/// window covers every model.
pub fn model_scope(name: &str) -> &str {
    name.split_once('_').map(|(_, scope)| scope).unwrap_or("")
}

pub fn applies_to_model(name: &str, model: &str) -> bool {
    let scope = model_scope(name);
    scope.is_empty()
        || model
            .to_ascii_lowercase()
            .contains(&scope.to_ascii_lowercase())
}

fn fraction(raw: &str, percent: bool) -> Option<f64> {
    let v: f64 = raw.trim().trim_end_matches('%').parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    // Claude reports a 0-1 fraction; a value above 1.5 can only be a percentage.
    let v = if percent || v > 1.5 { v / 100.0 } else { v };
    Some(v.min(1.0))
}

/// Parses every quota window found in `headers` (lower-cased names).
pub fn parse_windows(headers: &BTreeMap<String, String>, now: f64) -> BTreeMap<String, Window> {
    fn slot<'a>(out: &'a mut BTreeMap<String, Window>, name: &str, now: f64) -> &'a mut Window {
        out.entry(name.to_string()).or_insert(Window {
            used: f64::NAN,
            reset_at: 0.0,
            hours: 0.0,
            observed_at: now,
        })
    }
    let number = |raw: &str| raw.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    let mut out: BTreeMap<String, Window> = BTreeMap::new();
    for (key, value) in headers {
        if let Some(rest) = key.strip_prefix(CLAUDE_PREFIX) {
            if let Some(name) = rest.strip_suffix("-utilization") {
                if let Some(v) = fraction(value, false) {
                    slot(&mut out, name, now).used = v;
                }
            } else if let Some(name) = rest.strip_suffix("-reset") {
                // `anthropic-ratelimit-unified-reset` has no window name and never reaches here.
                if let Some(v) = number(value) {
                    slot(&mut out, name, now).reset_at = v;
                }
            }
        } else if let Some(rest) = key.strip_prefix(CODEX_PREFIX) {
            if let Some(name) = rest.strip_suffix("-used-percent") {
                if let Some(v) = fraction(value, true) {
                    slot(&mut out, name, now).used = v;
                }
            } else if let Some(name) = rest.strip_suffix("-window-minutes") {
                if let Some(v) = number(value) {
                    slot(&mut out, name, now).hours = v / 60.0;
                }
            } else if let Some(name) = rest.strip_suffix("-reset-at") {
                if let Some(v) = number(value) {
                    slot(&mut out, name, now).reset_at = v;
                }
            } else if let Some(name) = rest.strip_suffix("-reset-after-seconds") {
                if let Some(v) = number(value) {
                    let w = slot(&mut out, name, now);
                    if w.reset_at == 0.0 {
                        w.reset_at = now + v;
                    }
                }
            }
        }
    }
    out.retain(|_, w| w.used.is_finite());
    for (name, w) in out.iter_mut() {
        if w.hours == 0.0 {
            w.hours = hours_from_name(name);
        }
    }
    out
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
    fn claude_windows() {
        let w = parse_windows(
            &h(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
                ("anthropic-ratelimit-unified-5h-reset", "2000"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.79"),
                ("anthropic-ratelimit-unified-7d-reset", "9000"),
                ("anthropic-ratelimit-unified-status", "allowed"),
                ("anthropic-ratelimit-unified-reset", "2000"),
            ]),
            1000.0,
        );
        assert_eq!(w.len(), 2);
        assert_eq!(
            w["5h"],
            Window {
                used: 0.42,
                reset_at: 2000.0,
                hours: 5.0,
                observed_at: 1000.0
            }
        );
        assert_eq!(w["7d"].hours, 168.0);
        assert_eq!(w["7d"].used, 0.79);
    }

    #[test]
    fn codex_windows() {
        let w = parse_windows(
            &h(&[
                ("x-codex-primary-used-percent", "17"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-after-seconds", "600"),
                ("x-codex-secondary-used-percent", "55.5"),
                ("x-codex-secondary-window-minutes", "10080"),
                ("x-codex-secondary-reset-at", "777"),
                ("x-codex-plan-type", "pro"),
            ]),
            100.0,
        );
        assert_eq!(
            w["primary"],
            Window {
                used: 0.17,
                reset_at: 700.0,
                hours: 5.0,
                observed_at: 100.0
            }
        );
        assert_eq!(w["secondary"].hours, 168.0);
        assert_eq!(w["secondary"].reset_at, 777.0);
        assert!((w["secondary"].used - 0.555).abs() < 1e-9);
    }

    #[test]
    fn window_without_utilization_is_dropped() {
        let w = parse_windows(&h(&[("anthropic-ratelimit-unified-5h-reset", "2000")]), 0.0);
        assert!(w.is_empty());
    }

    #[test]
    fn percentage_sent_as_claude_fraction_is_normalised() {
        let w = parse_windows(
            &h(&[("anthropic-ratelimit-unified-7d-utilization", "58")]),
            0.0,
        );
        assert_eq!(w["7d"].used, 0.58);
    }

    #[test]
    fn reset_passed_means_empty() {
        let w = Window {
            used: 0.9,
            reset_at: 100.0,
            hours: 5.0,
            observed_at: 0.0,
        };
        assert_eq!(w.used_now(50.0), 0.9);
        assert_eq!(w.used_now(100.0), 0.0);
        assert_eq!(w.hours_left(50.0, 1.0), 50.0 / 3600.0);
        assert_eq!(w.hours_left(200.0, 1.0), 5.0);
    }

    #[test]
    fn model_scoped_windows() {
        assert_eq!(hours_from_name("7d_sonnet"), 168.0);
        assert!(applies_to_model("7d_sonnet", "claude-sonnet-5-5"));
        assert!(!applies_to_model("7d_sonnet", "claude-opus-5-5"));
        assert!(applies_to_model("7d", "anything"));
    }
}
