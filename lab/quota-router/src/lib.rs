//! quota-router: a CLIProxyAPI plugin (C ABI, JSON methods).
//!
//! Capabilities: `scheduler` (which account serves a request), `usage_plugin`
//! (quota, cache and spend monitoring), `request_interceptor` (the one final
//! error when no account can serve, before the host tries any) and
//! `management_api` (state, report and status).
//! Policy lives in `policy`; this file is the ABI shell, the event log and
//! persistence.

pub mod config;
pub mod limits;
pub mod policy;
pub mod quota;
pub mod state;
#[cfg(test)]
mod state_tests;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use config::Config;
use serde_json::{json, Map, Value};
use state::{Candidate, PickInput, PickOutcome, State, Usage};
use std::collections::BTreeMap;
use std::ffi::{c_char, c_void, CStr};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const ABI_VERSION: u32 = 1;
// 7: request interceptors may decline the request body (we never read it).
const SCHEMA_VERSION: u32 = 7;
const SAVE_EVERY_SECS: f64 = 30.0;
const MAX_SELECTIONS: usize = 8192;

struct Runtime {
    cfg: Config,
    state: State,
    loaded_from: Option<PathBuf>,
    last_save: f64,
    /// selection id -> (session key used at pick time, unix seconds).
    selections: BTreeMap<String, (String, f64)>,
}

static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<Runtime>> {
    RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------- persistence

fn state_path(cfg: &Config) -> PathBuf {
    Path::new(&cfg.data_dir).join("state.json")
}

fn load_state(cfg: &Config) -> State {
    fs::read(state_path(cfg))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn save_state(rt: &mut Runtime, at: f64) {
    rt.state.saved_at = at;
    rt.state.settings = Some(rt.cfg.clone());
    let path = state_path(&rt.cfg);
    let Ok(raw) = serde_json::to_vec(&rt.state) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if fs::create_dir_all(&rt.cfg.data_dir).is_ok() && fs::write(&tmp, raw).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
    rt.last_save = at;
}

/// Day number since the Unix epoch, and its civil date (UTC).
fn utc_date(secs: f64) -> String {
    let days = (secs / 86_400.0).floor() as i64;
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
    format!("{y:04}-{m:02}-{d:02}")
}

fn log_event(cfg: &Config, at: f64, mut event: Value) {
    if !cfg.log_events {
        return;
    }
    if let Some(obj) = event.as_object_mut() {
        obj.insert("v".into(), json!(1));
        obj.insert("ts".into(), json!((at * 1000.0).round() / 1000.0));
    }
    let dir = Path::new(&cfg.data_dir).join("events");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{}.jsonl", utc_date(at)));
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{event}");
    }
}

/// Cooldowns logged after the state file was last written (a crash between
/// saves) are put back from the event log, so a restart never forgets a refusal.
fn replay_cooldowns(cfg: &Config, state: &mut State, at: f64) -> usize {
    let since = state.saved_at;
    let dir = Path::new(&cfg.data_dir).join("events");
    let mut restored = 0;
    let mut day = if since > 0.0 { since } else { at - 86_400.0 };
    let mut days = Vec::new();
    while day <= at + 86_400.0 && days.len() < 40 {
        days.push(utc_date(day));
        day += 86_400.0;
    }
    days.dedup();
    for d in days {
        let Ok(raw) = fs::read_to_string(dir.join(format!("{d}.jsonl"))) else {
            continue;
        };
        for line in raw.lines().filter(|l| l.contains("\"cooldown\"")) {
            let Ok(e) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if e["t"] != "cooldown" || e["ts"].as_f64().unwrap_or(0.0) <= since {
                continue;
            }
            let c = limits::Cooldown {
                until: e["until"].as_f64().unwrap_or(0.0),
                set_at: e["ts"].as_f64().unwrap_or(0.0),
                source: e["source"].as_str().unwrap_or("").to_string(),
                estimated: e["estimated"].as_bool().unwrap_or(false),
            };
            let id = e["auth_id"].as_str().unwrap_or("");
            if state.restore_cooldown(at, id, e["scope"].as_str().unwrap_or(""), c) {
                restored += 1;
            }
        }
    }
    restored
}

/// Free-reset announcements written by the watcher: `announcements.json` in the
/// data directory, a list of `{provider, at, grant, clears_usage}`.
fn apply_announcements(rt: &mut Runtime, at: f64) {
    let path = Path::new(&rt.cfg.data_dir).join("announcements.json");
    let Ok(raw) = fs::read(&path) else { return };
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&raw) else {
        return;
    };
    for item in items {
        let provider = item["provider"].as_str().unwrap_or("").to_string();
        let when = item["at"].as_f64().unwrap_or(0.0);
        if provider.is_empty() || when <= 0.0 || when > at {
            continue;
        }
        let grant = item["grant"].as_u64().unwrap_or(1) as u32;
        let clears = item["clears_usage"].as_bool().unwrap_or(false);
        let n = rt.state.apply_announcement(&provider, when, grant, clears);
        if n > 0 {
            log_event(
                &rt.cfg,
                at,
                json!({"t": "announcement", "provider": provider, "at": when, "grant": grant, "clears_usage": clears, "accounts": n}),
            );
        }
    }
}

