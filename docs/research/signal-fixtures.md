# Signal fixtures — inventory (2026-10-30)

Inventory for the transparency plan (`docs/superpowers/plans/2026-10-27-agent-transparency.md`,
Task 12 Step 2, families in §4). It records **what proves each observable fact today** and
**what is still missing**; it does not claim a capture that does not exist.

Three evidence levels:

- **L1 — inline unit**: the frame is a `json!` literal inside the normalizer's own `#[cfg(test)]`.
  Proves the mapping, not the process wiring, and not that the CLI ever sends it.
- **L2 — scripted process**: a real child process from `crates/harness/tests/fixtures/`
  (`fake-*.sh|py`) driven by an integration test. Proves the wire, still against a
  hand-written script.
- **L3 — anonymized live capture**: `crates/harness/tests/fixtures/<agent>/live-*.jsonl`
  replayed by an integration test. Proves **that agent, that version**, said this.

Rule kept from the plan: one fixture per **signal family**, not one per agent; a per-agent
golden is added when that agent is verified live, and one agent's evidence never widens the
others (Gate B).

## Compaction

| Agent | Evidence | Level | Gap |
| --- | --- | --- | --- |
| Claude Code | `claude/normalize.rs`: `status_compacting_emits_in_progress`, `compact_boundary_snake_case`, `compact_boundary_camel_case`, `compact_summary_is_not_a_user_turn`, `old_wire_without_compact_fields_still_parses`, `boundary_lands_on_the_open_cycle` | L1 | `fake-claude.sh` never emits `status:compacting` / `compact_boundary`; real frame shape unverified (`slash-commands-inventory.md`) |
| Codex | `codex/normalize.rs`: `thread_compacted_emits_boundary_and_stats`, `compaction_without_params_still_completes` | L1 | `fake-codex.sh` never sends `thread/compacted` |
| Cursor | `cursor/mod.rs`: `compact_frame_records_a_summarize` | L1 | `fake-cursor-shim.sh` never emits `{"ev":"compact"}` |
| Muse | `muse/normalize.rs`: `compaction_items_are_shown`, `compaction_item_reports_failure_and_cancellation`, `compaction_item_carries_what_it_reports` | L1 | `fake-muse.py` never sends a `compaction` item |
| ACP ×8 (Grok, OpenCode, Qoder, Devin, Copilot, Hermes, Pi, Cline) | `fake-acp.sh` scenario `happy` (`compaction_update` → `compaction_summary_chunk` → completed) + `acp/normalize.rs`: `acp_compaction_update_maps_lifecycle_and_chunks`, `compaction_status_is_never_inferred` | **L2** | the only family with process proof; no per-agent capture, so all eight rows stay “not observed” in `acp.md` and the surface stays `None` |
| Antigravity | none | — | no signal observed, declared `None`; needs a long real run |
| Mock | `mock::compaction_script` + `mock_compaction_scenario_is_deterministic` | in-process | — |

## Questions

| Agent | Evidence | Level | Gap |
| --- | --- | --- | --- |
| Claude Code | `fake-claude.sh` `AskUserQuestion` frames + `claude/normalize.rs` description/free-text/multi-select tests | L2 | — |
| ACP ×8 | `fake-acp.sh` `question` (permission-shaped) and `elicit-{single,multi,array,cancel,decline,refused,url}`; `tests/acp_elicitation.rs::gate_b_elicitation_forms_round_trip_and_refusals_are_explicit` | **L2** | strongest family (gated draft RFD exercised, refusals explicit) |
| Muse | `muse/session.rs::questions_parse_with_free_text_ordered_labels_and_selection_mode` | L1 | `fake-muse.py` never sends `request_user_input` |
| Antigravity | `fake-agy.sh` `ask_question` + `tests/antigravity.rs::ask_question_is_a_visible_notice_not_an_unanswerable_panel` | L2 | — (declared `None`: visible notice, no reply channel) |
| Cursor, Codex | none needed | L1 (`surfaces.rs` golden row) | surface `None` is the assertion |

## Plans

