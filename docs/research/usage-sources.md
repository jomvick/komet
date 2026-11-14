# Usage sources per agent (issue #53)

Status: **draft — filled in from probe results, not from any third-party code.**

Goal: every agent added in Settings › Usage shows the best usage figure Komet
can honestly produce for the **account that is active on that agent right now**,
and says where the figure came from and how complete it is.

## Principles

1. **Active account only.** Sources are resolved for the active account of the
   agent. Inactive accounts are never probed; their last sample is shown dated.
2. **Ordered sources, best first.** Per agent: provider quota → local logs →
   Komet-measured turns. The first source that answers wins for the *quota*
   block; local logs and measured turns feed the *tokens* block.
3. **Never invent a zero.** No data is "not reported" with a reason, never `0`.
4. **Say how complete.** Every figure carries a source label and a
   `Coverage` (`complete` / `partial` / `unavailable`).
5. **Cheap.** Local readers are incremental (by file mtime and size), bounded in
   memory, and run only while the Usage page is visible.
6. **Read-only, local.** Readers never write to an agent's store and never keep
   message text; only counters, timestamps, model names.

## Account attribution

Agent logs do not say which account produced a turn. Komet keeps an
**activation timeline** per agent (`crates/engine/src/usage_sources.rs`):

- every switch Komet performs is recorded;
- at startup the live account is compared with the last recorded one; if it
  differs, the stretch since the last known entry is `Unknown` (a login done
  outside Komet) and is excluded from per-account totals, which then show
  `partial` coverage;
- agents without Komet-managed accounts are `Untagged`.

## Double counting

A turn run inside Komet is measured live **and** appears in the agent's own
log. A local turn is dropped when a measured turn has the same input and output
token counts within a small time tolerance (`drop_measured_duplicates`).

## Per-agent table

Filled from `python3 scripts/probe-usage-sources.py` run on the dev machine
(2026-11-14, report: `usage-probe-report.json`) plus targeted read-only
inspection of the files below. No third-party code was consulted.

| Agent | Local log location | Record shape | Names the account? | Token semantics verified | Quota source | Status |
|---|---|---|---|---|---|---|
| Claude | `~/.claude/projects/**.jsonl` (not needed — measured live) | — | — | — | provider quota (exists) | measured only |
| Codex | `~/.codex/sessions/**` (not needed — measured live) | — | — | — | provider quota (exists) | measured only |
| Cursor | — | — | — | — | provider quota (exists) | measured only |
| Antigravity | `~/.gemini/antigravity/brain/<uuid>/.system_generated/logs/transcript*.jsonl` | JSONL transcript | no | not probed for tokens (quota exists) | provider quota (exists) | measured + quota |
| Copilot | `~/.copilot/session-state/<uuid>/events.jsonl` | JSONL events (`data.accountingSnapshot`) | no | input/output present; `reasoning ≤ output` on sampled records | provider billing (exists) | measured + quota |
| Muse | `~/.local/share/muse/sessions/YYYY/MM/DD/<uuid>/session.jsonl` (+ `subagent/*/session.jsonl`) | JSONL envelope `{schema_version,id,stream,sequence,recorded_at(µs),record_type,payload_type,payload}`; events under `payload.event.kind` (`model_request_configured`, `assistant_message_committed`, `resource_usage_sampled`, `model_input_trace_recorded`…) | no | **no token counters anywhere** — `usage` = CPU/RSS/FDS only, `model_input_trace_recorded` carries byte counts; `session-index.db` has session metadata (model, prompt_count) but no tokens | none known | local reader → unavailable ("no token counters") |
| Pi | `~/.pi/agent/sessions/<cwd-slug>/<ISO-ts>_<uuid>.jsonl` | JSONL `{type:"message",id,parentId,timestamp(ISO),message:{role,model,usage:{input,output,cacheRead,cacheWrite,totalTokens,cost{input,output,cacheRead,cacheWrite,total}},timestamp(ms),stopReason}}` | no | `totalTokens = input + output` on sampled records (all values 0 — error turns); `cacheRead`/`cacheWrite` are separate fields, no `reasoning` field; whether `totalTokens` includes the cache fields is **unverified** (no non-zero sample on this machine) | none known | local reader |
| OpenCode | `~/.local/share/opencode/opencode.db` (SQLite; legacy `storage/message|part` JSON files absent on this install) | `part` rows with `data.type=="step-finish"` carry `tokens:{input,output,reasoning,total,cache:{read,write}}` + `cost`; `part.time_created`/`session_id`/`message_id` columns; model via `json_extract(message.data,'$.modelID')` (verified on the live DB — the field is `modelID`, `$.model.id` is always NULL); `session_v2` holds per-session token totals | no | on ~19k non-zero records: `total ≈ input + cache.read + output + reasoning` (not exact — provider total unreliable, do not use); `cache.write ≤ input` always; `reasoning` sometimes > `output` → reasoning is NOT a subset of output here | none known | local reader (sqlite, incremental by rowid) |
| Grok | `~/.grok/sessions/<urlencoded-cwd>/<uuid>/updates.jsonl` | JSONL `{timestamp(s),method:"_x.ai/session/update",params:{sessionId,update:{sessionUpdate:"turn_completed",usage:{inputTokens,outputTokens,totalTokens,cachedReadTokens,cacheCreationTokens,reasoningTokens,modelUsage{<model>:{…}},costUsdTicks,numTurns},_meta:{agentTimestampMs}}}}` | no (`ctx.user_id` exists in `logs/unified.jsonl`, not needed — timeline attributes) | on 13 `turn_completed` records: `totalTokens = inputTokens + outputTokens` (13/13); `reasoningTokens ≤ outputTokens` (subset of output); `cachedReadTokens ≤ inputTokens` (subset of input); `cacheCreationTokens` separate, not in total; models seen: grok-4.6, grok-4.7 | remote (later PR) | local reader |
| Devin | `~/.local/share/devin/cli/transcripts/<name>.json` | JSON transcripts | unknown | not probed for tokens | to investigate | out of scope (remote probe later) |
| Qoder | `~/.qoder/logs/sessions/<cwd>/<uuid>/segments/*.jsonl`, `~/.qoder/logs/runs/<ts>/manifest.json` | JSONL segments | unknown | not probed for tokens | to investigate (last) | out of scope (remote probe later) |
| Cline | `~/.cline/data/sessions/<id>/<id>.json` + `.messages.json` | JSON session docs | no | input/output present on sampled records; semantics not fully verified | none known | measured only (+ "quota not reported") |
| Hermes | `~/.hermes/sessions/request_dump_*.json` | JSON dumps | unknown | not probed for tokens | none known | measured only (+ "quota not reported") |

## Remote quota sources (Grok, Devin, Qoder)

Not started. Each needs its own read-only probe, run on a real signed-in
machine with the account's own credentials, before any code: what endpoint the
vendor's own CLI/app uses, what it returns, and what happens on expiry.
Credentials are read only from the active slot and never written to a log, a
report, or an issue.
