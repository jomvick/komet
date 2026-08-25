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

Explicit `sandbox_options` are validated by the engine (`validate_run_request`) before any process spawns. Invalid combinations (e.g. writable roots outside the workspace without full access, empty OpenCode pattern table without fallback, unknown permissions) fail immediately with a structured error rather than producing a partially-applied configuration at runtime.

### Known limitations

- **Claude Code:** some granular constraints cannot be expressed via command-line invocation alone (e.g. strict `fail_if_unavailable` enforcement); they rely on the CLI honoring its own settings.
- **Codex:** granular approval-policy wire shapes still need validation against live CLI behavior.
- **OpenCode:** permissions are generated as configuration but not automatically applied to running sessions; ACP exposes no permission surface.
