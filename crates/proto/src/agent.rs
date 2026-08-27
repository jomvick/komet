//! Agent-side wire types: harness identity, run requests, streaming events, tool calls.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessId {
    ClaudeCode,
    Codex,
    Cursor,
    /// xAI's Grok Build agent, driven over ACP (`grok agent stdio`).
    Grok,
    /// Nous Research's Hermes Agent, driven over ACP (`hermes acp`).
    Hermes,
    /// The pi coding agent (pi.dev), driven over ACP via the `pi-acp` adapter.
    Pi,
    /// SST's opencode agent, driven over ACP (`opencode acp`).
    #[serde(alias = "open-code")]
    Opencode,
    /// Test harness; never shown in production pickers.
    Mock,
    Antigravity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    /// xhigh + harness-specific setting.
    Ultracode,
    /// Prompt-prefix driven (Claude).
    Ultrathink,
}

impl ReasoningLevel {
    /// Map a UI thinkingOptionId onto the level ladder. Unknown ids return
    /// `None` — the caller decides whether to drop or downgrade.
    pub fn from_thinking_id(id: &str) -> Option<Self> {
        match id {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            "ultracode" => Some(Self::Ultracode),
            "ultrathink" => Some(Self::Ultrathink),
            _ => None,
        }
    }

    /// The canonical thinkingOptionId for this level (serde spelling).
    pub fn as_thinking_id(&self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::Ultracode => "ultracode",
            Self::Ultrathink => "ultrathink",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLevel {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

use std::path::PathBuf;

/// Provider-native sandbox options riding [`RunRequest`] — one slot per
/// harness family, mirroring Paseo's provider-options table. Exactly the
/// fields each harness natively understands; strict (`deny_unknown_fields`)
/// so a typo'd option fails at the wire instead of silently no-op'ing on
/// the agent side.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexSandbox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeSandbox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode: Option<OpenCodePerms>,
}

impl SandboxOptions {
    pub fn from_level(level: SandboxLevel) -> Self {
        match level {
            SandboxLevel::ReadOnly => Self {
                codex: Some(CodexSandbox {
                    sandbox_mode: Some(SandboxMode::ReadOnly),
                    approval_policy: Some(ApprovalPolicy::Never),
                    ..Default::default()
                }),
                claude: Some(ClaudeSandbox {
                    filesystem: FilesystemSandbox {
                        allow: vec![],
                        deny: vec!["/".into()],
                        allow_read: vec![],
                        deny_read: vec![],
                        allow_write: vec![],
                        deny_write: vec!["/".into()],
                    },
                    allow_unsandboxed_commands: false,
                    fail_if_unavailable: true,
                    ..Default::default()
                }),
                // Opencode: ReadOnly = everything asks/denies, never ambient.
                // Kept as `ask` fallback so the future Permission flow can
                // surface, but no write is auto-allowed.
                opencode: Some(OpenCodePerms {
                    bash: BashPerms {
                        patterns: vec![("*".into(), Perm::Deny)],
                    },
                    unscoped_actions: [
                        ("webfetch".into(), Perm::Deny),
                        ("websearch".into(), Perm::Deny),
                        ("todowrite".into(), Perm::Deny),
                    ]
                    .into(),
                    ..Default::default()
                }),
            },
            SandboxLevel::WorkspaceWrite => Self {
                codex: Some(CodexSandbox {
                    sandbox_mode: Some(SandboxMode::WorkspaceWrite),
                    ..Default::default()
                }),
                claude: Some(ClaudeSandbox::default()),
                opencode: Some(OpenCodePerms {
                    bash: BashPerms {
                        patterns: vec![("*".into(), Perm::Ask)],
                    },
                    ..Default::default()
                }),
            },
            SandboxLevel::DangerFullAccess => Self {
                codex: Some(CodexSandbox {
                    sandbox_mode: Some(SandboxMode::DangerFullAccess),
                    network_access: true,
                    approval_policy: Some(ApprovalPolicy::Never),
                    ..Default::default()
                }),
                claude: Some(ClaudeSandbox {
                    allow_unsandboxed_commands: true,
                    ..Default::default()
                }),
                opencode: Some(OpenCodePerms {
                    bash: BashPerms {
                        patterns: vec![("*".into(), Perm::Allow)],
                    },
                    unscoped_actions: [("webfetch".into(), Perm::Allow)].into(),
                    ..Default::default()
                }),
            },
        }
    }
}

/// Codex sandbox table (Paseo `codex` provider options). Note Paseo's
/// semantics: `approval_policy: "never"` only removes prompts — access is
/// governed by `sandbox_mode`, never by the approval policy.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexSandboxWorkspaceWrite {
    #[serde(default)]
    pub exclude_slash_tmp: bool,
    #[serde(default)]
    pub exclude_tmpdir_env_var: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexSandbox {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<SandboxMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<PathBuf>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_workspace_write: Option<CodexSandboxWorkspaceWrite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// Codex approval policy: either a blanket level (`"never"` on the wire) or a
/// granular split of commands to always-ask vs auto-approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// `"never"` removes prompts only; [`SandboxMode`] still gates access.
    Never,
    Granular {
        ask: Vec<String>,
        auto_approve: Vec<String>,
    },
}

// Hand-rolled because the wire mixes representations: a bare `"never"`
// string for the blanket level, a tagged object for the granular split.
impl Serialize for ApprovalPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Never => serializer.serialize_str("never"),
            Self::Granular { ask, auto_approve } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "granular")?;
                if !ask.is_empty() {
                    map.serialize_entry("ask", ask)?;
                }
                if !auto_approve.is_empty() {
                    map.serialize_entry("autoApprove", auto_approve)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ApprovalPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) if s == "never" => Ok(Self::Never),
            serde_json::Value::Object(map)
                if map.get("kind").and_then(|k| k.as_str()) == Some("granular") =>
            {
                let get_list = |key: &str| -> Vec<String> {
                    map.get(key)
                        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
                        .unwrap_or_default()
                };
                Ok(Self::Granular {
                    ask: get_list("ask"),
                    auto_approve: get_list("autoApprove"),
                })
            }
            _ => Err(serde::de::Error::custom(
                "approval policy must be \"never\" or {\"kind\":\"granular\",…}",
            )),
        }
    }
}

/// Claude Code sandbox settings (Paseo `claude` provider options), mirroring
/// Claude's `settings.permissions` surface.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSandboxSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSandbox {
    #[serde(default)]
    pub filesystem: FilesystemSandbox,
    #[serde(default)]
    pub network: NetworkSandbox,
    #[serde(default)]
    pub allow_unsandboxed_commands: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_commands: Vec<String>,
    #[serde(default)]
    pub fail_if_unavailable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
    #[serde(default)]
    pub settings: ClaudeSandboxSettings,
    /// Raw passthrough for Claude's `settings.permissions` JSON — kept opaque
    /// so Claude-side schema evolution doesn't require a komet release.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub settings_permissions: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemSandbox {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_write: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_write: Vec<String>,
}

