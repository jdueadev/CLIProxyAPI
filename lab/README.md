# lab/ — what this fork adds

Branch `lab/main` carries everything here. `main` tracks upstream and has no local changes.
Nothing under `cmd/`, `internal/` or `sdk/` is modified: the additions are a plugin and a
tool that use upstream's public plugin ABI, so an upstream merge cannot conflict with them.

| Path | What it is |
|---|---|
| `quota-router/` | Rust plugin (C ABI). Chooses the account for each request, keeps threads on their account, reserves Claude subscriptions for Claude Code and T3 Code, and records quota, cache and spend. |
| `quota-tools/` | Rust CLI over the plugin's event log: training rows, A/B comparison, and the free-reset announcement watcher. |
| `config.template.yaml` | Config for a lab machine, with 1Password references in place of secrets. |

Install on a machine: `D:\scripts\powershell\Install-CliProxy.ps1` (dry run by default).

## Turning it off

Each line is independent and takes effect on config reload, no restart.

| To undo | Set in `config.yaml` |
|---|---|
| Quota routing only (keep monitoring and the client gate) | `plugins.configs.quota-router.route: false` |
| The Claude client gate only | `plugins.configs.quota-router.gate_claude_subs: false` |
| The whole plugin | `plugins.configs.quota-router.enabled: false` |
| Everything, including the tasks and binaries | `Install-CliProxy.ps1 -Uninstall -Execute` |

With the plugin off the server is stock upstream. To get upstream's own thread stickiness
back in that case, also set `routing.session-affinity: true`.

## How an account is chosen

Upstream's default spreads requests evenly. The plugin replaces that for the providers
listed in `providers` (default `claude`, `codex`). For each request, in order:

1. **Client gate.** A Claude subscription login is only offered to clients whose
   User-Agent starts with an entry in `claude_clients` (default `claude-cli/`, which is
   Claude Code and anything built on the Claude Agent SDK, T3 Code included). Other clients
   get another credential if one exists, otherwise HTTP 4xx with `client_not_allowed`.
2. **Thread affinity.** A thread stays on the account it last used while that account is
   offered, because provider prompt caches are per account. A subagent starts on its
   parent's account. A binding lapses after `affinity_ttl_secs` idle (default 1 hour),
   when the cache is cold anyway and moving costs nothing.
3. **Drain what would be wasted.** For a new thread, each account's weekly window gives
   `need = drain_target - used` and `required burn = need / hours to reset`. An account
   whose required burn reaches `risk_factor` x its sustainable burn is *at risk* and goes
   first, highest required burn first.
4. **Free resets next.** Accounts holding a free reset rank ahead of the rest, but never
   ahead of an at-risk account.
5. **Then highest required burn**, ties to the least recently used.

Short windows (5-hour) are rate limits, not budgets: at `rate_soft` (90%) an account ranks
behind any account with room; at `block_at` (98%) no new thread starts there. If every
account is blocked the host's scheduler decides. **Banked resets never raise an account's
rank** and nothing spends one automatically.

The decision is a pure function (`quota-router/src/policy.rs`) with its cases as tests.

### Where the numbers come from

Quota is read from the rate-limit headers on each upstream response
(`anthropic-ratelimit-unified-*`, `x-codex-*`). An account that has not served a request
since start has no numbers and is treated as an untouched week, which is what gets it
probed. Sustainable burn is measured per account while it is in use
(`assumed_burn_per_hour` until then).

Reset counts are not exposed by any header we have seen, so they are set by hand or by a
confirmed announcement:

```
POST /v0/management/quota-router/resets   {"auth_id": "...", "free_resets": 1, "banked_resets": 2}
```

## Monitoring

All under `data_dir` (on lab machines `D:\vaults\cliproxy\quota-router`, private: account
identifiers appear in it; prompts and responses never do).

- `events/YYYY-MM-DD.jsonl`: one `pick` line per routing decision (client, task class,
  every candidate's tier and numbers, the choice and why) and one `usage` line per completed
  request (tokens, cache read and write, latency, TTFT, failure, the quota headers seen).
  The two share `selection_id`.
- `state.json`: current quota, bindings and counters.
- `GET /v0/management/quota-router/report` (management key required): per account, the
  projected use at reset, projected waste, what went unspent at the last reset, burn rates,
  cache-read share, cache misses, idle hours and plain advice. This is the
  underutilisation view.
- **Cache misses.** A request on a warm thread (same account, previous turn inside
  `cache_ttl_secs`) with at least `cache_miss_min_tokens` of prompt and zero cache read is
  flagged in its `usage` line and counted per account. Thread moves between accounts are
  counted as `rebinds`.

Clients may send `X-Task-Class: <label>`; it is recorded on the pick and carried into the
training rows.

### Training rows

```
quota-tools dataset --data D:\vaults\cliproxy\quota-router --out rows.jsonl
```

One line per request: what the router knew when it chose (`pick_*`, client, task class,
model, effort) joined to what happened. This is the input for a routing classifier and for
deciding which task classes a local model can take: compare failure rate and tokens by
`task_class` and `model` before moving anything.

```
quota-tools compare --data D:\vaults\cliproxy\quota-router --by task_class --where provider=claude
```

## Free-reset announcements

```
quota-tools watch   --data D:\vaults\cliproxy\quota-router       # daily task; exit 3 when something new
quota-tools confirm --data D:\vaults\cliproxy\quota-router --id <id> --provider codex [--clears-usage]
```

`watch` reads the Claude and OpenAI status feeds and the OpenAI news feed and keeps entries
that mention both a limit and a reset. A match only lands in `pending.json`. Routing changes
when a person runs `confirm`: every account of that provider gains a free reset, and with
`--clears-usage` its recorded usage is dropped because the windows restarted.

## Codex over WebSocket: how to decide

The WebSocket path is used only when the client connects to the proxy over WebSocket *and*
the Codex login has `"websockets": true` in its auth JSON. To measure it:

1. Run a fixed set of Codex tasks with `"websockets": false` on the login.
2. Set it to `true`, run the same set.
3. `quota-tools compare --data ... --by executor --where provider=codex --since <start>`

Compare TTFT and latency percentiles, output tokens per second, failure rate and cache-read
share. Groups under 30 requests are reported as too small. Keep whichever wins; the flag is
per login, so it can also be left on for some accounts only.

## Limits, stated plainly

- Verified end to end against a mock upstream only (two keys reporting different quota).
  No real Claude or Codex login has been routed yet, so the header names and the 0-1 versus
  percent reading for Claude are from upstream's source, not from a live response. The raw
  headers are in every `usage` line to check against.
- The client gate reads the User-Agent, which any client can set. It stops accidents, not a
  determined caller on the same machine.
- Quota for an idle account is only as fresh as its last response.
- Social-media posts are not watched: there is no feed to read.
- Pooling several subscriptions behind one endpoint may be against a provider's terms.
  That is the account holder's call; this code does not hide what client is calling.