| Agent | Evidence | Level | Gap |
| --- | --- | --- | --- |
| Claude Code | `claude/normalize.rs::decodes_typed_tools` (`TodoWrite`) | L1 | no `TodoWrite` in `fake-claude.sh` |
| Codex | `codex/normalize.rs`: `codex_plan_update_maps_to_live_todo`, `plan_update_accepts_alternate_spellings_and_clears`, `child_plan_updates_never_reach_the_root_card` | L1 | `fake-codex.sh` never sends `turn/plan/updated` |
| Cursor | `cursor/mod.rs::decodes_cursor_tool_vocabulary` (`updateTodos`) | L1 | shim fixture never emits `updateTodos` |
| ACP ×8 | `tests/acp.rs::happy_path_maps_chunks_tools_diffs_plans_and_commands`, `acp/normalize.rs::plan_maps_to_stable_todo_chip`, `opencode_todo_tracker_maps_to_todo_chip`, `todowrite_kind_without_raw_maps_to_empty_todo` | **L2** | — |
| Muse | `muse/normalize.rs::tool_names_map_to_typed_calls` (`write_todos` / `todo_write`) | L1 | — |
| Antigravity | `antigravity/normalize.rs` todo tests | L1 | — |
| Mock | none (`mock.rs` emits no `Todo` call) | — | `KOMET_MOCK_SCENARIO=compaction\|subagent_nested` exist, but no plan scenario — the persistent plan card has no demo path yet |

## Tool progress

| Agent | Evidence | Level | Gap |
| --- | --- | --- | --- |
| Codex | `codex/normalize.rs::output_delta_maps_to_ephemeral_tool_progress`; engine `sessions.rs::tool_progress_is_broadcast_but_never_journaled`, `tool_progress_buffer_coalesces_per_tool_and_caps_the_tail`; `tests/subagents.rs::tool_progress_and_activity_are_not_journaled` | L1 + engine | `fake-codex.sh` never streams `outputDelta` |
| ACP ×8 | `acp/normalize.rs::acp_intermediate_tool_content_emits_progress` | L1 | `fake-acp.sh` only sends `status:"completed"` updates (terminal → `ToolResult`), so no scripted non-terminal content proves the ephemeral path end-to-end |
| Muse | `muse/normalize.rs::tools_announce_once_and_end_with_their_output` (`visibleOutput`) | L1 | — |
| Claude Code, Cursor, Antigravity | none needed | L1 (`surfaces.rs`) | declared `None` / `Final` = no partial stdout mapped |

## Sub-agents

| Agent | Evidence | Level | Gap |
| --- | --- | --- | --- |
| Claude Code | `fixtures/claude/live-2.1.228-background-subagent.jsonl` + `tests/claude.rs::captured_live_background_subagent_frames_replay_correctly`, `happy_path_normalizes_events_and_tags_subagents`; 6 normalize tests | **L3** | — |
| Codex | `fixtures/codex/live-0.146.1-multi-agent.jsonl` + `tests/codex.rs::child_thread_routing_tags_and_never_settles_parent` | **L3** | — |
| Cursor | `fake-cursor-shim.sh` + `tests/cursor.rs::happy_path_maps_shim_frames_and_tags_subagents`, `cursor/mod.rs::nested_frames_arrive_tagged`, `task_start_seeds_the_subagent_opening_user_message` | L2 | — |
| ACP ×8 | unit tests only: `acp/subagent.rs`, `acp/subagent_opencode.rs::completion_metadata_yields_the_child_session`, `unstreamed_child_settles_with_fallback_text_and_done` | L1 | **no scripted process fixture** (`fake-acp.sh` has no nested session), no per-agent capture |
| Muse | `muse/normalize.rs::subagents_are_spawn_chips_and_background_work` | L1 | `fake-muse.py` never sends `subagent_spawn` |
| Antigravity | `antigravity/normalize.rs::test_normalize_subagent` (`invoke_subagent` → spawn chip) | L1 | surface `None`: chip only, no transcript expected |
| Mock | `mock::subagent_nested_script` + `mock_subagent_nested_scenario_is_deterministic` | in-process | — |

## Run phase / connection (Task 10)