impl<'de> Deserialize<'de> for FilesystemSandbox {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default)]
            allow: Vec<String>,
            #[serde(default)]
            deny: Vec<String>,
            #[serde(default, rename = "allowRead")]
            allow_read: Vec<String>,
            #[serde(default, rename = "denyRead")]
            deny_read: Vec<String>,
            #[serde(default, rename = "allowWrite")]
            allow_write: Vec<String>,
            #[serde(default, rename = "denyWrite")]
            deny_write: Vec<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            allow: raw.allow,
            deny: raw.deny,
            allow_read: raw.allow_read,
            deny_read: raw.deny_read,
            allow_write: raw.allow_write,
            deny_write: raw.deny_write,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkSandbox {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_hosts: Vec<String>,
}

/// OpenCode permission table (Paseo `opencode` provider options).
///
/// `bash.patterns` is an ORDERED pattern map: matching walks insertion order
/// and the LAST matching key wins, with a trailing `"*"` entry acting as
/// fallback (`"*": "ask"` is Paseo's canonical default). Stored as
/// `Vec<(String, Perm)>` rather than a std map so document order survives a
/// wire round-trip — reordering would silently change semantics.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenCodePerms {
    #[serde(default)]
    pub bash: BashPerms,
    #[serde(default)]
    pub unscoped_actions: UnscopedActions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Perm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<Perm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_directory: Option<Perm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webfetch: Option<Perm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websearch: Option<Perm>,
}

impl OpenCodePerms {
    /// The trailing `"*"` bash fallback perm, if any (see [`BashPerms`]).
    pub fn bash_fallback(&self) -> Option<Perm> {
        self.bash.fallback()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BashPerms {
    pub patterns: Vec<(String, Perm)>,
}

impl BashPerms {
    /// The trailing `"*"` fallback perm, if any pattern names it.
    pub fn fallback(&self) -> Option<Perm> {
        self.patterns
            .iter()
            .rev()
            .find(|(key, _)| key == "*")
            .map(|(_, perm)| *perm)
    }

    /// Resolve a command against all patterns in order; the LAST match wins.
    /// Patterns are OpenCode-style: a trailing `*` matches any remainder
    /// (`"npm *"`), otherwise the key must equal the command exactly.
    pub fn resolve(&self, command: &str) -> Option<Perm> {
        self.patterns
            .iter()
            .rev()
            .find(|(key, _)| Self::matches(key, command))
            .map(|(_, perm)| *perm)
    }

    fn matches(pattern: &str, command: &str) -> bool {
        match pattern.strip_suffix('*') {
            Some(prefix) => command.starts_with(prefix),
            None => pattern == command,
        }
    }
}

impl Serialize for BashPerms {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.patterns.iter().map(|(k, v)| (k, v)))
    }
}

impl<'de> Deserialize<'de> for BashPerms {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BashPerms;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a map of bash patterns to permissions")
            }
            // visit_map yields entries in DOCUMENT order regardless of any
            // map feature flags, which is exactly what we must preserve.
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut patterns = Vec::new();
                while let Some((key, perm)) = access.next_entry()? {
                    patterns.push((key, perm));
                }
                Ok(BashPerms { patterns })
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

/// Per-tool perms for tools outside the bash gate (`webfetch`, `websearch`,
/// `todowrite`, …). Key order carries no semantics here, so a plain map is fine.
pub type UnscopedActions = std::collections::BTreeMap<String, Perm>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Perm {
    Allow,
    Ask,
    Deny,
}

// ---------------------------------------------------------------------------
// Run-request sandbox validation
// ---------------------------------------------------------------------------

/// A structured rejection reason for [`RunRequest`] sandbox configuration.
/// Variants carry the offending values so the UI can render a precise
/// message and tests can match exactly — never an opaque string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Codex `writable_roots` entry outside `cwd` without
    /// `sandbox_mode = danger-full-access`.
    WritableRootOutsideCwd { root: PathBuf, cwd: PathBuf },
    /// Codex `network_access: true` without danger-full access.
    NetworkAccessRequiresDanger,
    /// Codex `approval_policy: "never"` with no `sandbox_mode` — access is
    /// then governed by nothing at all.
    ApprovalNeverWithoutMode,
    /// Claude `filesystem.allow` entry outside `cwd`.
    ClaudeFilesystemOutsideCwd { path: String, cwd: PathBuf },
    /// Claude contradiction: allow unsandboxed commands AND hard-fail when
    /// sandboxing is unavailable — the run could never start coherently.
    ClaudeUnsandboxedWithFailUnavailable,
    /// Claude `settings_permissions` passthrough carries an escalation-shaped
    /// value (bypass-style defaultMode or a wildcard/bash-all allow entry).
    ClaudeSettingsPermissionsEscalation { detail: String },
    /// OpenCode bash pattern map with no `"*"` fallback entry — either
    /// empty or lacking the wildcard. Under last-match-wins semantics any
    /// unmatched command resolves to nothing (ambient default), so such a
    /// table restricts by illusion.
    OpenCodeMissingFallback,
    /// An unusable permission pattern (empty key can never match anything).
    OpenCodeUnknownPerm { pattern: String },
    /// OpenCode permission tables cannot be applied yet (ACP has no
    /// permission config surface), so a request carrying one is refused
    /// rather than running unrestricted-by-illusion.
    OpenCodeOptionsNotApplied { detail: String },
    /// Reasoning level requested but not supported by the target harness.
    ReasoningLevelUnsupported {
        requested: ReasoningLevel,
        supported: Vec<ReasoningLevel>,
    },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WritableRootOutsideCwd { root, cwd } => write!(
                f,
                "codex writable root {root:?} is outside the workspace {cwd:?} without \
                 danger-full-access"
            ),
            Self::NetworkAccessRequiresDanger => write!(
                f,
                "codex networkAccess requires sandboxMode \"danger-full-access\""
            ),
            Self::ApprovalNeverWithoutMode => write!(
                f,
                "codex approvalPolicy \"never\" requires an explicit sandboxMode"
            ),
            Self::ClaudeFilesystemOutsideCwd { path, cwd } => write!(
                f,
                "claude filesystem.allow entry {path:?} is outside the workspace {cwd:?}"
            ),
            Self::ClaudeUnsandboxedWithFailUnavailable => write!(
                f,
                "claude allowUnsandboxedCommands contradicts failIfUnavailable"
            ),
            Self::ClaudeSettingsPermissionsEscalation { detail } => write!(
                f,
                "claude settingsPermissions passthrough would escalate access: {detail}"
            ),
            Self::OpenCodeMissingFallback => write!(
                f,
                "opencode bash permission map has no \"*\" fallback entry: an unmatched \
                 command would fall through to the ambient default instead of a chosen \
                 permission"
            ),
            Self::OpenCodeUnknownPerm { pattern } => {
                write!(f, "opencode has an unusable permission pattern {pattern:?}")
            }
            Self::OpenCodeOptionsNotApplied { detail } => write!(
                f,
                "opencode permission tables cannot be applied yet (ACP exposes no \
                 permission config surface), so the run is refused rather than executed \
                 with ambient default permissions: {detail}"
            ),
            Self::ReasoningLevelUnsupported {
                requested,
                supported,
            } => write!(
                f,
                "reasoning level {:?} not supported by this harness (supported: {:?})",
                requested, supported
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Detect escalation-shaped content in the opaque Claude
/// `settings.permissions` passthrough. Returns a human-readable detail when
/// the value would grant more than the sandbox claims: a `defaultMode` other
/// than `"default"`/`"acceptEdits"`, or an allow entry of `"*"`, bare
/// `"Bash"`, or any `"Bash(*…)"` (unsandboxed / all-commands bash).
fn settings_permissions_escalation(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    if let Some(mode) = obj.get("defaultMode").and_then(|m| m.as_str()) {
        if mode != "default" && mode != "acceptEdits" {
            return Some(format!("defaultMode {mode:?}"));
        }
    }
    if let Some(allow) = obj.get("allow").and_then(|a| a.as_array()) {
        for entry in allow {
            let Some(rule) = entry.as_str() else {
                continue;
            };
            if rule == "*" || rule == "Bash" || rule.starts_with("Bash(*") {
                return Some(format!("allow entry {rule:?}"));
            }
        }
    }
    None
}

fn lexically_clean(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    path.components()
        .fold(PathBuf::new(), |mut acc, c| match c {
            Component::CurDir => acc,
            Component::ParentDir => {
                acc.pop();
                acc
            }
            other => {
                acc.push(other.as_os_str());
                acc
            }
        })
}

/// Pure containment check of `path` within `cwd` (lexical only — no fs calls,
/// so validation stays deterministic and testable). Paths are resolved
/// against `cwd` when relative; `~`-prefixed entries pass leniently because
/// home expansion is host-side and cannot be decided here.
fn inside_cwd(path: &str, cwd: &std::path::Path) -> bool {
    if path.starts_with('~') {
        return true;
    }
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        lexically_clean(p)
    } else {
        lexically_clean(&cwd.join(p))
    };
    abs.starts_with(cwd)
}

