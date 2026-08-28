# Sandbox / Read-Only Access — Improvement Plan

> **Goal:** Strengthen komet's per-agent sandbox/read-only system by studying how Claude Code, Codex, and OpenCode natively manage access control, then adapting the best mechanisms to komet's context. Each agent gets a targeted improvement track.
>
> **Principle:** Don't force a generic sandbox — study what each CLI agent already does well, and wire komet to those native capabilities with komet-specific adaptations (overlay config, fail-fast validation, permission bridge).

**Research sources:** Claude Code docs (`code.claude.com/docs/en/sandboxing`), Codex docs (`developers.openai.com/codex/permissions`, `github.com/openai/codex`), OpenCode docs (`opencode.ai/docs/permissions`, `github.com/anomalyco/opencode/issues/2242`), Paseo parity verification report (`docs/reports/paseo-parity-verification.md`), current `docs/security.md`, `docs/superpowers/plans/2026-08-27-paseo-parity-sandbox.md`.

---

## 1. What each agent does well (the model to learn from)

### Claude Code — layered OS enforcement + permission surface

| Mechanism | Detail |
|---|---|
| **Bash sandbox (OS-level)** | Seatbelt (macOS) / bubblewrap (Linux+WSL2) wraps every Bash child process. Writes confined to CWD + `$TMPDIR`; network blocked to unapproved domains. |
| **Two independent layers** | Filesystem isolation (paths) + network isolation (domains). Can run with FS off but network on, or both. |
| **Read policy** | Entire disk readable *by default* (credentials exposed unless `sandbox.credentials.files` or `denyRead` blocks them). |
| **Protected paths** | `.claude/settings*`, `.claude/hooks`, `.mcp.json`, `.git/hooks`, `.git/config`, shell startup files — never writable, symlink-aware since v2.1.210. |
| **Permission modes** | `default`, `acceptEdits`, `plan`, `auto`, `dontAsk`, `bypassPermissions` — decides whether a tool call runs and whether the user is prompted. |
| **Read-only Bash commands** | Built-in set (`ls`, `cat`, `echo`, `pwd`, `head`, `tail`, `grep`, `find`, `wc`, `which`, `diff`, `stat`, `du`, `cd`, read-only `git`) runs without prompt in every mode. |
| **Content-scoped ask rules** | `Bash(git push *)` forces prompt even in sandbox; bare `Bash(*)` skipped for sandboxed commands but applies to fallback. |
| **Auto-allow vs auto mode** | `/sandbox` auto-allow approves Bash commands *because* the boundary contains them. Auto mode uses a classifier. Independent, composable. |
| **Escape hatch** | `dangerouslyDisableSandbox` drops back to regular flow; `allowUnsandboxedCommands: false` makes blocked commands fail outright. |
| **Subagents** | Inherit parent's sandbox config. |
| **Docker sandbox** | `sbx run claude` — full process isolation, only project-level config, `--dangerously-skip-permissions` default, `--clone` for agent teams. |
| **Sandbox runtime** | `@anthropic-ai/sandbox-runtime` wraps the *whole* Claude Code process (not just Bash) — same primitives, no Docker needed. |

### Codex — profile-based least privilege with progressive disclosure