No fixture yet: `RunPhase`, `Reconnecting`, `Degraded` have no harness- or UI-level test in the
tree at the time of writing (Task 10 Steps 1-7 are open). The engine-side rule (ephemeral, never
journaled) is the same one `tool_progress_and_activity_are_not_journaled` already asserts for
`WatchActivity`.

## Background processes (Task 13 Step 3)

The plan called this **blocked on a fixture**. The fixture already in the tree answers it for
Claude Code; nothing new was run.

| Agent | Evidence in the tree | What it proves | Limit |
| --- | --- | --- | --- |
| Claude Code | `fixtures/claude/live-2.1.228-background-subagent.jsonl`: `Agent` tool call with `run_in_background: true`; `tool_use_result { isAsync: true, status: "async_launched", agentId, outputFile, canReadOutputFile: true }`; `system/background_tasks_changed { tasks: [{ task_id, task_type, description }] }` (and `[]` when idle); `system/task_started { task_id, tool_use_id, task_type: "local_agent"\|"local_bash", subagent_type?, prompt?, owned_by_subagent? }`; `system/task_progress { task_id, usage { total_tokens, tool_uses, duration_ms }, last_tool_name }`; `system/task_updated { task_id, patch { status, end_time } }`; `system/task_notification { task_id, tool_use_id, status, output_file, summary, usage }` | A real lifecycle **and** an output handle: `output_file` for a `local_agent` is its JSONL transcript on the host, and the CLI's own tool list ships `TaskOutput` / `TaskStop` / `TaskGet` / `TaskList`, so “read tail” and “stop” are addressable by `task_id` without inventing a protocol | `local_bash` fires for an ordinary foreground Bash **inside a subagent** (`owned_by_subagent: true`, empty `output_file`), so `task_type: local_bash` alone is *not* a background-process signal — only `run_in_background: true` or a `local_agent` task is. No top-level background-bash capture exists yet |
| Codex | `fixtures/codex/live-0.146.1-multi-agent.jsonl`: every `item/started` `commandExecution` carries `processId`, `source: "unifiedExecStartup"`, `status: "inProgress"` → `item/completed` `"completed"`; `item/commandExecution/outputDelta` already yields live output (Task 7) | A process **id** and a live output stream per command | No background lifecycle observed: every command in the capture starts and completes inside its turn. No `owned_by` / detached equivalent, so a `ProcessStarted` event would be a guess |
| ACP ×8 | none | — | No capture at all; `WatchProcess`/`ProcessPanel` must stay hidden for the family until one exists |
| Cursor / Antigravity / Muse | none | — | Same: no process frames mapped, chip stays hidden |

Consequence for Step 4: Claude can get real `ProcessStarted`/`ProcessExited` from
`background_tasks_changed` + `task_notification` (list is complete, no inference), with output
read from `output_file` and a stop sent through `TaskStop`; Codex has no verified background
surface, so it must **not** emit process events yet; ACP emits none. A harness without a row here
never shows the chip.

## Summary of what is missing

1. **Process-level (L2) compaction for Claude, Codex, Cursor and Muse** — the mappings are only
   asserted on inline JSON, so a wire change would only be caught by the real CLI.
2. **A non-terminal ACP `tool_call_update`** — `fake-acp.sh` only sends `completed` updates, so
   the ephemeral progress path has no scripted process test.
3. **A nested ACP sub-agent fixture** — the only family whose sub-agent path has no scripted
   process test at all.
4. **Per-agent goldens** — L3 exists for Claude and Codex only, and only for sub-agents; there is
   no anonymized capture for compaction, plans or progress on any agent.
5. **Gate B evidence** — 0 of 8 ACP agents probed for `compaction_update` / `elicitation/create`
   (see `acp.md`); those rows stay `None` until a real run is recorded.
6. **Antigravity** — no observed compaction signal; declared `None` rather than invented.
7. **Background processes** — Claude's lifecycle is proven by the existing live capture
   (`background_tasks_changed`, `task_*`, `output_file`), so it can be built without a new run;
   Codex exposes only a `processId` (no detached lifecycle observed) and ACP has no capture at
   all, so neither may emit process events yet.

Fixture files are parsed/replayed, never generated at test time; no golden is synthesized from
an adapter's own output, which would only prove the adapter agrees with itself.
