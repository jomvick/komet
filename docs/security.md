# Security notes

## Sandbox is not a host boundary

Komet's sandbox settings configure the **agent CLI's own permission and sandbox mechanisms** (Codex `sandbox_mode`/`approvalPolicy`, Claude Code permission modes and settings permissions, OpenCode permission patterns). They are **not** an OS-level isolation layer.

### What the sandbox constrains

- The flags/permissions passed to each agent CLI at spawn time.
- With explicit `SandboxOptions`, komet maps your choices to native provider options instead of escalating to full access.

### What it does NOT constrain

- There is **no container or VM isolation**. Agent processes run directly on the host with your user's privileges.
- Anything the agent CLI can reach despite its own restrictions is still reachable: your files, network, credentials in your environment.
- A misbehaving or compromised CLI that ignores or mishandles its own sandboxing offers no second line of defense from komet.

Run agents on machines/data you accept exposing to them. Use OS-level isolation (containers, VMs) if you need a real boundary.

### Precedence rule (`options wins`)

- If a run request carries `sandbox_options`, it takes precedence over the legacy global `sandbox` level; the legacy field is ignored for that run.
- Without `sandbox_options`, behavior falls back to the legacy `sandbox` level exactly as before — old clients keep working unchanged.
- Yolo/auto-approve mode does not silently override an explicit `SandboxOptions`: granular options stay in effect even when auto-approve is on.

### Fail-fast validation

Explicit `sandbox_options` are validated by the engine (`validate_run_request`) before any process spawns. Invalid combinations (e.g. writable roots outside the workspace without full access, an OpenCode bash pattern table lacking a `"*"` fallback, unknown permissions) fail immediately with a structured error rather than producing a partially-applied configuration at runtime.

### Known limitations

- **Claude Code:** some granular constraints cannot be expressed via command-line invocation alone (e.g. strict `fail_if_unavailable` enforcement); they rely on the CLI honoring its own settings.
  - Filesystem deny rules map as `Edit(path)` (deny write) and `Read(path)` (Paseo `denyRead`, e.g. `~/.ssh`, `~/.aws`); `allow*` entries and `additionalDirectories` merge into `additionalDirectories`.
   - `sandbox.enabled` and `network.strictAllowlist` ARE first-class komet fields now (`ClaudeSandbox.enabled: Option<bool>`, `NetworkSandbox.strict_allowlist: Option<bool>`); `enabled: false` turns the whole Claude sandbox translation off, `strictAllowlist` is forwarded to the settings sandbox `network.strictAllowlist` key.
   - **Filesystem enforcement is Bash-only:** `deny`/`allow`/`deny_read` maps to Claude permission rules `Bash(cmd:*)` / `Edit(path)` / `Read(path)`. The CLI enforces these on tool calls, not as an OS-level filesystem jail. A `Bash` tool invocation that bypasses the permission layer is not blocked by these rules.
   - **A5 sandbox-runtime (`SandboxEnvironment::Srt`) remains opt-in only** for unattended runs — not the default. No container/VM isolation is implied by the settings above.
  - The `settingsPermissions` passthrough merges last over the generated permissions map, so it can override sandbox-derived entries. Escalation-shaped values (a `defaultMode` other than `"default"`/`"acceptEdits"`, or an allow entry of `"*"`, bare `"Bash"`, or `"Bash(*…)"`) are rejected at validation time; other passthrough content is forwarded unvalidated.
- **Codex:** `approval_policy` now expresses every Paseo level (`never`/`untrusted`/`on-request`/`granular`). `web_search` is Paseo's `disabled|cached|indexed|live` enum with bool compatibility (`true`→`live`, `false`→`disabled`); the Codex wire only carries a bool, so anything non-`disabled` maps to `webSearch: true`. `features` is Paseo's object form (`name → true|false|policy object`); the mapper emits the object so `network_proxy`-style policies survive (input arrays of names are accepted for compatibility).
- **OpenCode:** permission tables are applied via an `OPENCODE_CONFIG_CONTENT` overlay: komet generates a minimal `opencode.json` (from `harness::acp::opencode_perms::opencode_config_document`) and passes its CONTENT through the `OPENCODE_CONFIG_CONTENT` env var (final-precedence runtime mechanism), so the user's own config is not touched. The bare per-tool fields `read`, `edit`, `external_directory`, `webfetch`, `websearch` are emitted alongside the `bash` pattern map; a bare field set AND present in `unscoped_actions` wins (no duplicate key). **Merge semantics validated live against opencode CLI 1.18.23** (`opencode debug config`, 2026-08-28): (1) `OPENCODE_CONFIG_CONTENT` wins over a project `opencode.json` for the `"*"` fallback — the old `OPENCODE_CONFIG` file mechanism LOST to project config (a project `allow` overrode the overlay's `ask`, silently weakening it); (2) the merge is deep — project-specific pattern entries (e.g. `git commit: allow`) survive the overlay, so komet's AllowAlways pattern updates still compose with user config. Residual: a live behavioral check that an `ask` pattern actually prompts through the ACP session (config-level precedence is verified; the ACP permission-bridge covers the runtime side and is integration-tested against the harness fixture).
- **Sandbox `ask` (Paseo parity):** when an agent requests a tool permission at runtime (`session/request_permission`), komet surfaces it as a blocking `PermissionRequested` panel and only answers the agent after the user's decision; Deny also interrupts the run. Harnesses without an interactive permission surface (and decision races with a dead run) degrade to **Deny/cancelled — never a silent allow**.
