//! quota-tools: offline tools over quota-router's data directory.
//!
//!   dataset  join each pick with its outcome into one training row per request
//!   compare  group completed requests by a field and compare latency, failures, cache
//!   watch    scan provider feeds for free-reset announcements (writes pending.json)
//!   confirm  turn a reviewed pending item into an announcement the router applies
//!   status   per account and model: ok, near_limit or cooldown, and what the router would pick
//!   post-resets  send one account's free and banked reset counts to a running router
//!
//! Human-readable text goes to stderr; data goes to stdout.

mod feeds;
mod stats;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
quota-tools <command> [options]

  dataset --data <dir> [--out <file>]
      One JSON line per request: what the router knew when it chose, joined with
      what happened (tokens, cache, latency, failure). Default output: stdout.

  compare --data <dir> --by <field> [--where <field>=<value>]... [--since <unix-secs>]
      Groups completed requests by <field> (for example executor, auth_id, model,
      client, task_class) and prints count, failure rate, latency and TTFT
      percentiles, mean output tokens and cache-read share as JSON.

  watch --data <dir> [--feed <url>]... [--no-default-feeds]
      Fetches provider feeds, keeps entries that look like a usage-limit reset,
      and adds new ones to <dir>/pending.json. Nothing changes routing until
      `confirm`. Exit code 3 when new items were found.

  confirm --data <dir> --id <pending-id> --provider <claude|codex> [--grant <n>] [--clears-usage]
      Appends the reviewed item to <dir>/announcements.json. The router then gives
      every account of that provider <n> free resets (default 1).

  status --data <dir> [--json]
      Reads <dir>/state.json and prints, as JSON with schema_version, each account
      (by label, never by identifier) and each model: ok, near_limit or cooldown,
      the reset time from the upstream, and the account the router would pick now.
      The same document as GET /v0/management/quota-router/status, computed with
      this machine's clock from the last saved state (see state_saved_at).
      Exit code 4 when some model has no account that can serve.

  post-resets --url <proxy base url> --label <acct-...> [--free <n>] [--banked <n>]
      Sends reset counts for one account to a running router
      (POST /v0/management/quota-router/resets). The management key is read from
      the QUOTA_ROUTER_MANAGEMENT_KEY environment variable and never printed.
";