| Mechanism | Detail |
|---|---|
| **Three built-in profiles** | `:read-only` (read anywhere, no writes), `:workspace` (writes in workspace roots + temp, `.git/.agents/.codex` read-only), `:danger-full-access` (no restrictions). |
| **Custom profiles** | `extends` inheritance, filesystem entries (`read`/`write`/`deny`), network rules, `workspace_roots`. `deny` wins over `write` wins over `read`. |
| **Progressive disclosure** | Default `:minimal` platform-readable roots when `read-only`; widens to full platform defaults only when `ReadOnlyAccess::FullAccess`. `:root` only when intentional. |
| **Approval policies** | `untrusted`, `on-request`, `never` — independent of sandbox mode. `never` = no prompts; `untrusted` = only safe reads auto; `on-request` = ask for risky actions. |
| **Default conservative** | Starts read-only until directory trusted. Version-controlled folders → Auto (`workspace-write` + `on-request`). Non-versioned → read-only. |
| **OS primitives** | Seatbelt (macOS), Landlock + seccomp (Linux). Windows: AppContainer (experimental). |
| **Container caveat** | In Docker, Landlock/seccomp may be unavailable → use `--sandbox danger-full-access` inside a properly configured container. |
| **Split filesystem policy** | Modern `FileSystemSandboxPolicy` with `Restricted`/`Unrestricted`/`ExternalSandbox` kinds. Resolves per-CWD with precedence. |
| **New `ReadOnlyAccess::Restricted`** | Configurable readable roots + `include_platform_defaults` flag. Fail-closed on unsupported backends. |
| **Shell environment policy** | `inherit`/`exclude` for env vars (e.g., mask `*_KEY`, `*_TOKEN`, `*_SECRET`). |
| **Python SDK presets** | `Sandbox.read_only`, `Sandbox.workspace_write`, `Sandbox.full_access` — named presets on `thread_start(sandbox=...)`. |

### OpenCode — permission rules + emerging OS sandbox

| Mechanism | Detail |
|---|---|
| **Permission rules** | Ordered array of `{action, resource, effect}` where effect = `allow`/`ask`/`deny`. Last matching rule wins. Per-agent overrides. |
| **Actions** | `read`, `edit`, `glob`, `grep`, `bash`, `task`, `skill`, `lsp`, `question`, `webfetch`, `websearch`, `external_directory`, `doom_loop`. |
| **Default policy** | Permissive: most `allow`, `external_directory` + `doom_loop` default to `ask`, `.env` denied. |
| **External directory boundary** | `external_directory` controls paths outside the workspace; tools that touch external paths need a separate decision. |
| **Per-agent permissions** | `agents.<id>.permissions` merges with global; agent rules take precedence. Subagents run with own permissions. |
| **Experimental sandbox (macOS only)** | Seatbelt via `sandbox-exec`. `bash:unsandboxed` permission retries outside sandbox. Does NOT sandbox MCP servers or PTY shells. |
| **Community solutions** | `opencode-sandbox-plugin` wraps `bash` with `@anthropic-ai/sandbox-runtime` (filesystem + network + sensitive file protection). `nono` CLI wraps any agent in Landlock/Seatbelt. Bubblewrap scripts documented in issues. |
| **Overlay via `OPENCODE_CONFIG_CONTENT`** | Komet generates a minimal `opencode.json` overlay, passes content via env var — validated live against CLI 1.18.23. |
| **`readOnlyPaths`/`writeablePaths`/`allowedExecutables`** | Proposed in issue #4667 — per-agent security context with bubblewrap. Not yet implemented in OpenCode core. |

---

## 2. Current komet state (post-Paseo parity)

### What's already implemented ✅

- **Proto schemas** — `CodexSandbox`, `ClaudeSandbox`, `OpenCodePerms` with `deny_unknown_fields` strict validation
- **`SandboxOptions`** — provider-native options riding `RunRequest`; `from_level(SandboxLevel)` for `ReadOnly`/`WorkspaceWrite`/`DangerFullAccess`
- **`validate_run_request`** — fail-fast: rejects unknown fields, `ProviderOptionsRejected` for non-sandbox providers with non-empty options
- **`options wins`** — `sandbox_options` overrides legacy `sandbox` level; yolo doesn't clobber explicit options
- **Codex mapping** — `sandbox_mode`, `sandbox_workspace_write.exclude_*`, `networkAccess`, `writableRoots`, `features` (object form), `approval_policy` (`never`/`untrusted`/`on-request`/`granular`), `web_search` enum (`disabled`/`cached`/`indexed`/`live`)
- **Claude mapping** — `excludedCommands`→`Bash()`, `deny`/`deny_write`→`Edit()`, `deny_read`→`Read()`, `additionalDirectories`, `allowed/disallowedTools`, `network.allowedHosts/deniedHosts`, `settingsPermissions`, `enabled`, `strictAllowlist`
- **OpenCode overlay** — `OPENCODE_CONFIG_CONTENT` env var; `opencode_config_document` emits `read`/`edit`/`external_directory`/`webfetch`/`websearch` + `bash` pattern map; merge semantics validated live
- **Permission bridge** — `session/request_permission` → blocking `PermissionRequested` panel; `Deny` → interrupt; `Allow` → selected; non-interactive harnesses degrade to Deny (never silent allow)
- **Tests** — `sandbox_validation.rs`, `permission_flow.rs`, round-trip tests for all new fields