// ------------------------------------------------------------------ handlers

fn registration() -> Value {
    let field = |name: &str, ty: &str, default: Value, desc: &str| json!({"Name": name, "Type": ty, "Default": default, "Description": desc});
    let d = Config::default();
    json!({
        "schema_version": SCHEMA_VERSION,
        "metadata": {
            "Name": "quota-router",
            "Version": env!("CARGO_PKG_VERSION"),
            "Author": "jdueadev",
            "GitHubRepository": "https://github.com/jdueadev/CLIProxyAPI",
            "Logo": "",
            "ConfigFields": [
                field("route", "boolean", json!(d.route), "Route by quota. When false the plugin only observes and the built-in scheduler decides."),
                field("data_dir", "string", json!(d.data_dir), "Directory for state.json, events/*.jsonl and announcements.json."),
                field("drain_target", "number", json!(d.drain_target), "Fraction of each budget window to spend before it resets."),
                field("block_at", "number", json!(d.block_at), "New threads never start on an account at or above this fraction."),
                field("rate_soft", "number", json!(d.rate_soft), "Short windows at or above this fraction rank behind accounts with room."),
                field("preempt_at", "number", json!(d.preempt_at), "At or above this fraction of any window an account is near its limit: threads move off it before the limit is hit."),
                field("cooldown_default_secs", "integer", json!(d.cooldown_default_secs), "Cooldown after an upstream 429 that reports no reset time."),
                field("probe_after_secs", "integer", json!(d.probe_after_secs), "While a model is exhausted, let one request through to the host this often to find new accounts."),
                field("final_error", "boolean", json!(d.final_error), "Answer one final 429 when no account can serve, before the host tries any."),
                field("assumed_burn_per_hour", "number", json!(d.assumed_burn_per_hour), "Burn assumed for an account with no measured history."),
                field("risk_factor", "number", json!(d.risk_factor), "Share of sustainable burn at which an account counts as at risk."),
                field("affinity_ttl_secs", "integer", json!(d.affinity_ttl_secs), "Idle seconds after which a thread may move to another account."),
                field("gate_claude_subs", "boolean", json!(d.gate_claude_subs), "Reserve Claude subscription credentials for the listed clients."),
                field("claude_clients", "array", json!(d.claude_clients), "User-Agent prefixes allowed to use Claude subscriptions."),
                field("providers", "array", json!(d.providers), "Providers scheduled by this plugin."),
                field("log_events", "boolean", json!(d.log_events), "Write pick and usage events as JSON lines."),
            ],
        },
        "capabilities": {
            "scheduler": true,
            "scheduler_across_priorities": false,
            "usage_plugin": true,
            "management_api": true,
            "request_interceptor": true,
            "request_interceptor_omit_body_before": true,
            "request_interceptor_omit_body_after": true,
        },
    })
}

