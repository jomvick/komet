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
  - `sandbox.enabled` and `network.strictAllowlist` are not first-class komet fields — carry them via the `settings.sandbox` passthrough (arbitrary JSON merged last).
  - The `settingsPermissions` passthrough merges last over the generated permissions map, so it can override sandbox-derived entries. Escalation-shaped values (a `defaultMode` other than `"default"`/`"acceptEdits"`, or an allow entry of `"*"`, bare `"Bash"`, or `"Bash(*…)"`) are rejected at validation time; other passthrough content is forwarded unvalidated.
- **Codex:** `approval_policy` now expresses every Paseo level (`never`/`untrusted`/`on-request`/`granular`). `web_search` is a boolean on komet because the Codex sandbox wire takes a boolean; Paseo's richer `disabled|cached|indexed|live` enum is not representable (only on/off). `features` is a reduced name-list (`Vec<String>`), so `network_proxy`/`multi_agent_v2` object forms are not expressible.
- **OpenCode:** permission tables are applied via an `OPENCODE_CONFIG` overlay: komet generates a minimal `opencode.json` (from `harness::acp::opencode_perms::opencode_config_document`) into a per-run temp directory and points `OPENCODE_CONFIG` at it, so the user's own config is not touched. The bare per-tool fields `read`, `edit`, `external_directory`, `webfetch`, `websearch` are emitted alongside the `bash` pattern map; a bare field set AND present in `unscoped_actions` wins (no duplicate key). Residual risk: the overlay's **merge semantics against the user's existing config are unverified against a live OpenCode CLI** — verify once on a real install that a restricted permission table actually takes effect (e.g. a bash `ask` pattern really prompts) and that the overlay does not silently shadow user settings.
- **Sandbox `ask` (Paseo parity):** when an agent requests a tool permission at runtime (`session/request_permission`), komet surfaces it as a blocking `PermissionRequested` panel and only answers the agent after the user's decision; Deny also interrupts the run. Harnesses without an interactive permission surface (and decision races with a dead run) degrade to **Deny/cancelled — never a silent allow**.