### What's missing or weak 🟡 / 🔴

| Gap | Agent | Severity |
|---|---|---|
| No OS-level sandbox for any agent | All | 🔴 Komet documents "sandbox is not a host boundary" — no container/VM isolation layer exists |
| Bash tool not sandboxed natively | OpenCode | 🔴 OpenCode's experimental Seatbelt sandbox is macOS-only and not wired through komet |
| `readOnlyPaths`/`writeablePaths`/`allowedExecutables` concept | OpenCode | 🟡 Proposed in upstream issue #4667 but not implemented — komet could pioneer this |
| No `sandbox.credentials` equivalent | Claude | 🟡 Claude Code protects `~/.ssh`, `~/.aws/credentials` via `sandbox.credentials.files` — komet has no equivalent concept |
| No env var masking | Codex | 🟡 Codex has `shell_environment_policy.exclude` for `*_KEY`, `*_TOKEN`, `*_SECRET` — komet doesn't mask env vars |
| No Landlock/seccomp fallback | Codex | 🟡 Codex detects kernel support and falls back; komet has no detection or fallback strategy |
| No Docker sandbox integration | All | 🟡 All three agents support Docker sandboxes (`sbx run`, container `--sandbox`, `nono`) — komet doesn't offer this |
| No `dangerouslyDisableSandbox: false` guard | Claude | 🟡 Claude can be configured to fail rather than fall back; komet has no equivalent |
| Permission rules don't cover MCP servers | All | 🟡 MCP servers run outside any sandbox — a known gap across all three agents |
| No `allowManagedDomainsOnly` enforcement | Claude | 🟡 Claude can lock network to managed domains; komet's network isolation is simpler |
| OpenCode upstream sandbox not adopted | OpenCode | 🟡 Upstream `opencode-sandbox-plugin` exists — komet should evaluate integrating rather than maintaining its own overlay-only approach |
| No runtime behavioral check of permission prompts | OpenCode | 🟡 Komet verified config-level precedence but hasn't validated that `ask` patterns actually prompt through the ACP session |

---

## 3. Improvement plan — per-agent tracks

### Track A: Claude Code — tighten the native sandbox model

**Rationale:** Claude Code has the most mature OS-level sandbox (Seatbelt/bubblewrap). Komet should map to it more completely and add the missing guards.

#### A1. Implement `sandbox.credentials` mapping
- **What:** Add `ClaudeSandbox.credentials` field (`Vec<String>`) mapping to `sandbox.credentials.files` in Claude settings
- **Why:** Currently `~/.ssh`, `~/.aws/credentials` are readable by sandboxed commands. Claude's built-in `sandbox.credentials.files` denies these explicitly.
- **Action:** Add field to proto `ClaudeSandbox`, map to `sandbox.credentials.files` in settings generation, add `denyRead` rules for listed paths
- **Files:** `crates/proto/src/agent.rs`, `crates/harness/src/claude/mod.rs`, `crates/harness/src/claude/wire.rs`
- **Test:** Verify `~/.ssh` and `~/.aws/credentials` appear in generated settings when `credentials` is set

#### A2. Wire `fail_if_unavailable` → `allowUnsandboxedCommands: false`
- **What:** When `ClaudeSandbox.fail_if_unavailable: true`, ensure the generated settings include `allowUnsandboxedCommands: false` so commands that can't be sandboxed *fail* rather than falling back to the regular permission flow
- **Why:** Currently komet sets `allow_unsandboxed_commands: false` only in `ReadOnly` mode. It should be the default when sandboxing is active.
- **Action:** Update `ClaudeSandbox` mapper to always emit `allowUnsandboxedCommands: false` unless explicitly `false`
- **Files:** `crates/harness/src/claude/wire.rs`