fn configure(request: &Value) -> Result<Value, String> {
    let yaml = request["config_yaml"]
        .as_str()
        .map(|s| B64.decode(s).map_err(|e| e.to_string()))
        .transpose()?
        .unwrap_or_default();
    let cfg = Config::from_yaml(&yaml)?;
    let at = now();
    let mut guard = lock();
    let path = state_path(&cfg);
    match guard.as_mut() {
        // A reconfigure that keeps the data directory keeps the live state.
        Some(rt) if rt.loaded_from.as_deref() == Some(path.as_path()) => rt.cfg = cfg,
        _ => {
            let mut state = load_state(&cfg);
            if state.label_key == [0, 0] {
                state.label_key = limits::new_label_key();
            }
            let restored = replay_cooldowns(&cfg, &mut state, at);
            if restored > 0 {
                log_event(
                    &cfg,
                    at,
                    json!({"t": "cooldowns_restored", "count": restored}),
                );
            }
            *guard = Some(Runtime {
                cfg,
                state,
                loaded_from: Some(path),
                last_save: at,
                selections: BTreeMap::new(),
            });
        }
    }
    Ok(registration())
}

fn header<'a>(headers: &'a Value, name: &str) -> &'a str {
    headers
        .as_object()
        .and_then(|m| m.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
        .and_then(|(_, v)| v.as_array().and_then(|a| a.last()).or(Some(v)))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

const SESSION_METADATA: &[&str] = &[
    "canonical_session_id",
    "lcp_affinity_session_id",
    "execution_session_id",
    "derived_session_id",
];
const SESSION_HEADERS: &[&str] = &[
    "X-Claude-Code-Session-Id",
    "Session_id",
    "Session-Id",
    "X-Session-ID",
    "X-Session-Affinity",
    "X-Conversation-Id",
    "X-Thread-Id",
    "Thread-Id",
];
const PARENT_HEADERS: &[&str] = &[
    "X-Codex-Parent-Thread-Id",
    "X-Parent-Session-ID",
    "X-Parent-Session-Affinity",
    "X-Parent-Conversation-Id",
    "X-Parent-Thread-Id",
];

fn first_nonempty<'a>(values: impl Iterator<Item = &'a str>) -> String {
    values
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Short client label for the event log: product and version, nothing else.
fn client_label(user_agent: &str) -> String {
    user_agent
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .take(48)
        .collect()
}

fn pick(request: &Value) -> Result<Value, String> {
    let at = now();
    let mut guard = lock();
    let rt = guard.as_mut().ok_or("plugin is not configured")?;
    let headers = &request["Options"]["Headers"];
    let metadata = &request["Options"]["Metadata"];
    let meta_str = |key: &str| metadata[key].as_str().unwrap_or("");
    let session = first_nonempty(
        SESSION_METADATA
            .iter()
            .map(|k| meta_str(k))
            .chain(SESSION_HEADERS.iter().map(|h| header(headers, h))),
    );
    let parent = first_nonempty(
        std::iter::once(meta_str("parent_session_id"))
            .chain(PARENT_HEADERS.iter().map(|h| header(headers, h))),
    );
    let user_agent = header(headers, "User-Agent").to_string();
    let candidates: Vec<Candidate> = request["Candidates"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|c| {
                    let attrs = &c["Attributes"];
                    let kind = attrs["auth_kind"].as_str().unwrap_or("");
                    Candidate {
                        id: c["ID"].as_str().unwrap_or("").to_string(),
                        provider: c["Provider"].as_str().unwrap_or("").to_string(),
                        // Config-synthesised API keys are marked `apikey`; everything else is a login.
                        subscription: !kind.eq_ignore_ascii_case("apikey")
                            && !kind.eq_ignore_ascii_case("api_key"),
                    }
                })
                .filter(|c| !c.id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let input = PickInput {
        model: request["Model"].as_str().unwrap_or("").to_string(),
        session: session.clone(),
        parent_session: parent.clone(),
        user_agent: user_agent.clone(),
        candidates,
    };
    let result = rt.state.pick(at, &input, &rt.cfg);
    let selection_id = meta_str("selection_id").to_string();
    if !selection_id.is_empty() && !session.is_empty() {
        if rt.selections.len() >= MAX_SELECTIONS {
            let cutoff = at - 3600.0;
            rt.selections.retain(|_, (_, t)| *t >= cutoff);
        }
        rt.selections
            .insert(selection_id.clone(), (session.clone(), at));
    }
    let (outcome, chosen, response) = match &result.outcome {
        PickOutcome::Choose(id) => (
            "choose",
            id.clone(),
            json!({"handled": true, "auth_id": id}),
        ),
        PickOutcome::Defer => ("defer", String::new(), json!({"handled": false})),
        PickOutcome::Reject(why) => (
            "reject",
            String::new(),
            json!({"handled": true, "reject": true, "reject_code": "client_not_allowed", "reject_reason": why}),
        ),
        // Reached only when the interceptor could not answer first (the pool was
        // not known yet, or this is the periodic probe). Nothing goes upstream;
        // the host turns `auth_unavailable` into a 503 and the client's retry
        // meets the interceptor's final 429.
        PickOutcome::Exhausted(ex) => (
            "exhausted",
            String::new(),
            json!({"handled": true, "reject": true, "reject_code": "auth_unavailable", "reject_reason": ex.message(at)}),
        ),
    };
    if let Some(t) = result.transition {
        log_transition(&rt.cfg, at, t, &input.model, &result.outcome);
    }
    let meta_keys: Vec<&String> = metadata
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    log_event(
        &rt.cfg,
        at,
        json!({
            "t": "pick",
            "selection_id": selection_id,
            "session": session,
            "parent_session": parent,
            "provider": request["Provider"],
            "providers": request["Providers"],
            "model": input.model,
            "stream": request["Stream"],
            "client": client_label(&user_agent),
            "task_class": header(headers, "X-Task-Class"),
            "path": meta_str("request_path"),
            "effort": meta_str("reasoning_effort"),
            "outcome": outcome,
            "chosen": chosen,
            "reason": result.decision.as_ref().map(|d| d.reason),
            "bound_before": result.bound_before,
            "gated": result.gated,
            "candidates": result.decision.as_ref().map(|d| &d.scores),
            "affinity_broken": result.decision.as_ref().and_then(|d| d.affinity_broken.as_ref()),
            "meta_keys": meta_keys,
        }),
    );
    if at - rt.last_save >= SAVE_EVERY_SECS {
        save_state(rt, at);
    }
    Ok(response)
}

/// One line when a model runs out of accounts and one when it is served again:
/// the router stays quiet in between, however many requests it refuses.
fn log_transition(cfg: &Config, at: f64, t: &str, model: &str, outcome: &PickOutcome) {
    let mut event = json!({"t": t, "model": model});
    if let PickOutcome::Exhausted(ex) = outcome {
        event["earliest_reset"] = json!(ex.earliest_reset);
        event["accounts"] = json!(ex.accounts);
    }
    log_event(cfg, at, event);
}

/// Before the host picks an account: answer the final error when the router
/// already knows no account for this model can serve. Pass through otherwise.
fn intercept_before(request: &Value) -> Result<Value, String> {
    let at = now();
    let mut guard = lock();
    let Some(rt) = guard.as_mut() else {
        return Ok(json!({}));
    };
    let model = [&request["Model"], &request["RequestedModel"]]
        .into_iter()
        .filter_map(Value::as_str)
        .find(|m| !m.trim().is_empty())
        .unwrap_or("")
        .to_string();
    let user_agent = header(&request["Headers"], "User-Agent").to_string();
    let Some((ex, first)) = rt.state.preflight(at, &model, &user_agent, &rt.cfg) else {
        return Ok(json!({}));
    };
    if first {
        log_event(
            &rt.cfg,
            at,
            json!({"t": "exhausted", "model": model, "earliest_reset": ex.earliest_reset, "accounts": ex.accounts}),
        );
    }
    let source = request["SourceFormat"].as_str().unwrap_or("");
    let anthropic = source.to_ascii_lowercase().starts_with("claude")
        || user_agent.to_ascii_lowercase().starts_with("claude-cli/");
    let (status, headers, body) = limits::final_error(&ex, at, anthropic);
    Ok(json!({
        "Terminate": true,
        "StatusCode": status,
        "ResponseHeaders": headers,
        "ResponseBody": B64.encode(serde_json::to_vec(&body).unwrap_or_default()),
    }))
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn usage(record: &Value) -> Result<Value, String> {
    let at = now();
    let mut guard = lock();
    let rt = guard.as_mut().ok_or("plugin is not configured")?;
    apply_announcements(rt, at);
    let headers: BTreeMap<String, String> = record["ResponseHeaders"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let value = v.as_array().and_then(|a| a.last()).unwrap_or(v).as_str()?;
                    Some((k.to_ascii_lowercase(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let selection_id = record["SelectionID"].as_str().unwrap_or("");
    let host_session = record["SessionID"].as_str().unwrap_or("");
    // Prefer the key the pick was made under, so bindings and usage line up.
    let session = rt
        .selections
        .get(selection_id)
        .map(|(s, _)| s.clone())
        .unwrap_or_else(|| host_session.to_string());
    let detail = &record["Detail"];
    let u = Usage {
        auth_id: record["AuthID"].as_str().unwrap_or("").to_string(),
        provider: record["Provider"].as_str().unwrap_or("").to_string(),
        model: record["Model"].as_str().unwrap_or("").to_string(),
        session: session.clone(),
        failed: record["Failed"].as_bool().unwrap_or(false),
        input_tokens: int(&detail["InputTokens"]),
        output_tokens: int(&detail["OutputTokens"]),
        cache_read_tokens: int(&detail["CacheReadTokens"]).max(
            if int(&detail["CacheCreationTokens"]) == 0 {
                int(&detail["CachedTokens"])
            } else {
                0
            },
        ),
        cache_write_tokens: int(&detail["CacheCreationTokens"]),
        headers,
        status: int(&record["Failure"]["StatusCode"]),
        failure_body: record["Failure"]["Body"].as_str().unwrap_or("").to_string(),
    };
    let observed = rt.state.observe(at, &u, &rt.cfg);
    if let Some((scope, c)) = &observed.cooldown {
        log_event(
            &rt.cfg,
            at,
            json!({"t": "cooldown", "auth_id": u.auth_id, "provider": u.provider, "model": u.model, "scope": scope,
                   "until": c.until, "source": c.source, "estimated": c.estimated}),
        );
        // A refusal must survive a restart: write it now, not at the next save.
        save_state(rt, at);
    }
    let quota_headers: Map<String, Value> = u
        .headers
        .iter()
        .filter(|(k, _)| {
            k.starts_with("anthropic-ratelimit-")
                || k.starts_with("x-codex-")
                || k.as_str() == "retry-after"
        })
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    log_event(
        &rt.cfg,
        at,
        json!({
            "t": "usage",
            "request_id": record["RequestID"],
            "trace_id": record["TraceID"],
            "selection_id": selection_id,
            "session": session,
            "host_session": host_session,
            "parent_session": record["ParentSessionID"],
            "provider": u.provider,
            "executor": record["ExecutorType"],
            "model": u.model,
            "alias": record["Alias"],
            "response_model": record["ResponseModel"],
            "auth_id": u.auth_id,
            "auth_type": record["AuthType"],
            "source": record["Source"],
            "effort": record["ReasoningEffort"],
            "service_tier": record["EffectiveServiceTier"],
            "stream": record["Stream"],
            "latency_ms": int(&record["Latency"]) / 1_000_000,
            "ttft_ms": int(&record["TTFT"]) / 1_000_000,
            "failed": u.failed,
            "status": record["Failure"]["StatusCode"],
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "reasoning_tokens": int(&detail["ReasoningTokens"]),
            "cache_read_tokens": u.cache_read_tokens,
            "cache_write_tokens": u.cache_write_tokens,
            "total_tokens": int(&detail["TotalTokens"]),
            "cache_miss": observed.cache_miss,
            "windows": observed.windows,
            "quota_headers": quota_headers,
        }),
    );
    if at - rt.last_save >= SAVE_EVERY_SECS {
        save_state(rt, at);
    }
    Ok(json!({}))
}

fn management_register() -> Value {
    json!({"routes": [
        {"Method": "GET", "Path": "/quota-router/report", "Description": "Per-account drain, waste and cache report."},
        {"Method": "GET", "Path": "/quota-router/state", "Description": "Raw plugin state."},
        {"Method": "GET", "Path": "/quota-router/status", "Description": "Per account and model: ok, near_limit or cooldown, reset time, and what would be picked now. Labels only, no identifiers."},
        {"Method": "POST", "Path": "/quota-router/resets", "Description": "Set free and banked reset counts for one account."},
    ]})
}

fn management_handle(request: &Value) -> Result<Value, String> {
    let at = now();
    let mut guard = lock();
    let rt = guard.as_mut().ok_or("plugin is not configured")?;
    apply_announcements(rt, at);
    let method = request["Method"]
        .as_str()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let path = request["Path"].as_str().unwrap_or("");
    let reply = |status: u16, body: Value| {
        json!({
            "StatusCode": status,
            "Headers": {"Content-Type": ["application/json"]},
            "Body": B64.encode(serde_json::to_vec(&body).unwrap_or_default()),
        })
    };
    if method == "GET" && path.ends_with("/quota-router/report") {
        return Ok(reply(200, rt.state.report(at, &rt.cfg)));
    }
    if method == "GET" && path.ends_with("/quota-router/status") {
        return Ok(reply(200, rt.state.status(at, &rt.cfg)));
    }
    if method == "GET" && path.ends_with("/quota-router/state") {
        return Ok(reply(
            200,
            serde_json::to_value(&rt.state).map_err(|e| e.to_string())?,
        ));
    }
    if method == "POST" && path.ends_with("/quota-router/resets") {
        let raw = request["Body"]
            .as_str()
            .map(|s| B64.decode(s).unwrap_or_default())
            .unwrap_or_default();
        let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
            return Ok(reply(
                400,
                json!({"error": "body must be JSON: {auth_id or label, free_resets?, banked_resets?}"}),
            ));
        };
        let by_label = body["label"]
            .as_str()
            .and_then(|l| rt.state.auth_for_label(l));
        let id = by_label
            .as_deref()
            .or(body["auth_id"].as_str())
            .unwrap_or("")
            .to_string();
        let id = id.as_str();
        let free = body["free_resets"].as_u64().map(|v| v as u32);
        let banked = body["banked_resets"].as_u64().map(|v| v as u32);
        if !rt.state.set_resets(id, free, banked) {
            return Ok(reply(
                404,
                json!({"error": "unknown auth_id or label; an account appears after its first request (labels are listed by GET /quota-router/status)"}),
            ));
        }
        log_event(
            &rt.cfg,
            at,
            json!({"t": "resets", "auth_id": id, "free_resets": free, "banked_resets": banked}),
        );
        save_state(rt, at);
        return Ok(reply(200, json!({"status": "ok"})));
    }
    Ok(reply(404, json!({"error": "unknown quota-router route"})))
}

fn dispatch(method: &str, request: &[u8]) -> Result<Value, String> {
    let request: Value = if request.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(request).map_err(|e| e.to_string())?
    };
    match method {
        "plugin.register" | "plugin.reconfigure" => configure(&request),
        "scheduler.pick" => pick(&request),
        "usage.handle" => usage(&request),
        "request.intercept_before" => intercept_before(&request),
        "request.intercept_after" => Ok(json!({})),
        "management.register" => Ok(management_register()),
        "management.handle" => management_handle(&request),
        "plugin.quiesce" | "plugin.shutdown" => {
            flush();
            Ok(json!({}))
        }
        other => Err(format!("unknown method: {other}")),
    }
}

fn flush() {
    let mut guard = lock();
    if let Some(rt) = guard.as_mut() {
        save_state(rt, now());
    }
}

// ----------------------------------------------------------------------- ABI

#[repr(C)]
pub struct Buffer {
    ptr: *mut u8,
    len: usize,
}

#[repr(C)]
pub struct HostApi {
    abi_version: u32,
    host_ctx: *mut c_void,
    call: *const c_void,
    free_buffer: *const c_void,
}

type PluginCall = unsafe extern "C" fn(*const c_char, *const u8, usize, *mut Buffer) -> i32;
type PluginFree = unsafe extern "C" fn(*mut c_void, usize);
type PluginShutdown = unsafe extern "C" fn();

#[repr(C)]
pub struct PluginApi {
    abi_version: u32,
    call: Option<PluginCall>,
    free_buffer: Option<PluginFree>,
    shutdown: Option<PluginShutdown>,
}

/// # Safety
/// Called once by the host with a valid `plugin` pointer.
#[no_mangle]
pub unsafe extern "C" fn cliproxy_plugin_init(
    _host: *const HostApi,
    plugin: *mut PluginApi,
) -> i32 {
    if plugin.is_null() {
        return 1;
    }
    (*plugin).abi_version = ABI_VERSION;
    (*plugin).call = Some(plugin_call);
    (*plugin).free_buffer = Some(plugin_free);
    (*plugin).shutdown = Some(plugin_shutdown);
    0
}

fn envelope(result: Result<Value, String>) -> (Vec<u8>, i32) {
    let (value, code) = match result {
        Ok(v) => (json!({"ok": true, "result": v}), 0),
        Err(message) => (
            json!({"ok": false, "error": {"code": "plugin_error", "message": message}}),
            1,
        ),
    };
    (
        serde_json::to_vec(&value).unwrap_or_else(|_| {
            br#"{"ok":false,"error":{"code":"plugin_error","message":"encode"}}"#.to_vec()
        }),
        code,
    )
}

unsafe extern "C" fn plugin_call(
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut Buffer,
) -> i32 {
    if !response.is_null() {
        (*response).ptr = std::ptr::null_mut();
        (*response).len = 0;
    }
    // A panic must never cross the C boundary.
    let outcome = std::panic::catch_unwind(|| {
        if method.is_null() {
            return Err("method is required".to_string());
        }
        let method = CStr::from_ptr(method)
            .to_str()
            .map_err(|_| "method is not utf-8".to_string())?;
        let body: &[u8] = if request.is_null() || request_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(request, request_len)
        };
        dispatch(method, body)
    })
    .unwrap_or_else(|_| Err("plugin panicked".to_string()));
    let (bytes, code) = envelope(outcome);
    if !response.is_null() {
        let boxed = bytes.into_boxed_slice();
        (*response).len = boxed.len();
        (*response).ptr = Box::into_raw(boxed) as *mut u8;
    }
    code
}

unsafe extern "C" fn plugin_free(ptr: *mut c_void, len: usize) {
    if !ptr.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            ptr as *mut u8,
            len,
        )));
    }
}

