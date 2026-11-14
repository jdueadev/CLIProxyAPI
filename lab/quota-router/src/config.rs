//! Plugin configuration, read from `plugins.configs.quota-router` in the host config.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Master switch for routing. When false the plugin only observes and logs;
    /// every pick is left to the host's built-in scheduler.
    pub route: bool,
    /// Directory for state and event logs. Relative paths resolve against the
    /// host's working directory.
    pub data_dir: String,
    /// Fraction of a budget window that should be spent by its reset (rule 1).
    pub drain_target: f64,
    /// No request, bound or new, is sent to an account whose applicable window
    /// is at or above this fraction: it counts as at its limit.
    pub block_at: f64,
    /// Pre-emption margin. An account at or above this fraction of any applicable
    /// window is `near_limit`: it ranks behind every account with room, and a
    /// thread bound to it moves to an account with room before the limit is hit.
    pub preempt_at: f64,
    /// Cooldown applied after an upstream 429 that names no reset time.
    pub cooldown_default_secs: u64,
    /// While every account for a model is unavailable the router answers the
    /// final error itself; once this long has passed since it last asked the
    /// host for candidates, one request goes through to refresh the pool.
    pub probe_after_secs: u64,
    /// Answer "nothing can serve" with one final 429 before the host tries any
    /// account. When false, the host's own errors reach the client.
    pub final_error: bool,
    /// Short (rate) windows at or above this fraction push the account behind
    /// every account that still has room.
    pub rate_soft: f64,
    /// Windows at least this long are budgets to drain; shorter ones are rate limits.
    pub budget_min_hours: f64,
    /// Burn rate (fraction of a budget window per hour) assumed for an account
    /// with no measured history.
    pub assumed_burn_per_hour: f64,
    /// An account is at risk of wasting quota when the burn it needs reaches
    /// this share of the burn it can sustain.
    pub risk_factor: f64,
    /// Idle time after which a session binding is dropped. Provider prompt
    /// caches are cold by then, so re-routing costs nothing.
    pub affinity_ttl_secs: u64,
    /// Keep Claude subscription credentials for the clients listed below.
    pub gate_claude_subs: bool,
    /// User-Agent prefixes allowed to use Claude subscription credentials.
    pub claude_clients: Vec<String>,
    /// Providers this plugin schedules. Others are left to the host.
    pub providers: Vec<String>,
    /// A follow-up request inside the cache TTL with at least this many input
    /// tokens and no cache read is recorded as a cache miss.
    pub cache_miss_min_tokens: i64,
    /// Seconds a provider prompt cache is assumed to stay warm.
    pub cache_ttl_secs: u64,
    /// Write one JSON line per pick and per completed request.
    pub log_events: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            route: true,
            data_dir: "quota-router".into(),
            drain_target: 0.90,
            block_at: 0.98,
            preempt_at: 0.95,
            cooldown_default_secs: 300,
            probe_after_secs: 600,
            final_error: true,
            rate_soft: 0.90,
            budget_min_hours: 24.0,
            assumed_burn_per_hour: 0.02,
            risk_factor: 0.5,
            affinity_ttl_secs: 3600,
            gate_claude_subs: true,
            claude_clients: vec!["claude-cli/".into()],
            providers: vec!["claude".into(), "codex".into()],
            cache_miss_min_tokens: 4096,
            cache_ttl_secs: 300,
            log_events: true,
        }
    }
}

impl Config {
    pub fn from_yaml(raw: &[u8]) -> Result<Self, String> {
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Self::default());
        }
        let cfg: Config = serde_yaml::from_slice(raw).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        let unit = |name: &str, v: f64| {
            if (0.0..=1.0).contains(&v) {
                Ok(())
            } else {
                Err(format!("{name} must be between 0 and 1, got {v}"))
            }
        };
        unit("drain_target", self.drain_target)?;
        unit("block_at", self.block_at)?;
        unit("rate_soft", self.rate_soft)?;
        unit("preempt_at", self.preempt_at)?;
        if self.preempt_at > self.block_at {
            return Err(format!(
                "preempt_at ({}) must not exceed block_at ({}): the router moves threads away before the hard stop",
                self.preempt_at, self.block_at
            ));
        }
        if self.assumed_burn_per_hour <= 0.0 {
            return Err("assumed_burn_per_hour must be positive".into());
        }
        if self.risk_factor <= 0.0 {
            return Err("risk_factor must be positive".into());
        }
        Ok(())
    }

    pub fn schedules(&self, provider: &str) -> bool {
        self.providers
            .iter()
            .any(|p| p.eq_ignore_ascii_case(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yaml_is_default() {
        let cfg = Config::from_yaml(b"\n").unwrap();
        assert!(cfg.route);
        assert_eq!(cfg.drain_target, 0.90);
    }

    #[test]
    fn host_keys_are_ignored_and_overrides_apply() {
        let cfg =
            Config::from_yaml(b"enabled: true\npriority: 5\nroute: false\ndrain_target: 0.8\n")
                .unwrap();
        assert!(!cfg.route);
        assert_eq!(cfg.drain_target, 0.8);
    }

    #[test]
    fn out_of_range_is_rejected() {
        assert!(Config::from_yaml(b"drain_target: 90\n").is_err());
    }
}
