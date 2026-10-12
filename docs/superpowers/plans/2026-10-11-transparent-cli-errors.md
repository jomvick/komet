# Transparent CLI Errors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** CLI-side errors surface in komet typed (ErrorKind), with the CLI's raw text accessible.

**Architecture:** New `ErrorKind` enum + `classify_error(text, code)` in the harness crate; every harness normalizer tags `AgentEvent::Error` with `(kind, detail, retryable)`; the doc fold carries them into `MessagePart::Error`; the transcript renders a collapsible Details row plus one action per kind. All new fields `#[serde(default)]` so old docs re-read.

**Tech Stack:** Rust (serde, serde_json), GPUI transcript rows.

**Spec:** User request 2026-10-11, branch `feat/transparent-cli-errors` (5 points + tests; no transcript redesign beyond Error parts; full diff shown before commit; no push).

## Global Constraints

- `message` field unchanged on both `AgentEvent::Error` and `MessagePart::Error`.
- Every new serde field gets `#[serde(default)]`; old docs must re-read byte-identically (round-trip test).
- No transcript redesign outside Error parts.
- stderr tail in `detail` capped at 4 KiB, for EVERY `Done` errored.
- Tool output summary capped at 160 chars, persisted ONLY when `is_error`.
- Full diff shown before commit. No push.

## Review Focus

- An old doc containing `{"kind":"error","id":"e0","message":"x"}` re-reads with `kind=Unknown`, `detail=None`, `retryable=false`, and re-serializes without the new keys (skip-if-default) — pinned in Task 1.
- A `Done` errored with empty stderr still carries a non-empty `detail` fallback (the error text itself) — pinned in Task 2.
- `classify_error` never panics on empty text / unknown code; returns `(Unknown, false)` — pinned in Task 2.
- A successful tool result never persists an output summary (only `is_error` does) — pinned in Task 4.
- An Error part with `kind=Unknown` and no detail renders the plain chip (no empty Details toggle) — pinned in Task 5.

---

### Task 1: Proto types — ErrorKind + extended Error variants

**Files:**
- Modify: `crates/proto/src/agent.rs:1604-1606` (`AgentEvent::Error`)
- Modify: `crates/doc/src/parts.rs:192-195` (`MessagePart::Error`; note: spec says "crates/proto" but this type lives in `crates/doc` — implement here)
- Modify: `crates/doc/src/schema.rs:161,228,594,921` (DocPartJson error mapping — new fields pass through)
- Test: `crates/doc/src/schema.rs` (round-trip test) or `crates/doc/tests/`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `komet_proto::ErrorKind` — `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]`, `#[serde(rename_all = "camelCase")]`, variants `Auth, Billing, RateLimit, Overloaded, ModelUnavailable, Network, Config, CliCrash, Timeout, Refusal, Truncated, Unknown`; `impl Default -> Unknown`; `impl ErrorKind { pub fn retryable_default(self) -> bool }` (true for RateLimit, Overloaded, Network, Timeout; false otherwise).
  - `AgentEvent::Error { message: String, #[serde(default)] kind: ErrorKind, #[serde(default, skip_serializing_if = "Option::is_none")] detail: Option<String>, #[serde(default)] retryable: bool }`
  - `MessagePart::Error { id: String, message: String, #[serde(default)] kind: ErrorKind, #[serde(default, skip_serializing_if = "Option::is_none")] detail: Option<String>, #[serde(default)] retryable: bool }`
  - Wire names: `AgentEvent::Error.kind` stays `kind` (enum tag is `type`); `MessagePart::Error.kind` serializes as `errorKind` (enum tag already occupies `kind`); `kind` skipped when Unknown, `detail` skipped when None, `retryable` always serialized (old readers ignore unknown fields; old writers omit → defaults).
  - `DocPartJson` gains `errorKind: Option<ErrorKind>, errorDetail: Option<String>, errorRetryable: Option<bool>` (all additive); `to_doc_part` stores kind only when known / retryable only when true so Unknown errors keep byte-identical rows; `from_doc_part` defaults to Unknown/None/false.
  - `AgentEvent::Done` is NOT extended (167 construction sites): stderr detail reaches the transcript via a typed `Error` event emitted before each errored Done (same message text), and the fold's `Done` arm dedupes identical-message pairs (Task 2).
  - Every existing `AgentEvent::Error { message }` construction site must still compile — update them with `..` or explicit defaults (list: `crates/harness/src/mock.rs:192`, `codex/normalize.rs:305`, `codex/mod.rs:984,1266,1476`, `claude/normalize.rs:399,470,540`, `acp/mod.rs:4449`, `engine/src/sessions.rs:337`, tests).