unsafe extern "C" fn plugin_shutdown() {
    let _ = std::panic::catch_unwind(flush);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, body: Value) -> Value {
        dispatch(method, &serde_json::to_vec(&body).unwrap()).unwrap()
    }

    fn configure_at(dir: &Path) {
        let yaml = format!("data_dir: '{}'\n", dir.display());
        call("plugin.register", json!({"config_yaml": B64.encode(yaml)}));
    }

    fn codex_pick(model: &str, session: &str) -> Value {
        call(
            "scheduler.pick",
            json!({
                "Provider": "codex", "Model": model,
                "Options": {"Headers": {"User-Agent": ["codex_cli_rs/0.200.0"], "Session_id": [session]}, "Metadata": {}},
                "Candidates": [{"ID": "a.json", "Provider": "codex", "Attributes": {}},
                               {"ID": "b.json", "Provider": "codex", "Attributes": {}}],
            }),
        )
    }

    fn intercept(model: &str) -> Value {
        call(
            "request.intercept_before",
            json!({"SourceFormat": "openai-response", "Model": model,
                   "Headers": {"User-Agent": ["codex_cli_rs/0.200.0"]}}),
        )
    }

    fn body_of(answer: &Value) -> Value {
        let raw = B64
            .decode(answer["ResponseBody"].as_str().unwrap())
            .unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    /// The whole plugin through its ABI entry: a usage limit seen on every
    /// account survives a restart, and a client that retries blindly gets the
    /// same final 429 at once, every time, without the router asking the host
    /// to pick (so nothing more is sent upstream). One test, because the plugin
    /// runtime is process-global.
    #[test]
    fn exhausted_model_answers_one_final_error_across_restart_and_retries() {
        let dir = std::env::temp_dir().join(format!(
            "quota-router-test-{}-{}",
            std::process::id(),
            now() as u64
        ));
        let _ = fs::remove_dir_all(&dir);
        configure_at(&dir);
        let model = "gpt-5.5-codex";
        assert_eq!(intercept(model), json!({}), "unknown model passes through");
        assert_eq!(codex_pick(model, "t")["auth_id"], "a.json");
        let reset = (now() + 3600.0).floor();
        for id in ["a.json", "b.json"] {
            let body =
                format!(r#"{{"error":{{"type":"usage_limit_reached","resets_at":{reset}}}}}"#);
            call(
                "usage.handle",
                json!({"AuthID": id, "Provider": "codex", "Model": model, "Failed": true,
                       "Failure": {"StatusCode": 429, "Body": body}}),
            );
        }
        // Restart without a clean shutdown: the cooldown was written when it was seen.
        *lock() = None;
        configure_at(&dir);
        let picks_before = lock().as_ref().unwrap().state.picks;
        let started = std::time::Instant::now();
        let answers: Vec<Value> = (0..20).map(|_| intercept(model)).collect();
        let elapsed = started.elapsed();
        assert!(elapsed.as_millis() < 2000, "20 refusals took {elapsed:?}");
        let first = &answers[0];
        assert_eq!(first["Terminate"], true);
        assert_eq!(first["StatusCode"], 429);
        assert_eq!(first["ResponseHeaders"]["X-Should-Retry"], json!(["false"]));
        let retry_after: f64 = first["ResponseHeaders"]["Retry-After"][0]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((retry_after - (reset - now())).abs() <= 2.0);
        let body = body_of(first);
        assert_eq!(body["error"]["type"], "usage_limit_reached");
        assert_eq!(body["error"]["resets_at"], reset as i64);
        for a in &answers {
            let b = body_of(a);
            assert_eq!(b["error"]["resets_at"], body["error"]["resets_at"]);
            assert_eq!(b["error"]["code"], "quota_router_exhausted");
            assert_eq!(a["StatusCode"], 429);
        }
        {
            let guard = lock();
            let rt = guard.as_ref().unwrap();
            assert_eq!(
                rt.state.picks, picks_before,
                "no retry reached the scheduler"
            );
            assert_eq!(rt.state.refused, 20);
        }
        // One `exhausted` line in the event log, not one per refusal.
        let log = dir
            .join("events")
            .join(format!("{}.jsonl", utc_date(now())));
        let events = fs::read_to_string(log).unwrap();
        let exhausted_lines = events
            .lines()
            .filter(|l| l.contains(r#""t":"exhausted""#))
            .count();
        assert_eq!(exhausted_lines, 1);
        // The status route shows it, by label only.
        let status = call(
            "management.handle",
            json!({"Method": "GET", "Path": "/v0/management/quota-router/status"}),
        );
        let raw = B64.decode(status["Body"].as_str().unwrap()).unwrap();
        let status: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(status["models"][0]["state"], "exhausted");
        assert!(!status.to_string().contains(".json"));

        // A cooldown only in the event log (the state file predates it) is put back.
        *lock() = None;
        let raw = fs::read(dir.join("state.json")).unwrap();
        let mut st: State = serde_json::from_slice(&raw).unwrap();
        for a in st.accounts.values_mut() {
            a.cooldowns.clear();
        }
        st.saved_at = now() - 60.0;
        fs::write(dir.join("state.json"), serde_json::to_vec(&st).unwrap()).unwrap();
        configure_at(&dir);
        assert_eq!(intercept(model)["StatusCode"], 429);
        *lock() = None;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(utc_date(0.0), "1970-01-01");
        assert_eq!(utc_date(1_794_355_200.0), "2026-11-11");
        assert_eq!(utc_date(951_782_400.0), "2000-02-29");
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_takes_last_value() {
        let h = json!({"user-agent": ["a", "claude-cli/2.1 (external)"]});
        assert_eq!(header(&h, "User-Agent"), "claude-cli/2.1 (external)");
        assert_eq!(header(&h, "Missing"), "");
        assert_eq!(
            client_label("claude-cli/2.1 (external, cli)"),
            "claude-cli/2.1"
        );
    }
}