const EXIT_EXHAUSTED: u8 = 4;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let opts = match Opts::parse(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command.as_str() {
        "dataset" => dataset(&opts),
        "compare" => compare(&opts),
        "watch" => watch(&opts),
        "confirm" => confirm(&opts),
        "status" => status(&opts),
        "post-resets" => post_resets(&opts),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command: {other}")),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

#[derive(Default)]
struct Opts {
    values: BTreeMap<String, Vec<String>>,
    flags: Vec<String>,
}

impl Opts {
    const FLAGS: &'static [&'static str] = &["no-default-feeds", "clears-usage", "json"];

    fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = Opts::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let name = arg
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument: {arg}"))?;
            if Self::FLAGS.contains(&name) {
                out.flags.push(name.to_string());
            } else {
                let value = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
                out.values
                    .entry(name.to_string())
                    .or_default()
                    .push(value.clone());
            }
        }
        Ok(out)
    }

    fn one(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.one(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn all(&self, name: &str) -> &[String] {
        self.values.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn data(&self) -> Result<PathBuf, String> {
        Ok(PathBuf::from(self.required("data")?))
    }
}

/// Every event in `<data>/events/*.jsonl`, oldest file first. Lines that do not
/// parse are counted, not fatal: the log is appended to while we read.
fn read_events(data: &Path) -> Result<(Vec<Value>, usize), String> {
    let dir = data.join("events");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut events = Vec::new();
    let mut bad = 0;
    for file in files {
        let f =
            fs::File::open(&file).map_err(|e| format!("cannot open {}: {e}", file.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line.map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => events.push(v),
                Err(_) => bad += 1,
            }
        }
    }
    Ok((events, bad))
}

/// Joins picks to outcomes by selection id. Requests the host scheduled itself
/// have no selection id and come through with `pick: null`.
fn joined_rows(events: &[Value]) -> Vec<Value> {
    let picks: BTreeMap<&str, &Value> = events
        .iter()
        .filter(|e| e["t"] == "pick")
        .filter_map(|e| {
            e["selection_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| (s, e))
        })
        .collect();
    events
        .iter()
        .filter(|e| e["t"] == "usage")
        .map(|u| {
            let pick = u["selection_id"]
                .as_str()
                .and_then(|s| picks.get(s))
                .copied();
            let chosen = pick.and_then(|p| {
                let id = p["chosen"].as_str()?;
                p["candidates"].as_array()?.iter().find(|c| c["id"] == id)
            });
            json!({
                "ts": u["ts"],
                "request_id": u["request_id"],
                "session": u["session"],
                "provider": u["provider"],
                "model": u["model"],
                "executor": u["executor"],
                "auth_id": u["auth_id"],
                "effort": u["effort"],
                "stream": u["stream"],
                "client": pick.map(|p| &p["client"]),
                "task_class": pick.map(|p| &p["task_class"]),
                "path": pick.map(|p| &p["path"]),
                "pick_reason": pick.map(|p| &p["reason"]),
                "pick_candidates": pick.and_then(|p| p["candidates"].as_array().map(Vec::len)),
                "pick_tier": chosen.map(|c| &c["tier"]),
                "pick_budget_used": chosen.map(|c| &c["budget_used"]),
                "pick_rate_used": chosen.map(|c| &c["rate_used"]),
                "pick_required_burn": chosen.map(|c| &c["required_burn"]),
                "pick_hours_left": chosen.map(|c| &c["hours_left"]),
                "failed": u["failed"],
                "status": u["status"],
                "latency_ms": u["latency_ms"],
                "ttft_ms": u["ttft_ms"],
                "input_tokens": u["input_tokens"],
                "output_tokens": u["output_tokens"],
                "reasoning_tokens": u["reasoning_tokens"],
                "cache_read_tokens": u["cache_read_tokens"],
                "cache_write_tokens": u["cache_write_tokens"],
                "cache_miss": u["cache_miss"],
            })
        })
        .collect()
}

fn dataset(opts: &Opts) -> Result<u8, String> {
    let (events, bad) = read_events(&opts.data()?)?;
    let rows = joined_rows(&events);
    let mut out: Box<dyn Write> = match opts.one("out") {
        Some(path) => {
            Box::new(fs::File::create(path).map_err(|e| format!("cannot create {path}: {e}"))?)
        }
        None => Box::new(std::io::stdout().lock()),
    };
    for row in &rows {
        writeln!(out, "{row}").map_err(|e| e.to_string())?;
    }
    let with_pick = rows.iter().filter(|r| !r["pick_reason"].is_null()).count();
    eprintln!(
        "{} rows ({} joined to a pick, {} unreadable lines skipped)",
        rows.len(),
        with_pick,
        bad
    );
    Ok(0)
}

fn compare(opts: &Opts) -> Result<u8, String> {
    let by = opts.required("by")?;
    let since: f64 = opts
        .one("since")
        .map(|s| s.parse().map_err(|_| "--since must be a number"))
        .transpose()?
        .unwrap_or(0.0);
    let filters: Vec<(&str, &str)> = opts
        .all("where")
        .iter()
        .map(|w| {
            w.split_once('=')
                .ok_or_else(|| format!("--where expects field=value, got {w}"))
        })
        .collect::<Result<_, _>>()?;
    let (events, _) = read_events(&opts.data()?)?;
    let rows = joined_rows(&events);
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for row in &rows {
        if row["ts"].as_f64().unwrap_or(0.0) < since
            || filters.iter().any(|(f, v)| text(&row[*f]) != *v)
        {
            continue;
        }
        groups.entry(text(&row[by])).or_default().push(row);
    }
    let report: Vec<Value> = groups
        .iter()
        .map(|(key, rows)| stats::summarise(by, key, rows))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
    );
    if report.iter().any(|g| g["n"].as_u64().unwrap_or(0) < 30) {
        eprintln!("note: groups under 30 requests are too small to compare; treat their numbers as anecdotes");
    }
    Ok(0)
}

fn read_list(path: &Path) -> Result<Vec<Value>, String> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map_err(|e| format!("{} is not a JSON list: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn write_list(path: &Path, list: &[Value]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let raw = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
    fs::write(&tmp, raw)
        .and_then(|_| fs::rename(&tmp, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn watch(opts: &Opts) -> Result<u8, String> {
    let data = opts.data()?;
    fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let mut urls: Vec<String> = if opts.flag("no-default-feeds") {
        Vec::new()
    } else {
        feeds::DEFAULT_FEEDS.iter().map(|s| s.to_string()).collect()
    };
    urls.extend(opts.all("feed").iter().cloned());
    let path = data.join("pending.json");
    let mut pending = read_list(&path)?;
    let mut added = 0;
    let mut failed = 0;
    for url in &urls {
        let body = match feeds::fetch(url) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("feed failed: {url}: {e}");
                failed += 1;
                continue;
            }
        };
        for entry in feeds::parse(&body) {
            if !feeds::looks_like_reset(&entry) {
                continue;
            }
            let id = feeds::entry_id(url, &entry);
            if pending.iter().any(|p| p["id"] == id.as_str()) {
                continue;
            }
            eprintln!("new: [{id}] {} ({})", entry.title, entry.link);
            pending.push(json!({
                "id": id,
                "feed": url,
                "title": entry.title,
                "link": entry.link,
                "published": entry.published,
                "provider_guess": feeds::provider_guess(url, &entry),
                "status": "pending",
            }));
            added += 1;
        }
    }
    if added > 0 {
        write_list(&path, &pending)?;
    }
    eprintln!(
        "{} feeds read, {} failed, {} new items; pending file: {}",
        urls.len() - failed,
        failed,
        added,
        path.display()
    );
    if failed == urls.len() && !urls.is_empty() {
        return Err("every feed failed".into());
    }
    Ok(if added > 0 { 3 } else { 0 })
}

fn confirm(opts: &Opts) -> Result<u8, String> {
    let data = opts.data()?;
    let id = opts.required("id")?;
    let provider = opts.required("provider")?;
    if !matches!(provider, "claude" | "codex") {
        return Err("--provider must be claude or codex".into());
    }
    let grant: u64 = opts
        .one("grant")
        .map(|s| s.parse().map_err(|_| "--grant must be a whole number"))
        .transpose()?
        .unwrap_or(1);
    let pending_path = data.join("pending.json");
    let mut pending = read_list(&pending_path)?;
    let item = pending
        .iter_mut()
        .find(|p| p["id"] == id)
        .ok_or_else(|| format!("no pending item with id {id}"))?;
    if item["status"] == "confirmed" {
        return Err(format!("{id} is already confirmed"));
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs_f64();
    item["status"] = json!("confirmed");
    let title = item["title"].clone();
    let link = item["link"].clone();
    let ann_path = data.join("announcements.json");
    let mut announcements = read_list(&ann_path)?;
    announcements.push(json!({
        "provider": provider,
        "at": at,
        "grant": grant,
        "clears_usage": opts.flag("clears-usage"),
        "source_id": id,
        "title": title,
        "link": link,
    }));
    write_list(&ann_path, &announcements)?;
    write_list(&pending_path, &pending)?;
    eprintln!("confirmed {id}: {provider} accounts gain {grant} free reset(s) on the router's next request");
    Ok(0)
}

fn unix_now() -> Result<f64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .map_err(|e| format!("system clock is before 1970: {e}"))
}

/// The status document from a saved state file, at `now`.
fn status_document(data: &Path, now: f64) -> Result<Value, String> {
    let path = data.join("state.json");
    let raw = fs::read(&path).map_err(|e| {
        format!(
            "cannot read {}: {e}. Point --data at the router's data_dir (the plugin writes state.json there every 30 seconds).",
            path.display()
        )
    })?;
    let state: quota_router::state::State = serde_json::from_slice(&raw)
        .map_err(|e| format!("{} is not a quota-router state file: {e}", path.display()))?;
    let cfg = state.settings.clone().unwrap_or_default();
    let mut doc = state.status(now, &cfg);
    doc["source"] = json!("state_file");
    Ok(doc)
}

fn status(opts: &Opts) -> Result<u8, String> {
    let doc = status_document(&opts.data()?, unix_now()?)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?
    );
    let exhausted = doc["models"]
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m["state"] == "exhausted"));
    Ok(if exhausted { EXIT_EXHAUSTED } else { 0 })
}

fn post_resets(opts: &Opts) -> Result<u8, String> {
    let base = opts.required("url")?.trim_end_matches('/');
    let label = opts.required("label")?;
    let count = |name: &str| -> Result<Option<u64>, String> {
        opts.one(name)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("--{name} must be a whole number"))
            })
            .transpose()
    };
    let (free, banked) = (count("free")?, count("banked")?);
    if free.is_none() && banked.is_none() {
        return Err("give --free, --banked or both".into());
    }
    let key = std::env::var("QUOTA_ROUTER_MANAGEMENT_KEY").map_err(|_| {
        "QUOTA_ROUTER_MANAGEMENT_KEY is not set; export the proxy's management key (for example with `op run`) and run again".to_string()
    })?;
    let mut body = json!({"label": label});
    if let Some(v) = free {
        body["free_resets"] = json!(v);
    }
    if let Some(v) = banked {
        body["banked_resets"] = json!(v);
    }
    let url = format!("{base}/v0/management/quota-router/resets");
    let reply = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .post(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    match reply {
        Ok(_) => {
            eprintln!("posted reset counts for {label}");
            Ok(0)
        }
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "router answered {code}: {}",
            r.into_string().unwrap_or_default()
        )),
        Err(e) => Err(format!("cannot reach {url}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_join_to_usage_by_selection_id() {
        let events = vec![
            json!({"t": "pick", "selection_id": "s1", "client": "claude-cli/2.1", "task_class": "review", "reason": "policy", "chosen": "a",
                   "candidates": [{"id": "a", "tier": "at_risk", "budget_used": 0.3}, {"id": "b", "tier": "normal"}]}),
            json!({"t": "usage", "selection_id": "s1", "auth_id": "a", "latency_ms": 100, "failed": false}),
            json!({"t": "usage", "selection_id": "", "auth_id": "b", "latency_ms": 50, "failed": true}),
        ];
        let rows = joined_rows(&events);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["client"], "claude-cli/2.1");
        assert_eq!(rows[0]["pick_tier"], "at_risk");
        assert_eq!(rows[0]["pick_candidates"], 2);
        assert!(rows[1]["pick_reason"].is_null());
    }

    #[test]
    fn status_reads_the_saved_state_by_label() {
        use quota_router::state::{Candidate, PickInput, State, Usage};
        let dir = std::env::temp_dir().join(format!("quota-tools-status-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cfg = quota_router::config::Config::default();
        let mut s = State::default();
        let input = PickInput {
            model: "gpt-5.5-codex".into(),
            user_agent: "codex_cli_rs/0.200.0".into(),
            candidates: vec![Candidate {
                id: "secret-account.json".into(),
                provider: "codex".into(),
                subscription: true,
            }],
            ..Default::default()
        };
        s.pick(100.0, &input, &cfg);
        let refusal = Usage {
            auth_id: "secret-account.json".into(),
            provider: "codex".into(),
            model: "gpt-5.5-codex".into(),
            failed: true,
            status: 429,
            failure_body: r#"{"error":{"type":"usage_limit_reached","resets_at":5000}}"#.into(),
            ..Default::default()
        };
        s.observe(100.0, &refusal, &cfg);
        s.settings = Some(cfg);
        fs::write(dir.join("state.json"), serde_json::to_vec(&s).unwrap()).unwrap();
        let doc = status_document(&dir, 200.0).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["source"], "state_file");
        assert_eq!(doc["models"][0]["state"], "exhausted");
        assert_eq!(doc["models"][0]["earliest_reset_unix"], 5000);
        assert_eq!(doc["accounts"][0]["state"], "cooldown");
        assert!(!doc.to_string().contains("secret-account"));
    }

    #[test]
    fn options_parse() {
        let args: Vec<String> = [
            "--data",
            "d",
            "--where",
            "a=b",
            "--where",
            "c=d",
            "--clears-usage",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = Opts::parse(&args).unwrap();
        assert_eq!(o.one("data"), Some("d"));
        assert_eq!(o.all("where").len(), 2);
        assert!(o.flag("clears-usage"));
        assert!(Opts::parse(&["stray".to_string()]).is_err());
        assert!(Opts::parse(&["--data".to_string()]).is_err());
    }
}