#### A3. Add `sandbox.enabled` boolean mapping (already done, verify)
- **Status:** `claude.enabled` field added — when `false`, turns off the whole Claude sandbox translation
- **Verify:** Test that `enabled: false` produces no `--permission-mode` flag and no settings sandbox block

#### A4. Add `sandbox.network.strictAllowlist` mapping (already done, verify)
- **Status:** `NetworkSandbox.strict_allowlist` added — maps to `sandbox.network.strictAllowlist`
- **Verify:** Test that `strictAllowlist: true` emits correctly and `allowedDomains` is enforced

#### A5. Evaluate Docker sandbox integration
- **What:** Add a `sandbox_environment` option to `RunRequest` — when set, komet wraps the agent process in a Docker container (matching Claude's `sbx run` pattern)
- **Why:** Claude's Docker sandbox provides full process isolation, not just Bash. Komet should offer this as an opt-in hardening layer.
- **Action:** Design `SandboxEnvironment` enum (`None`, `Docker`, `Vm`), add to proto, implement Docker spawn wrapper in engine
- **Files:** `crates/proto/src/agent.rs`, `crates/engine/src/sessions.rs`, new `crates/harness/src/sandbox_env.rs`
- **Priority:** Medium — requires Docker daemon dependency

#### A6. Protect Claude settings files at the komet level
- **What:** Add `denyWrite` entries for `.claude/settings.json`, `.claude/settings.local.json`, `.claude/hooks`, `.mcp.json` in the generated Claude settings — mirroring Claude's built-in protected paths
- **Why:** Even with sandbox enabled, komet should ensure these paths are explicitly denied in the generated config as defense-in-depth
- **Action:** Add to `ClaudeSandbox::from_level` default deny list
- **Files:** `crates/proto/src/agent.rs`

---

### Track B: Codex — adopt the profile-based model

**Rationale:** Codex's permission profiles (`:read-only`, `:workspace`, `:danger-full-access`) with inheritance and `extends` are the cleanest model. Komet should adopt this profile pattern and add the missing `Restricted` read access.

#### B1. Add `CodexSandbox` `read_only_access` mapping to `ReadOnlyAccess::Restricted`
- **What:** Map komet's `CodexSandbox` to Codex's `ReadOnlyAccess::Restricted { include_platform_defaults, readable_roots }` when in read-only mode
- **Why:** Currently komet maps `ReadOnly` to `SandboxMode::ReadOnly` + `ApprovalPolicy::Never` but doesn't express the granular `ReadOnlyAccess::Restricted` form that limits reads to current directory + minimal platform roots
- **Action:** Add `read_only_access` field to `CodexSandbox` proto struct, mapper emits `ReadOnlyAccess::Restricted` when appropriate
- **Files:** `crates/proto/src/agent.rs`, `crates/harness/src/codex/mod.rs`, `crates/harness/src/codex/normalize.rs`
- **Test:** Verify `read_only_access.restricted` produces the restricted profile in the wire request

#### B2. Implement env var masking (`shell_environment_policy`)
- **What:** Add `CodexSandbox.env_exclude` field (`Vec<String>`) mapping to Codex's `[shell_environment_policy] exclude` — masks `*_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, `AWS_*`
- **Why:** Codex supports env var exclusion to prevent credential leakage. Komet should expose this.
- **Action:** Add field to proto, mapper emits `[shell_environment_policy] exclude = [...]` in codex config
- **Files:** `crates/proto/src/agent.rs`, `crates/harness/src/codex/mod.rs`

#### B3. Add `CodexSandbox.workspace_roots` with protected subpaths
- **What:** Ensure `.git`, `.agents`, `.codex` are automatically protected as read-only under each writable root (matching Codex's `default_read_only_subpaths_for_writable_root`)
- **Why:** Codex automatically protects these paths. Komet should too.
- **Action:** Add `default_read_only_subpaths` function in `codex/normalize.rs`, apply when `workspace_roots` are set
- **Files:** `crates/harness/src/codex/normalize.rs`

#### B4. Evaluate Landlock/seccomp detection and fallback
- **What:** Add detection of Landlock/seccomp kernel support; if unavailable, warn or fall back to `danger-full-access` with a clear message
- **Why:** Codex checks kernel support and handles the case gracefully. Komet should too, especially for Linux containers.
- **Action:** Add `check_sandbox_backend()` function, integrate into harness spawn logic
- **Files:** `crates/harness/src/codex/mod.rs`, new `crates/harness/src/sandbox_detect.rs`
- **Priority:** Low — informational, not blocking

#### B5. Add Codex Docker sandbox mode
- **What:** When running in a Docker container, komet should detect it and adjust the sandbox policy (Codex docs say to use `--sandbox danger-full-access` inside a properly configured container)
- **Why:** Container environments may not expose Landlock/seccomp
- **Action:** Detect container environment, adjust `sandbox_mode` accordingly, document the requirement
- **Files:** `crates/harness/src/codex/mod.rs`

---

### Track C: OpenCode — integrate upstream sandbox plugin + harden permissions

**Rationale:** OpenCode is the weakest sandbox story. Komet should adopt the upstream `opencode-sandbox-plugin` approach and add the `readOnlyPaths`/`writeablePaths`/`allowedExecutables` concept that the community proposed.

#### C1. Integrate `@anthropic-ai/sandbox-runtime` as optional OpenCode sandbox backend
- **What:** Instead of (or in addition to) the `OPENCODE_CONFIG_CONTENT` overlay, offer an optional `opencode_sandbox_runtime` mode that wraps bash commands with the sandbox-runtime plugin
- **Why:** The overlay-only approach configures permissions but doesn't enforce OS-level isolation. The upstream plugin provides real filesystem/network isolation.
- **Action:** Add `OpenCodeSandboxRuntime { enabled: bool, config: OpenCodeSandboxConfig }` to proto, in `opencode_perms.rs` add a path that generates the plugin config + wraps bash commands
- **Files:** `crates/proto/src/agent.rs`, `crates/harness/src/acp/opencode_perms.rs`, `crates/harness/src/acp/mod.rs`
- **Priority:** High — this closes the biggest gap for OpenCode

#### C2. Implement `readOnlyPaths`/`writeablePaths`/`allowedExecutables` model
- **What:** Add a komet-native security context model for OpenCode agents that maps to bubblewrap constraints (following issue #4667's proposal)
- **Why:** OpenCode doesn't have this natively yet. Komet can define the data model and generate the bubblewrap config.
- **Action:** Define `AgentSandboxContext { read_only_paths: Vec<PathBuf>, writeable_paths: Vec<PathBuf>, allowed_executables: Vec<String> }`, generate bubblewrap wrapper script
- **Files:** `crates/proto/src/agent.rs`, new `crates/harness/src/acp/bubblewrap.rs`, `crates/harness/src/acp/mod.rs`
- **Test:** Verify bubblewrap script generation, validate paths, test with `bwrap --dry-run`

#### C3. Add sensitive file read protection for OpenCode
- **What:** Generate OpenCode permission rules that deny `read` on `~/.ssh`, `~/.aws/credentials`, `~/.config/gcloud`, `~/.npmrc`, `.env` by default (matching the upstream plugin's defaults)
- **Why:** Currently OpenCode's `read` defaults to `allow` — sensitive files are readable
- **Action:** Add `sensitive_file_deny` default list to `OpenCodePerms`, merge into `opencode_config_document`
- **Files:** `crates/proto/src/agent.rs`, `crates/harness/src/acp/opencode_perms.rs`

#### C4. Add OpenCode upstream sandbox plugin compatibility layer
- **What:** Add a `use_sandbox_plugin` flag that generates `.opencode/sandbox.json` compatible with the `kszarek/opencode-sandbox-plugin` format
- **Why:** If users already have the plugin installed, komet should generate compatible config rather than its own overlay
- **Action:** Add flag to `OpenCodePerms`, generate `sandbox.json` alongside the overlay
- **Files:** `crates/harness/src/acp/opencode_perms.rs`

#### C5. Add `bash:unsandboxed` permission for OpenCode
- **What:** When the sandbox is active, add a `bash:unsandboxed` permission rule that allows specific commands to run outside the sandbox (matching OpenCode's built-in escape hatch)
- **Why:** Some commands can't run sandboxed — users need an escape hatch
- **Action:** Add `unsandboxed: Vec<String>` to `OpenCodePerms`, generate corresponding permission rule
- **Files:** `crates/proto/src/agent.rs`, `crates/harness/src/acp/opencode_perms.rs`

---

### Track D: Cross-cutting improvements

#### D1. OS-level isolation layer (Docker/VM)
- **What:** Add a `SandboxEnvironment` type to the engine that can wrap *any* agent spawn in a container or VM
- **Design:** `SandboxEnvironment { kind: SandboxEnvKind, config: SandboxEnvConfig }` where `kind` is `None | Docker | SocketVM` (SocketVM is a lightweight microVM)
- **Why:** Komet's security.md explicitly states "sandbox is not a host boundary." This track provides the actual host boundary.
- **Action:**
  1. Add `SandboxEnvironment` to `RunRequest` (optional, additive)
  2. Implement Docker wrapper in engine (`crates/engine/src/sandbox_env.rs`)
  3. For SocketVM, evaluate `socketvm` or similar lightweight microVM
  4. Integrate with harness spawn logic — the harness receives the sandbox env and wraps the process
  5. Document limitations (GPU, networking, filesystem mounts)
- **Files:** `crates/proto/src/agent.rs`, `crates/engine/src/sessions.rs`, `crates/engine/src/sandbox_env.rs`, all harness `mod.rs`
- **Priority:** High — this is the single biggest security improvement

#### D2. Unified sandbox validation with backend capability detection
- **What:** Add a `SandboxBackendCapability` check that validates whether the target platform supports the requested sandbox type before accepting the run request
- **Why:** If a user requests `SandboxEnvironment::Docker` but Docker isn't available, or requests `SandboxMode::ReadOnly` with Landlock on a kernel that doesn't support it, fail fast with a clear error
- **Action:** Add `check_sandbox_capabilities()` to `validate_run_request`, return `ValidationError::SandboxBackendUnavailable` with details
- **Files:** `crates/proto/src/agent.rs` (`validate_run_request`), new `crates/engine/src/sandbox_detect.rs`

#### D3. MCP server sandbox awareness
- **What:** Document and implement a pattern where MCP servers run in a restricted context (or are denied when sandbox is active)
- **Why:** All three agents have this gap — MCP servers run outside the sandbox. Komet should at least make MCP servers opt-in when sandboxing is active
- **Action:** Add `mcp_sandbox_behavior` option (`allow`/`deny`/`restrict`), document the limitation in `security.md`
- **Files:** `crates/proto/src/agent.rs`, `docs/security.md`

#### D4. Permission bridge hardening
- **What:** Add `request_permission` timeout — if the user doesn't respond within N seconds, the permission is auto-denied
- **Why:** Prevent hangs when the user is unavailable and the agent is waiting for a permission decision
- **Action:** Add `permission_timeout_ms` to `RunRequest`, implement timeout in `sessions.rs`
- **Files:** `crates/proto/src/agent.rs`, `crates/engine/src/sessions.rs`
- **Test:** Verify timeout triggers Deny + interrupt

#### D5. Audit logging of sandbox decisions
- **What:** Log every sandbox-related decision (what was configured, what was validated, what was denied) to the run journal
- **Why:** For security auditing and debugging — when something goes wrong, you need to know what sandbox policy was actually applied
- **Action:** Add `SandboxAuditLog` struct, emit events at key points in the harness pipeline
- **Files:** `crates/engine/src/sessions.rs`, new `crates/engine/src/sandbox_audit.rs`

---

## 4. Implementation priority

### Phase 1: Critical gaps (week 1-2)
| # | Track | Task | Effort |
|---|---|---|---|
| D1 | Cross | OS-level isolation layer (Docker) | High |
| A1 | Claude | `sandbox.credentials` mapping | Medium |
| B1 | Codex | `ReadOnlyAccess::Restricted` mapping | Medium |
| C1 | OpenCode | `@anthropic-ai/sandbox-runtime` integration | High |

### Phase 2: Hardening (week 3-4)
| # | Track | Task | Effort |
|---|---|---|---|
| A2 | Claude | `allowUnsandboxedCommands: false` default | Low |
| A4 | Claude | `strictAllowlist` verify | Low |
| B2 | Codex | Env var masking | Medium |
| B3 | Codex | Protected subpaths | Medium |
| C2 | OpenCode | `readOnlyPaths`/`writeablePaths` model | High |
| C3 | OpenCode | Sensitive file deny defaults | Low |
| D2 | Cross | Unified sandbox capability detection | Medium |

### Phase 3: Polish (week 5-6)
| # | Track | Task | Effort |
|---|---|---|---|
| A5 | Claude | Docker sandbox integration | Medium |
| B4 | Codex | Landlock/seccomp detection | Medium |
| C4 | OpenCode | Upstream plugin compatibility | Medium |
| C5 | OpenCode | `bash:unsandboxed` permission | Low |
| D3 | Cross | MCP server sandbox awareness | Medium |
| D4 | Cross | Permission bridge timeout | Low |
| D5 | Cross | Audit logging | Medium |

---

## 5. Validation strategy

### For each track:
1. **Unit tests** — Verify proto serialization/deserialization of new fields
2. **Wire tests** — Verify the generated CLI flags/config match the target agent's expected format
3. **Integration tests** — Spawn the agent with the generated config, verify sandbox behavior (e.g., attempt to read `~/.ssh` → denied, attempt to write outside workspace → denied)
4. **Round-trip tests** — Verify that round-tripping through the proto preserves all sandbox semantics

### Cross-agent validation:
- Run `cargo test --no-fail-fast -p komet-proto -p komet-harness -p komet-engine` after each phase
- Validate against actual agent CLI versions (Claude Code v2.1.x, Codex 0.146.1+, OpenCode 1.18.23+)
- Run the bubblewrap script in dry-run mode to validate constraints

---

## 6. Architecture decisions

### What we're NOT doing
- **Not building a generic sandbox** — we leverage each agent's native sandbox, just map more completely
- **Not changing the permission bridge model** — `PermissionRequested` → user decision → `Allow`/`Deny` is proven
- **Not removing `SandboxLevel`** — the three-level model (`ReadOnly`/`WorkspaceWrite`/`DangerFullAccess`) stays as the high-level API; `SandboxOptions` provides the granular provider-native overrides
- **Not enforcing sandbox at the engine level for non-sandboxable agents** — Cursor, Grok, Hermes, Pi, Antigravity still reject non-empty options; that's correct

### What we ARE doing
- **Leveraging each agent's native OS primitives** (Seatbelt, Landlock, bubblewrap) rather than building our own
- **Adding the missing fields** that Paseo defines and that the agents support natively
- **Providing OS-level isolation as an option** (Docker/VM) for users who need a real boundary
- **Failing fast** on invalid configurations — no silent degradation

---

## 7. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Docker dependency adds complexity | Make `SandboxEnvironment::Docker` optional; document the dependency; provide a `none` default |
| Upstream plugin changes break compatibility | Pin the plugin version in komet's adapter installation; test against the pinned version |
| Bubblewrap script generation is fragile | Generate scripts with validated templates; test with `bwrap --dry-run`; use the approach from issue #4667 comments |
| New fields break existing clients | All new fields are `Option<T>` with `skip_serializing_if = "Option::is_none"`; backward compatible |
| OS-level sandbox reduces functionality | Document what's restricted; provide escape hatches (`dangerouslyDisableSandbox`, `bash:unsandboxed`); test with real agent workflows |