/// Validate a run request's explicit `sandbox_options` before any harness
/// spawn. Precedence rule: when `sandbox_options` rides the request it WINS
/// over the coarse `sandbox` level entirely — danger status comes from the
/// codex table's own `sandbox_mode`, never from `request.sandbox`, and
/// `auto_approve` never grants anything the options table denies.
pub fn validate_run_request(request: &RunRequest) -> Result<(), ValidationError> {
    let Some(options) = &request.sandbox_options else {
        return Ok(());
    };
    let cwd = lexically_clean(std::path::Path::new(&request.cwd));

    if let Some(codex) = &options.codex {
        let danger_full_access = codex.sandbox_mode == Some(SandboxMode::DangerFullAccess);
        if !danger_full_access {
            for root in &codex.writable_roots {
                if !inside_cwd(&root.to_string_lossy(), &cwd) {
                    return Err(ValidationError::WritableRootOutsideCwd {
                        root: root.clone(),
                        cwd: cwd.clone(),
                    });
                }
            }
            if codex.network_access {
                return Err(ValidationError::NetworkAccessRequiresDanger);
            }
        }
        if codex.approval_policy == Some(ApprovalPolicy::Never) && codex.sandbox_mode.is_none() {
            return Err(ValidationError::ApprovalNeverWithoutMode);
        }
    }

    if let Some(claude) = &options.claude {
        for allowed in claude
            .filesystem
            .allow
            .iter()
            .chain(&claude.filesystem.allow_read)
            .chain(&claude.filesystem.allow_write)
        {
            if !inside_cwd(allowed, &cwd) {
                return Err(ValidationError::ClaudeFilesystemOutsideCwd {
                    path: allowed.clone(),
                    cwd: cwd.clone(),
                });
            }
        }
        if claude.allow_unsandboxed_commands && claude.fail_if_unavailable {
            return Err(ValidationError::ClaudeUnsandboxedWithFailUnavailable);
        }
        if let Some(detail) = settings_permissions_escalation(&claude.settings_permissions) {
            return Err(ValidationError::ClaudeSettingsPermissionsEscalation { detail });
        }
    }

    if let Some(opencode) = &options.opencode {
        if opencode.bash_fallback().is_none() {
            return Err(ValidationError::OpenCodeMissingFallback);
        }
        if let Some((pattern, _)) = opencode.bash.patterns.iter().find(|(k, _)| k.is_empty()) {
            return Err(ValidationError::OpenCodeUnknownPerm {
                pattern: pattern.clone(),
            });
        }
    }

    Ok(())
}