- [ ] **Step 1: Write failing test `old_error_doc_rereads_with_unknown_kind`**

```rust
#[test]
fn old_error_doc_rereads_with_unknown_kind() {
    let part: MessagePart = serde_json::from_str(
        r#"{"kind":"error","id":"e0","message":"boom"}"#,
    ).unwrap();
    assert_eq!(part.kind(), ErrorKind::Unknown); // or matches!(part, MessagePart::Error { kind: ErrorKind::Unknown, detail: None, retryable: false, .. })
    let v = serde_json::to_value(&part).unwrap();
    assert_eq!(v.get("message").unwrap(), "boom");
    assert!(v.get("kind").is_none(), "skip-if-default keeps old bytes");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-doc old_error_doc_rereads_with_unknown_kind`
Expected: FAIL (no `kind` field / accessor).

- [ ] **Step 3: Implement `ErrorKind` in `crates/proto/src/agent.rs`, extend both Error variants, fix all construction sites and `schema.rs` mappings**

`kind` needs `#[serde(default)]` WITHOUT skip (Copy enum, always present in memory; skip-if-default optional — keep it simple: always serialize on new writes is fine since old readers ignore unknown… wait, old APP versions reading NEW docs: unknown fields are ignored by serde by default, so always-serialize is safe; but the round-trip test above asserts `kind` absent — so use `skip_serializing_if = "ErrorKind::is_unknown"` with `pub fn is_unknown(self) -> bool`. Same for `retryable`: `skip_serializing_if = "std::ops::Not::not"` won't work on bool directly — use `skip_serializing_if = "is_false"` helper or just `#[serde(default)]` always-serialize `false`. Decision: `kind` skipped when Unknown; `detail` skipped when None; `retryable` always serialized (a bare `false` is harmless and keeps the test to `kind`/`detail` only). Update the test expectation accordingly: assert `v.get("kind").is_none()` and `v.get("detail").is_none()).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-doc old_error_doc_rereads_with_unknown_kind`
Expected: PASS. Also `cargo test -p komet-proto` PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/proto/src/agent.rs crates/doc/src/parts.rs crates/doc/src/schema.rs
git commit -m "feat(errors): ErrorKind + kind/detail/retryable on Error variants"
```

### Task 2: classify_error + harness wiring + stderr tail

**Files:**
- Create: `crates/harness/src/errors.rs`
- Modify: `crates/harness/src/lib.rs` (module export)
- Modify: `crates/harness/src/claude/normalize.rs:13-27,399,470,540` (assistant_error_text + result.errors → classify)
- Modify: `crates/harness/src/codex/normalize.rs:49,305` (turn.error, "error" items → classify)
- Modify: `crates/harness/src/acp/mod.rs:2724-2744,2358,4449` (stop_outcome, RPC errors, cline_empty_turn_error → classify)
- Modify: `crates/harness/src/claude/mod.rs`, `crates/harness/src/codex/mod.rs`, `crates/harness/src/antigravity/mod.rs` (every `Done { status: Errored }` joins stderr tail ≤4KiB into detail)
- Test: `crates/harness/src/errors.rs` (classification table test)

**Interfaces:**
- Consumes: `komet_proto::ErrorKind`.
- Produces: `pub fn classify_error(text: &str, code: Option<i64>) -> (ErrorKind, bool)` in `komet_harness::errors`. Matching (case-insensitive substring on text, checked in this order):
  - Auth: "authentication_failed", "oauth_org_not_allowed", "sign in", "unauthorized", "401", "invalid api key", "login required"
  - Billing: "billing_error", "insufficient balance", "credits exhausted", "payment", "402"
  - RateLimit: "rate_limit", "rate limit", "429", "too many requests", "limit resets", "usage limit"
  - Overloaded: "overloaded", "capacity", "503", "server_error" (Claude transient 500 → Overloaded? No: `server_error` → Overloaded retryable per Claude semantics — decide: yes, Overloaded)
  - ModelUnavailable: "model_not_found", "model not found", "404", "unknown model"
  - Network: "network", "connection", "econnreset", "etimedout", "socket hang up", "dns", "fetch failed"
  - Config: "invalid_request", "invalid argument", "bad request", "400"
  - CliCrash: "crashed", "killed", "sigkill", "exit code", "panicked"
  - Timeout: "timed out", "timeout", "deadline exceeded", "-32000" (JSON-RPC server timeout used by ACP probes — code -32000 → Timeout ONLY when text also matches timeout-ish? Spec lists -32000 in the table: decide code `-32000 → Timeout`, codes `-32601/-32602 → Config`, `-32700/-32600/-32603 → Unknown`)
  - Refusal: "refused", "refusal", "content policy", "blocked"
  - Truncated: "max_tokens", "max_turn_requests", "max output", "truncated", "hit the maximum"
  - Else Unknown, retryable=false. Retryable defaults from `ErrorKind::retryable_default`, text "`try again`" forces true? No — keep kind default only.
- `pub fn stderr_detail(stderr_tail: &str, fallback: &str) -> String` — last 4096 bytes on char boundary, or fallback when tail empty.

- [ ] **Step 1: Write failing test `classify_error_table`**

```rust
#[test]
fn classify_error_table() {
    let cases: &[(&str, Option<i64>, ErrorKind, bool)] = &[
        ("Cline credits exhausted (Insufficient balance): pick a free model", None, ErrorKind::Billing, false),
        ("authentication_failed", None, ErrorKind::Auth, false),
        ("billing_error", None, ErrorKind::Billing, false),
        ("rate_limit", None, ErrorKind::RateLimit, true),
        ("Claude is overloaded", None, ErrorKind::Overloaded, true),
        ("model_not_found", None, ErrorKind::ModelUnavailable, false),
        ("request timed out after 10s", Some(-32000), ErrorKind::Timeout, true),
        ("The agent refused to continue.", None, ErrorKind::Refusal, false),
        ("hit the maximum output length", None, ErrorKind::Truncated, false),
        ("weird new failure", Some(-32603), ErrorKind::Unknown, false),
        ("", None, ErrorKind::Unknown, false),
    ];
    for (text, code, kind, retryable) in cases {
        assert_eq!(classify_error(text, *code), (*kind, *retryable), "text={text:?}");
    }
}
```

- [ ] **Step 2: Run test, expect FAIL (function not defined)**

Run: `cargo test -p komet-harness errors::`
Expected: FAIL.

- [ ] **Step 3: Implement `errors.rs`, wire all call sites, add stderr tail to every Done-errored**

Each `AgentEvent::Error { message }` site becomes `{ message, kind, detail, retryable }` where `(kind, retryable) = classify_error(text, code)`. At every errored-`Done` site, emit a typed `Error` event FIRST with `detail = Some(stderr_detail(&tail, &msg))` and the SAME message text as `Done.error`; the fold's `Done` arm then dedupes: if the last part is `MessagePart::Error` with the identical message, skip pushing (the detailed part already carries kind + stderr tail). Keep `message` byte-identical to today. Fold dedupe test: `done_errored_after_identical_error_pushes_single_part`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p komet-harness errors::` then `cargo test -p komet-harness`
Expected: PASS (update existing assertions matching `Error { message }` patterns — they still match with `..`).

- [ ] **Step 5: Commit**

```bash
git add crates/harness/src/
git commit -m "feat(errors): classify_error wired through claude/codex/acp + stderr detail"
```

### Task 3: stop_outcome Truncated + empty-turn guard for all harnesses

**Files:**
- Modify: `crates/harness/src/acp/mod.rs:2724-2744` (`stop_outcome`)
- Modify: harness session loops (acp + claude + codex run_session) — empty-turn guard
- Test: `crates/harness/src/acp/mod.rs` (or `crates/harness/tests/`): `max_tokens_emits_nonblocking_truncated`, `empty_turn_errors`

**Interfaces:**
- Consumes: `classify_error`, `AgentEvent::Error`.
- Produces: `stop_outcome` returns `(DoneStatus, Option<String>, Option<AgentEvent>)` — the third element is a NON-BLOCKING `Error { kind: Truncated, .. }` emitted BEFORE the Completed Done when stopReason is `max_tokens`/`max_turn_requests`. All other reasons unchanged. Empty-turn guard: a turn that produced no text, no tool calls, and no error surfaces `Error { kind, .. }` (classified from session/CLI error when available, else Unknown "The agent returned an empty turn.") instead of silent Completed — extended from Cline-only (`cline_empty_turn_error`) to every harness via a shared helper `empty_turn_error(harness_name, cli_hint: Option<&str>) -> AgentEvent`.

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn max_tokens_emits_nonblocking_truncated_error() { /* stop_outcome with stopReason=max_tokens → Completed + Error{Truncated} */ }
#[test]
fn empty_turn_is_never_silent() { /* helper returns Some(Error) for each harness name with None hint */ }
```

- [ ] **Step 2: Run, expect FAIL**

Run: `cargo test -p komet-harness truncated` / `empty_turn`
Expected: FAIL.

- [ ] **Step 3: Implement stop_outcome change + shared empty-turn helper, call it in acp/claude/codex loops**

- [ ] **Step 4: Run tests**

Run: `cargo test -p komet-harness`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/harness/src/
git commit -m "feat(errors): non-blocking Truncated + empty-turn guard for all harnesses"
```

### Task 4: Tool failures carry output text; fold keeps 160-char summary iff is_error

**Files:**
- Modify: `crates/harness/src/claude/normalize.rs`, `crates/harness/src/codex/normalize.rs` (ToolResult error text passthrough)
- Modify: fold (`crates/doc/src/parts.rs` ToolResult arm — locate `summarize_tool_output` call site)
- Test: harness test `failing_tool_keeps_output_text`; doc test `error_tool_result_keeps_160_summary` + `ok_tool_result_keeps_no_summary`

**Interfaces:**
- Consumes: `AgentEvent::ToolResult { output, is_error }`.
- Produces: normalizers set `output` to the CLI's error text verbatim (no cap at this layer); the fold truncates to 160 chars (char boundary + `…`) and persists `output` ONLY when `is_error == true` (success keeps `output: None` as today).

- [ ] **Step 1: Write failing tests** (both crates, exact 160-char expectation with a 200-char input)
- [ ] **Step 2: Run, expect FAIL**
- [ ] **Step 3: Implement passthrough + fold cap**
- [ ] **Step 4: Run `cargo test -p komet-harness -p komet-doc`, expect PASS**
- [ ] **Step 5: Commit** `feat(errors): tool failure text with 160-char error-only summary`

### Task 5: UI — Error part with collapsible Details + per-kind action

**Files:**
- Modify: `crates/ui/src/transcript.rs:890-905` (Error arm), `RowKind::ErrorChip` definition (locate)
- Test: existing UI tests if any cover transcript rows; else manual `cargo check -p komet-ui`

**Interfaces:**
- Consumes: `MessagePart::Error { kind, detail, retryable, message }`.
- Produces: chip shows one-line `message`; when `detail` is non-empty a « Détails » toggle reveals the raw text; exactly one action per kind: Auth → opens Accounts; Billing/ModelUnavailable → opens model selector; RateLimit/Overloaded/Network → Retry (only when `retryable`, else no action); CliCrash → copy logs to clipboard; Timeout/Truncated/Refusal/Config/Unknown → no action (Refusal/Config show Details only). Unknown + no detail renders today's plain chip (Review Focus 5).

- [ ] **Step 1: Implement rendering** (no new test harness exists for GPUI rows — verify via `cargo check -p komet-ui`)
- [ ] **Step 2: Run `cargo check -p komet-ui`, expect success**
- [ ] **Step 3: Commit** `feat(errors): collapsible Details + per-kind Error actions`

### Task 6: Finalisation

- [ ] **Step 1: Run `cargo test -p komet-proto -p komet-doc -p komet-harness`** (NOT the full workspace — heavy builds restart the server; run per-crate)
- [ ] **Step 2: Present the COMPLETE `git diff` to the user BEFORE committing anything remaining**
- [ ] **Step 3: Commit remaining work on `feat/transparent-cli-errors`, no push**

## Self-Review

1. **Spec coverage:** §1→Task 1 (all 12 kinds, `message` unchanged, old docs re-read); §2→Task 2 (all three normalizers + RPC errors + 4Ko stderr on every Done errored); §3→Task 3 (max_tokens/max_turn_requests non-blocking Truncated + guard for all); §4→Task 4 (passthrough + 160 iff is_error); §5 (UI)→Task 5 (Details + one action per kind); Tests→Tasks 1–4 (table with real strings incl. -32000, old-doc reread, empty turn, max_tokens, failing tool). Gaps: none.
2. **Step scan:** each test step carries exact assertions; each code step carries exact signatures/values. The `errors.rs` matching table is the one algorithmic body, given verbatim.
3. **Type consistency:** `classify_error(&str, Option<i64>) -> (ErrorKind, bool)` used identically in Tasks 2–3; `ErrorKind` path `komet_proto::ErrorKind` everywhere; `stderr_detail(&str, &str) -> String` in Task 2 only.
4. **Review Focus:** five lines above, each pinned to its owning task's test.
5. **Proportion:** plan ≈ spec length; bodies only where signature+tests underdetermine (matching table, UI actions).