/// Validate reasoning level against the harness's supported ladder.
/// `None` reasoning always passes (no explicit request).
pub fn validate_reasoning(
    requested: Option<ReasoningLevel>,
    supported: &[ReasoningLevel],
) -> Result<(), ValidationError> {
    if let Some(level) = requested {
        if !supported.contains(&level) {
            return Err(ValidationError::ReasoningLevelUnsupported {
                requested: level,
                supported: supported.to_vec(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringMode {
    /// Steer delivered at the next step boundary within the live turn.
    StepBoundary,
    /// Steer delivered only between turns.
    TurnBoundary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub label: String,
    /// Short tagline rendered under the name in the model picker (11px muted),
    /// mirroring the Electron app's `ModelInfo.description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub options: Vec<ModelOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub choices: Vec<ModelOptionChoice>,
    pub default_choice: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub prompt: String,
    /// The harness picked at send time. Rides the command plane so
    /// claim-on-first-command (chat row still in flight on the registry
    /// channel) dispatches — and records — the picked harness instead of the
    /// engine default. Additive + serde-defaulted for wire compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// Harness-specific option selections (option id -> choice id), JSON round-tripped.
    #[serde(default)]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub cwd: String,
    pub sandbox: SandboxLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_options: Option<SandboxOptions>,
    #[serde(default)]
    pub auto_approve: bool,
    /// Harness-native session id to resume, if any.
    pub resume: Option<String>,
    /// Absolute paths of image attachments already staged on the run device
    /// (composer uploads: UploadChunk/UploadCommit → durable path). The same
    /// paths also ride the prompt text as `Attached images (local files …)`
    /// refs (komet's `withAttachments` transport — that's what persists in the
    /// doc); this field additionally lets a harness inline the bytes as image
    /// content blocks. Additive + serde-defaulted for wire compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// Host-side isolated-worktree creation (see [`WorktreeSpec`]): when set,
    /// the HOST materializes the worktree at command-drain time and runs there
    /// instead of `cwd`. Additive + serde-defaulted for wire compat — an old
    /// host ignores it and runs in `cwd` (the repo's main checkout).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeSpec>,
}

/// Isolated-worktree directive riding [`RunRequest`]. The worktree is created
/// by the HOST while draining the queued Run — not by the sender over a
/// blocking CreateWorktree RPC — so the send path stays durable: a lost relay
/// frame can't wedge the composer on "Sending…" while the session runs anyway
/// (2026-08-18 user report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSpec {
    /// The repo whose worktree to create (the space's folder on the host).
    pub repo_path: String,
    /// Base ref the fresh `komet/<name>` branch is created off.
    pub base: String,
}

/// The session-scoped singleton id for the live plan/todo chip. ACP plan
/// updates carry no wire id; adapters emit every update under this one id so
/// the fold refreshes the same chip in place. Consumers that de-duplicate
/// tool ids across segment boundaries (the engine's stale-echo filter) must
/// EXEMPT this id — it legitimately reappears in every segment for the whole
/// life of a run.
pub const LIVE_PLAN_TOOL_ID: &str = "acp-plan";

/// A decoded tool invocation, reduced to the fields each kind renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolCall {
    Exec {
        command: String,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        /// Full content; STRIPPED by the render-parts policy before entering the doc.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    EditFile {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_string: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        new_string: Option<String>,
    },
    ApplyPatch {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Search {
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Glob {
        pattern: String,
    },
    WebFetch {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    WebSearch {
        query: String,
    },
    Todo {
        #[serde(default)]
        items: Vec<TodoItem>,
    },
    Mcp {
        server: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    Unknown {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
}

impl ToolCall {
    /// A subagent SPAWN call — the `Agent[: <description>]` naming convention
    /// every driver decodes its spawn tool into (claude/codex `Task`, cursor
    /// `task`, grok `spawn_subagent`, opencode `task`). This is the single
    /// genus gate for subagent binding: tagged subagent traffic may only ever
    /// stamp a ref/status onto a spawn call, so a driver keying bug can never
    /// turn an ordinary Run/Read chip into a spawn chip (2026-08-20: claude's
    /// background-shell `task_notification` did exactly that — the chip
    /// linked to a never-created doc and opened an empty panel).
    pub fn is_subagent_spawn(&self) -> bool {
        let name = match self {
            ToolCall::Unknown { name, .. } => name,
            ToolCall::Mcp { tool, .. } => tool,
            _ => return false,
        };
        name == "Agent" || name.starts_with("Agent: ")
    }

    /// The model a subagent SPAWN was given, when the spawn named one.
    ///
    /// Read off the spawn's own input rather than the session's picked model:
    /// a spawn may override it per child (claude's `Agent` takes `model`, grok
    /// `spawn_subagent` a `model_id`), and two chips spawned in one turn can
    /// legitimately name different models. `None` means the spawn didn't say —
    /// the child inherits the parent's model, which the chip already implies,
    /// so nothing is rendered rather than guessing a name.
    ///
    /// Only ever answers for [`is_subagent_spawn`](Self::is_subagent_spawn)
    /// calls: an ordinary tool with a stray `model` argument is not a spawn.
    pub fn subagent_model(&self) -> Option<&str> {
        if !self.is_subagent_spawn() {
            return None;
        }
        let input = match self {
            ToolCall::Unknown { input, .. } | ToolCall::Mcp { input, .. } => input.as_ref()?,
            _ => return None,
        };
        SUBAGENT_MODEL_KEYS
            .iter()
            .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|model| !model.is_empty())
    }
}

/// Spawn-input keys that carry a child model, in precedence order. Drivers
/// disagree on the spelling, so the lookup is by key set, not by harness —
/// a new adapter naming it any of these needs no code change here.
pub const SUBAGENT_MODEL_KEYS: [&str; 4] = ["model", "modelId", "model_id", "subagent_model"];

/// The spawn-input keys [`sanitize_tool_call`](crate::) must preserve so the
/// chip can name the child's model. Deliberately tiny: everything else on a
/// spawn's input (the whole prompt, most of all) stays host-local.
pub const SUBAGENT_INPUT_KEEP: [&str; 5] = [
    "model",
    "modelId",
    "model_id",
    "subagent_model",
    "subagent_type",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

/// A slash command advertised by the agent (ACP `availableCommands`): typed as
/// `/name` at the start of the composer, sent to the agent as prompt text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Placeholder hint for the command's argument, when it takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
}

/// A file modification carried inline on a tool result (ACP
/// `ToolCallContent::Diff`). `old_text: None` means a new file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PermissionKind {
    Tool { name: String },
    Command { cmdline: String },
    FileWrite { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    Chat,
    Repo,
    Pattern(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PermissionChoice {
    Allow,
    AllowAlways { scope: Scope },
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPermissionAction {
    pub id: String,
    pub label: String,
    pub behavior: PermissionBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionDecision {
    pub request_id: String,
    pub choice: PermissionChoice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_permissions: Option<SandboxOptions>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneStatus {
    Completed,
    Interrupted,
    Errored,
}

/// WHY a run settled — set by the ENGINE on synthesized endings so the UI can
/// tell a user-requested stop apart from an engine-restart recovery (neither
/// should render as a crash). Absent ([`None`], never serialized) means the
/// harness produced its own natural `Done`.
///
/// Wire-compat: additive optional field — older peers parse a Done carrying a
/// `reason` fine only once they know the field (deny_unknown_fields is NOT set
/// on this enum); the omitted case round-trips byte-identical to pre-field
/// payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneReason {
    /// The user (or an engine shutdown path acting for them) stopped the run.
    UserRequested,
    /// Synthesized by boot recovery (`recover_stale`): the previous engine
    /// process died mid-run and the journal was closed on its behalf.
    EngineRestart,
}

/// The normalized streaming event every harness emits.
///
/// Mirrors komet's `AgentEvent` tagged enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    #[serde(rename_all = "camelCase")]
    SessionStarted {
        harness: HarnessId,
        model: String,
        #[serde(default)]
        tools: Vec<String>,
        cwd: String,
        /// Harness-native session id (used for resume).
        session_id: String,
        assistant_message_id: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Backend-internal steering boundary marker.
    #[serde(rename_all = "camelCase")]
    AssistantMessageCompleted {
        assistant_message_id: String,
    },
    ToolCall {
        id: String,
        call: ToolCall,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        id: String,
        is_error: bool,
        /// Tool output text, capped by the emitting harness (ACP tool-call
        /// content; claude/codex adapters never populate it). The doc-side
        /// fold applies its own byte cap before anything persists.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// Inline file diff for edit-shaped tools (ACP `Diff` content).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<ToolDiff>,
    },
    /// Kept as a harness passthrough (rate-limit probes); never persisted to docs.
    #[serde(rename_all = "camelCase")]
    Usage {
        input_tokens: u64,
        #[serde(default)]
        cached_input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_limit: Option<u64>,
    },
    /// The agent advertised (or changed) its slash-command set — ACP
    /// `available_commands_update`. The engine caches the latest list per
    /// harness for the composer's `/` popup; never persisted to docs.
    #[serde(rename_all = "camelCase")]
    AvailableCommands {
        commands: Vec<SlashCommand>,
    },
    Error {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    PermissionRequested {
        request_id: String,
        kind: PermissionKind,
        summary: String,
        choices: Vec<PermissionChoice>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        actions: Vec<AgentPermissionAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    PermissionResolved {
        request_id: String,
        choice: PermissionChoice,
    },
    #[serde(rename_all = "camelCase")]
    InputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    #[serde(rename_all = "camelCase")]
    InputResolved {
        request_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Steered {
        assistant_message_id: Option<String>,
        next_assistant_message_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        status: DoneStatus,
        result: Option<String>,
        error: Option<String>,
        session_id: Option<String>,
        /// Set only on engine-synthesized endings (see [`DoneReason`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<DoneReason>,
    },
    /// A USER-role message injected into a running session — today only seen
    /// wrapped in [`AgentEvent::Subagent`]: the PARENT agent steering its
    /// subagent mid-run (claude: a tagged user frame's text blocks). The
    /// engine writes it to the subagent doc as its own user entry, closing
    /// the streaming assistant segment above it — the subagent transcript
    /// then reads like any steered chat. Never emitted untagged (the parent
    /// chat's user messages come from doc commands, not the wire).
    #[serde(rename_all = "camelCase")]
    UserMessage {
        text: String,
    },
    /// An event belonging to a SUBAGENT's nested transcript, attributed to
    /// the spawning tool call (`parent_tool_use_id` = the parent-feed
    /// `ToolCall::id` that launched it). Never folded into the parent chat
    /// doc — the engine routes these to the subagent's own doc; the parent
    /// keeps only the spawn chip. Additive: old consumers that don't match
    /// this variant drop the nested traffic, which is the pre-subagent-viz
    /// behavior.
    #[serde(rename_all = "camelCase")]
    Subagent {
        parent_tool_use_id: String,
        event: Box<AgentEvent>,
    },
}

/// Cumulative provider-reported usage and context-window metrics for a chat.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageStats {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub context_limit: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
    #[serde(default)]
    pub compactions_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions_reason: Option<String>,
}

impl ContextUsageStats {
    pub fn new(context_limit: u64) -> Self {
        let context_limit = if context_limit == 0 {
            200_000
        } else {
            context_limit
        };
        Self {
            context_limit,
            compact_threshold: Some(context_limit.saturating_mul(3) / 4),
            ..Self::default()
        }
    }

    /// Cached and reasoning tokens are provider breakdowns already included
    /// in the input/output totals, so they are not counted a second time.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub fn context_ratio(&self) -> f32 {
        if self.context_limit == 0 {
            0.0
        } else {
            (self.total_tokens() as f32 / self.context_limit as f32).clamp(0.0, 1.0)
        }
    }

    pub fn context_percent(&self) -> u32 {
        (self.context_ratio() * 100.0).round() as u32
    }

    pub fn ingest(
        &mut self,
        input: u64,
        cached: u64,
        output: u64,
        reasoning: u64,
        limit: Option<u64>,
    ) {
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.cached_input_tokens = self.cached_input_tokens.saturating_add(cached);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(reasoning);
        if let Some(limit) = limit.filter(|limit| *limit > 0) {
            self.context_limit = limit;
            self.compact_threshold = Some(limit.saturating_mul(3) / 4);
        }
    }
}

/// Conservative context-window defaults for providers that do not report one.
pub fn default_context_limit_for_model(model_name: &str) -> u64 {
    let model = model_name.to_ascii_lowercase();
    if ["gemini-1.5", "gemini-2.0", "gemini-2.5"]
        .iter()
        .any(|name| model.contains(name))
    {
        1_000_000
    } else if model.contains("deepseek")
        || ["gpt-4o", "o1", "o3"]
            .iter()
            .any(|name| model.contains(name))
    {
        128_000
    } else if model.contains("grok") {
        131_072
    } else {
        200_000
    }
}

/// Formats token counts for compact UI labels.
pub fn format_tokens(count: u64) -> String {
    if count >= 1_000_000_000 {
        format!("{:.1}b", count as f64 / 1_000_000_000.0)
    } else if count >= 10_000_000 {
        format!("{}m", count / 1_000_000)
    } else if count >= 1_000_000 {
        format!("{:.1}m", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{}k", count / 1_000)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// thinkingOptionId (UI) <-> ReasoningLevel round-trip table; unknown ids
    /// must be rejected rather than silently downgraded.
    #[test]
    fn thinking_id_maps_to_reasoning_level_and_back() {
        let table = [
            ("minimal", ReasoningLevel::Minimal),
            ("low", ReasoningLevel::Low),
            ("medium", ReasoningLevel::Medium),
            ("high", ReasoningLevel::High),
            ("xhigh", ReasoningLevel::XHigh),
            ("max", ReasoningLevel::Max),
            ("ultra", ReasoningLevel::Ultra),
            ("ultracode", ReasoningLevel::Ultracode),
            ("ultrathink", ReasoningLevel::Ultrathink),
        ];
        for (id, level) in table {
            assert_eq!(ReasoningLevel::from_thinking_id(id), Some(level));
            assert_eq!(level.as_thinking_id(), id);
        }
        assert_eq!(ReasoningLevel::from_thinking_id("bogus"), None);
        assert_eq!(ReasoningLevel::from_thinking_id(""), None);
    }

    #[test]
    fn agent_event_round_trips() {
        let ev = AgentEvent::ToolCall {
            id: "t1".into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    /// Drivers spell the key differently; the chip must not care which one
    /// spawned the child. Non-spawns never answer, whatever they carry.
    #[test]
    fn subagent_model_reads_every_spelling_and_only_off_a_spawn() {
        let spawn = |input: serde_json::Value| ToolCall::Unknown {
            name: "Agent: scan".into(),
            input: Some(input),
        };
        for key in SUBAGENT_MODEL_KEYS {
            let call = spawn(serde_json::json!({ key: "haiku" }));
            assert_eq!(call.subagent_model(), Some("haiku"), "key {key}");
        }
        // An MCP-shaped spawn (cursor routes its `task` through MCP) too.
        assert_eq!(
            ToolCall::Mcp {
                server: "s".into(),
                tool: "Agent: scan".into(),
                input: Some(serde_json::json!({ "model": "sonnet" })),
            }
            .subagent_model(),
            Some("sonnet")
        );
        // Not a spawn: the name gate wins over the key.
        assert_eq!(
            ToolCall::Unknown {
                name: "Bash".into(),
                input: Some(serde_json::json!({ "model": "haiku" })),
            }
            .subagent_model(),
            None
        );
        // A spawn that named nothing usable inherits — nothing to render.
        assert_eq!(
            spawn(serde_json::json!({ "model": " " })).subagent_model(),
            None
        );
        assert_eq!(
            spawn(serde_json::json!({ "prompt": "x" })).subagent_model(),
            None
        );
        assert_eq!(
            ToolCall::Unknown {
                name: "Agent".into(),
                input: None
            }
            .subagent_model(),
            None
        );
        // Non-string values are not names.
        assert_eq!(
            spawn(serde_json::json!({ "model": 5 })).subagent_model(),
            None
        );
    }

    #[test]
    fn run_request_attachments_default_and_round_trip() {
        // Old-wire JSON without the field parses (additive compat)…
        let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.attachments.is_empty());
        // …and an empty list serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("attachments").is_none());
        // Populated lists round-trip.
        let req = RunRequest {
            attachments: vec!["/tmp/a.png".into()],
            ..req
        };
        let round: RunRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(round.attachments, vec!["/tmp/a.png".to_string()]);
    }

    #[test]
    fn run_request_worktree_default_and_round_trip() {
        // Old-wire JSON without the field parses (additive compat)…
        let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.worktree.is_none());
        // …and `None` serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("worktree").is_none());
        // A populated spec round-trips camelCased.
        let req = RunRequest {
            worktree: Some(WorktreeSpec {
                repo_path: "/repos/comet".into(),
                base: "main".into(),
            }),
            ..req
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["worktree"]["repoPath"], "/repos/comet");
        let round: RunRequest = serde_json::from_value(json).unwrap();
        assert_eq!(round.worktree, req.worktree);
    }

    #[test]
    fn harness_id_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&HarnessId::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
    }

    #[test]
    fn opencode_accepts_its_legacy_wire_name() {
        assert_eq!(
            serde_json::from_str::<HarnessId>("\"open-code\"").unwrap(),
            HarnessId::Opencode
        );
        assert_eq!(
            serde_json::to_string(&HarnessId::Opencode).unwrap(),
            "\"opencode\""
        );
    }

    #[test]
    fn context_usage_accumulates_without_double_counting_breakdowns() {
        let mut stats = ContextUsageStats::new(128_000);
        stats.ingest(10_000, 2_000, 500, 300, None);

        assert_eq!(stats.total_tokens(), 10_500);
        assert_eq!(stats.context_percent(), 8);
        assert_eq!(stats.compact_threshold, Some(96_000));
        assert_eq!(default_context_limit_for_model("gemini-2.5-pro"), 1_000_000);
        assert_eq!(format_tokens(10_500), "10k");
    }

    #[test]
    fn sandbox_options_rejects_unknown_field() {
        let json = r#"{"sandboxMode":"workspace-write","unknown":1}"#;
        assert!(serde_json::from_str::<CodexSandbox>(json).is_err());
    }

    #[test]
    fn opencode_perms_last_key_is_fallback() {
        let json = r#"{"bash":{"*":"ask","git status":"allow"}}"#;
        let perms: OpenCodePerms = serde_json::from_str(json).unwrap();
        assert_eq!(perms.bash_fallback(), Some(Perm::Ask));
    }

    #[test]
    fn codex_sandbox_full_table_round_trips_camel_case() {
        let json = r#"{
            "sandboxMode": "workspace-write",
            "writableRoots": ["/repo"],
            "networkAccess": true,
            "webSearch": false,
            "features": ["shell"],
            "approvalPolicy": "never"
        }"#;
        let opts: CodexSandbox = serde_json::from_str(json).unwrap();
        assert_eq!(opts.sandbox_mode, Some(SandboxMode::WorkspaceWrite));
        assert_eq!(opts.writable_roots, vec![PathBuf::from("/repo")]);
        assert!(opts.network_access);
        assert!(!opts.web_search);
        assert_eq!(opts.approval_policy, Some(ApprovalPolicy::Never));
        // round-trip keeps the same wire shape
        let back: CodexSandbox =
            serde_json::from_str(&serde_json::to_string(&opts).unwrap()).unwrap();
        assert_eq!(back, opts);
    }

    #[test]
    fn codex_approval_policy_granular_round_trips() {
        let json = r#"{"kind":"granular","ask":["rm -rf"],"autoApprove":["ls"]}"#;
        let pol: ApprovalPolicy = serde_json::from_str(json).unwrap();
        match &pol {
            ApprovalPolicy::Granular { ask, auto_approve } => {
                assert_eq!(ask, &vec!["rm -rf".to_string()]);
                assert_eq!(auto_approve, &vec!["ls".to_string()]);
            }
            other => panic!("expected granular, got {other:?}"),
        }
    }

    #[test]
    fn claude_sandbox_full_table_round_trips() {
        let json = r#"{
            "filesystem": {"allow": ["/repo"], "deny": ["~/.ssh"]},
            "network": {"allowedHosts": ["crates.io"]},
            "allowUnsandboxedCommands": false,
            "excludedCommands": ["sudo"],
            "failIfUnavailable": true,
            "settingsPermissions": {"allow": ["Bash(npm run *)"]}
        }"#;
        let opts: ClaudeSandbox = serde_json::from_str(json).unwrap();
        assert_eq!(
            opts.filesystem,
            FilesystemSandbox {
                allow: vec!["/repo".into()],
                deny: vec!["~/.ssh".into()],
                ..Default::default()
            }
        );
        assert_eq!(opts.network.allowed_hosts, vec!["crates.io".to_string()]);
        assert!(!opts.allow_unsandboxed_commands);
        assert_eq!(opts.excluded_commands, vec!["sudo".to_string()]);
        assert!(opts.fail_if_unavailable);
        assert_eq!(
            opts.settings_permissions.get("allow"),
            Some(&serde_json::json!(["Bash(npm run *)"]))
        );
        let back: ClaudeSandbox =
            serde_json::from_str(&serde_json::to_string(&opts).unwrap()).unwrap();
        assert_eq!(back, opts);
    }

    /// Pattern maps must survive a wire round-trip in DOCUMENT order — the
    /// last matching key is the fallback, so reordering changes semantics.
    #[test]
    fn opencode_bash_patterns_preserve_order_across_wire() {
        let json = r#"{"bash":{"*":"deny","npm *":"allow","git status":"ask"}}"#;
        let perms: OpenCodePerms = serde_json::from_str(json).unwrap();
        assert_eq!(
            perms.bash.patterns,
            vec![
                ("*".to_string(), Perm::Deny),
                ("npm *".to_string(), Perm::Allow),
                ("git status".to_string(), Perm::Ask),
            ]
        );
        // Command resolution walks all patterns, last match wins.
        assert_eq!(perms.bash.resolve("npm install"), Some(Perm::Allow));
        assert_eq!(perms.bash.resolve("cargo build"), Some(Perm::Deny));
        assert_eq!(perms.bash.resolve("git status"), Some(Perm::Ask));
        // Round-trip preserves order exactly on the wire (string path).
        // NOTE: routing through serde_json::Value instead would sort keys —
        // serde_json::Map is BTreeMap-ordered without the `preserve_order`
        // feature. Wire transports carry bytes/strings, which is the path
        // that matters here.
        let wire = serde_json::to_string(&perms).unwrap();
        let back: OpenCodePerms = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, perms);
        // A trailing "*" entry is the fallback.
        let json = r#"{"bash":{"x":"allow","*":"deny"}}"#;
        let perms: OpenCodePerms = serde_json::from_str(json).unwrap();
        assert_eq!(perms.bash_fallback(), Some(Perm::Deny));
    }

    #[test]
    fn opencode_unscoped_actions_map_tools_to_perms() {
        let json =
            r#"{"unscopedActions":{"webfetch":"ask","websearch":"allow","todowrite":"allow"}}"#;
        let perms: OpenCodePerms = serde_json::from_str(json).unwrap();
        assert_eq!(perms.unscoped_actions.get("webfetch"), Some(&Perm::Ask));
        assert_eq!(perms.unscoped_actions.get("websearch"), Some(&Perm::Allow));
        assert_eq!(perms.unscoped_actions.get("todowrite"), Some(&Perm::Allow));
    }

    #[test]
    fn sandbox_options_carries_each_provider_and_defaults_empty() {
        assert_eq!(
            SandboxOptions::default(),
            SandboxOptions {
                codex: None,
                claude: None,
                opencode: None,
            }
        );
        let req = RunRequest {
            prompt: "p".into(),
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: ".".into(),
            sandbox: SandboxLevel::ReadOnly,
            harness: None,
            auto_approve: false,
            attachments: vec![],
            worktree: None,
            resume: None,
            sandbox_options: Some(SandboxOptions {
                opencode: Some(OpenCodePerms {
                    bash: BashPerms {
                        patterns: vec![("*".into(), Perm::Allow)],
                    },
                    unscoped_actions: Default::default(),
                ..Default::default()
                }),
                ..Default::default()
            }),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["sandboxOptions"]["opencode"]["bash"]["*"], "allow");
        let back: RunRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.sandbox_options, req.sandbox_options);
    }

    #[test]
    fn run_request_old_wire_without_sandbox_options_still_parses() {
        let old = r#"{"prompt":"p","cwd":".","sandbox":"workspace-write"}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.sandbox_options.is_none());
        assert_eq!(req.sandbox, SandboxLevel::WorkspaceWrite);
        // round-trip: None serialise away (old readers never see it)
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("sandboxOptions").is_none());
    }

    // ------------------------------------------------------------------
    // validate_run_request
    // ------------------------------------------------------------------

    fn base_request() -> RunRequest {
        RunRequest {
            prompt: "p".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/repo".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            sandbox_options: None,
            auto_approve: false,
            attachments: vec![],
            worktree: None,
            resume: None,
        }
    }

    fn codex_options(codex: CodexSandbox) -> Option<SandboxOptions> {
        Some(SandboxOptions {
            codex: Some(codex),
            ..Default::default()
        })
    }

    fn claude_options(claude: ClaudeSandbox) -> Option<SandboxOptions> {
        Some(SandboxOptions {
            claude: Some(claude),
            ..Default::default()
        })
    }

    fn opencode_options(perms: OpenCodePerms) -> Option<SandboxOptions> {
        Some(SandboxOptions {
            opencode: Some(perms),
            ..Default::default()
        })
    }

    #[test]
    fn validation_accepts_a_plain_request_without_options() {
        assert_eq!(validate_run_request(&base_request()), Ok(()));
    }

    #[test]
    fn validation_accepts_well_formed_codex_options() {
        let mut req = base_request();
        req.sandbox_options = codex_options(CodexSandbox {
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            writable_roots: vec!["/repo/target".into(), "subdir".into()],
            approval_policy: Some(ApprovalPolicy::Never),
            ..Default::default()
        });
        assert_eq!(validate_run_request(&req), Ok(()));
    }

    #[test]
    fn validation_rejects_writable_root_outside_cwd() {
        let mut req = base_request();
        req.sandbox_options = codex_options(CodexSandbox {
            writable_roots: vec!["/etc".into()],
            ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::WritableRootOutsideCwd {
                root: "/etc".into(),
                cwd: "/repo".into(),
            })
        );
    }

    #[test]
    fn validation_allows_outside_cwd_roots_under_danger_full_access() {
        let mut req = base_request();
        req.sandbox_options = codex_options(CodexSandbox {
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            writable_roots: vec!["/etc".into()],
            network_access: true,
            ..Default::default()
        });
        assert_eq!(validate_run_request(&req), Ok(()));
    }

    #[test]
    fn validation_sandbox_options_win_over_sandbox_level() {
        // `sandbox = DangerFullAccess` on the request must NOT rescue a codex
        // table that itself is not danger-full: when `sandbox_options` rides
        // the request it WINS over the coarse level entirely.
        let mut req = base_request();
        req.sandbox = SandboxLevel::DangerFullAccess;
        req.sandbox_options = codex_options(CodexSandbox {
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            writable_roots: vec!["/etc".into()],
            ..Default::default()
        });
        assert!(matches!(
            validate_run_request(&req),
            Err(ValidationError::WritableRootOutsideCwd { .. })
        ));
    }

    #[test]
    fn validation_yolo_does_not_override_explicit_options() {
        // auto_approve=true must never silently grant what the explicit
        // options table denies.
        let mut req = base_request();
        req.auto_approve = true;
        req.sandbox_options = codex_options(CodexSandbox {
            network_access: true,
            ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::NetworkAccessRequiresDanger)
        );
    }

    #[test]
    fn validation_rejects_network_access_without_danger() {
        let mut req = base_request();
        req.sandbox_options = codex_options(CodexSandbox {
            sandbox_mode: Some(SandboxMode::ReadOnly),
            network_access: true,
            ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::NetworkAccessRequiresDanger)
        );
    }

    #[test]
    fn validation_rejects_approval_never_without_sandbox_mode() {
        let mut req = base_request();
        req.sandbox_options = codex_options(CodexSandbox {
            approval_policy: Some(ApprovalPolicy::Never),
            ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::ApprovalNeverWithoutMode)
        );
    }

    #[test]
    fn validation_rejects_claude_allow_outside_cwd() {
        let mut req = base_request();
        req.sandbox_options = claude_options(ClaudeSandbox {
            filesystem: FilesystemSandbox {
                allow: vec!["/home/other".into()],
                deny: vec![],
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::ClaudeFilesystemOutsideCwd {
                path: "/home/other".into(),
                cwd: "/repo".into(),
            })
        );
    }

    #[test]
    fn validation_rejects_claude_unsandboxed_with_fail_if_unavailable() {
        let mut req = base_request();
        req.sandbox_options = claude_options(ClaudeSandbox {
            allow_unsandboxed_commands: true,
            fail_if_unavailable: true,
            ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::ClaudeUnsandboxedWithFailUnavailable)
        );
    }

    #[test]
    fn validation_accepts_well_formed_claude_options() {
        let mut req = base_request();
        req.sandbox_options = claude_options(ClaudeSandbox {
            filesystem: FilesystemSandbox {
                allow: vec!["/repo/src".into(), "relative/dir".into()],
                deny: vec!["~/.ssh".into()],
                ..Default::default()
            },
            fail_if_unavailable: true,
            ..Default::default()
        });
        assert_eq!(validate_run_request(&req), Ok(()));
    }

    #[test]
    fn validation_rejects_claude_settings_permissions_bypass_mode() {
        let mut req = base_request();
        req.sandbox_options = claude_options(ClaudeSandbox {
            settings_permissions: serde_json::json!({"defaultMode": "bypassPermissions"}),
            ..Default::default()
        });
        assert!(matches!(
            validate_run_request(&req),
            Err(ValidationError::ClaudeSettingsPermissionsEscalation { .. })
        ));
    }

    #[test]
    fn validation_rejects_claude_settings_permissions_wildcard_allow() {
        for allow in [
            serde_json::json!(["*"]),
            serde_json::json!(["Bash"]),
            serde_json::json!(["Bash(*)"]),
            serde_json::json!(["Read(src/**)", "Bash(*)"]),
        ] {
            let mut req = base_request();
            req.sandbox_options = claude_options(ClaudeSandbox {
                settings_permissions: serde_json::json!({"allow": allow}),
                ..Default::default()
            });
            assert!(
                matches!(
                    validate_run_request(&req),
                    Err(ValidationError::ClaudeSettingsPermissionsEscalation { .. })
                ),
                "should reject allow={allow}"
            );
        }
    }

    #[test]
    fn validation_accepts_non_escalating_claude_settings_permissions() {
        let mut req = base_request();
        req.sandbox_options = claude_options(ClaudeSandbox {
            settings_permissions: serde_json::json!({
                "defaultMode": "acceptEdits",
                "allow": ["Read(src/**)", "Bash(git *)"],
                "deny": ["~/.ssh"]
            }),
            ..Default::default()
        });
        assert_eq!(validate_run_request(&req), Ok(()));
    }

    #[test]
    fn validation_rejects_opencode_patterns_without_fallback() {
        // Empty AND non-empty-without-"*" both lack a fallback: under
        // last-match-wins semantics an unmatched command resolves to nothing
        // (i.e. ambient default), so either shape is unusable.
        for patterns in [
            vec![],
            vec![("npm *".to_string(), Perm::Ask)],
            vec![
                ("git *".to_string(), Perm::Deny),
                ("npm *".to_string(), Perm::Allow),
            ],
        ] {
            let mut req = base_request();
            req.sandbox_options = opencode_options(OpenCodePerms {
                bash: BashPerms { patterns },
                unscoped_actions: Default::default(),
                ..Default::default()
            });
            assert_eq!(
                validate_run_request(&req),
                Err(ValidationError::OpenCodeMissingFallback),
                "pattern table without \"*\" must be rejected"
            );
        }
    }

    #[test]
    fn validation_accepts_opencode_options_once_applicable() {
        // ACP permission surface now wired (Task 2.0), so a well-formed
        // opencode table passes validation instead of being refused.
        let mut req = base_request();
        req.sandbox_options = opencode_options(OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("npm *".into(), Perm::Ask), ("*".into(), Perm::Deny)],
            },
            unscoped_actions: [("webfetch".to_string(), Perm::Allow)]
                .into_iter()
                .collect(),
            ..Default::default()
        });
        assert_eq!(validate_run_request(&req), Ok(()));
    }

    #[test]
    fn validation_rejects_opencode_unknown_perm_pattern() {
        // An empty pattern key can never match any command — an unusable
        // ("unknown") permission entry. Structural checks (fallback, usable
        // keys) surface before the not-yet-applicable refusal so users see
        // WHY their table was rejected.
        let mut req = base_request();
        req.sandbox_options = opencode_options(OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("".into(), Perm::Allow), ("*".into(), Perm::Ask)],
            },
            unscoped_actions: Default::default(),
                ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::OpenCodeUnknownPerm { pattern: "".into() })
        );
    }

    #[test]
    fn permission_requested_round_trips() {
        let ev = AgentEvent::PermissionRequested {
            request_id: "r1".into(),
            kind: PermissionKind::Command {
                cmdline: "rm -rf dist".into(),
            },
            summary: "Run `rm -rf dist`".into(),
            choices: vec![
                PermissionChoice::Allow,
                PermissionChoice::AllowAlways { scope: Scope::Chat },
                PermissionChoice::Deny,
            ],
            actions: vec![
                AgentPermissionAction {
                    id: "reject".into(),
                    label: "Deny".into(),
                    behavior: PermissionBehavior::Deny,
                    pattern: None,
                },
                AgentPermissionAction {
                    id: "accept".into(),
                    label: "Allow".into(),
                    behavior: PermissionBehavior::Allow,
                    pattern: None,
                },
            ],
            provider: Some("opencode".into()),
        };
        let json = serde_json::to_value(&ev).unwrap();
        let back: AgentEvent = serde_json::from_value(json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn old_wire_without_permission_still_parses() {
        let json = r#"{"type":"error","message":"something failed"}"#;
        let parsed: Result<AgentEvent, _> = serde_json::from_str(json);
        assert!(parsed.is_ok());
    }

    #[test]
    fn done_reason_round_trips() {
        let ev = AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: Some("Run interrupted by engine restart".into()),
            session_id: None,
            reason: Some(DoneReason::EngineRestart),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""reason":"engineRestart""#));
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }

    /// Done payloads WITHOUT a reason (every pre-field peer, journal, and
    /// snapshot) keep parsing, with `reason` defaulting to `None` — and
    /// `None` stays absent on serialization so the omitted case is
    /// byte-identical to the old wire.
    #[test]
    fn old_wire_without_done_reason_still_parses() {
        let json = concat!(
            r#"{"type":"done","status":"completed","result":null,"#,
            r#""error":null,"sessionId":"hs-1"}"#
        );
        let parsed: AgentEvent = serde_json::from_str(json).unwrap();
        let expected = AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("hs-1".into()),
            reason: None,
        };
        assert_eq!(parsed, expected);
        // None never serializes — the field only appears when set.
        let json = serde_json::to_string(&expected).unwrap();
        assert!(!json.contains("reason"));
    }

    #[test]
    fn validation_rejects_unsupported_reasoning_level() {
        assert_eq!(
            validate_reasoning(Some(ReasoningLevel::High), &[]),
            Err(ValidationError::ReasoningLevelUnsupported {
                requested: ReasoningLevel::High,
                supported: vec![],
            })
        );
    }

    #[test]
    fn validation_accepts_reasoning_level_in_agent_ladder() {
        assert_eq!(
            validate_reasoning(
                Some(ReasoningLevel::Medium),
                &[
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High
                ]
            ),
            Ok(())
        );
        assert_eq!(validate_reasoning(None, &[]), Ok(()));
    }

    #[test]
    fn validation_rejects_opencode_patterns_without_wildcard_fallback() {
        let mut req = base_request();
        req.sandbox_options = opencode_options(OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("git status".into(), Perm::Allow)],
            },
            unscoped_actions: Default::default(),
                ..Default::default()
        });
        assert_eq!(
            validate_run_request(&req),
            Err(ValidationError::OpenCodeMissingFallback)
        );
    }

    #[test]
    fn opencode_fallback_is_keyed_not_positional() {
        let perms = OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("*".into(), Perm::Ask), ("git status".into(), Perm::Allow)],
            },
            unscoped_actions: Default::default(),
                ..Default::default()
        };
        assert_eq!(perms.bash_fallback(), Some(Perm::Ask));
    }

    #[test]
    fn codex_exclude_tmp() {
        let json = r#"{"sandboxWorkspaceWrite":{"excludeSlashTmp":true,"excludeTmpdirEnvVar":true}}"#;
        let v: CodexSandbox = serde_json::from_str(json).unwrap();
        assert!(v.sandbox_workspace_write.unwrap().exclude_slash_tmp);
        assert!(serde_json::from_str::<CodexSandbox>(r#"{"unknown":1}"#).is_err());
    }

    #[test]
    fn claude_allowed_tools() {
        let json = r#"{"allowedTools":["Read"],"disallowedTools":["Bash"],"additionalDirectories":["/tmp"],"settings":{"sandbox":{"enabled":true}}}"#;
        let v: ClaudeSandbox = serde_json::from_str(json).unwrap();
        assert_eq!(v.allowed_tools, vec!["Read"]);
        assert_eq!(v.additional_directories, vec!["/tmp"]);
    }

    #[test]
    fn opencode_external_directory() {
        let json = r#"{"bash":{"*":"ask"},"externalDirectory":"allow","read":"deny","edit":"ask"}"#;
        let v: OpenCodePerms = serde_json::from_str(json).unwrap();
        assert_eq!(v.external_directory, Some(Perm::Allow));
        assert_eq!(v.read, Some(Perm::Deny));
    }

    #[test]
    fn claude_sandbox_separates_read_and_write_lists() {
        let fs = FilesystemSandbox {
            allow_read: vec!["/repo/.env".into()],
            deny_write: vec!["/repo/.env".into()],
            ..Default::default()
        };
        assert!(fs.allow_read.contains(&"/repo/.env".to_string()));
        assert!(fs.deny_write.contains(&"/repo/.env".to_string()));
    }
}
