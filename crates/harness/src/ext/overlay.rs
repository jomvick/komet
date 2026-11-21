use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use komet_proto::HarnessId;

use crate::ext::model::{ExtItem, ExtKind};

/// Persisted per-(kind, agent, item) enabled flags.
///
/// Stored at `<data_dir>/agent-ext-overlay.json` (0600, atomic rename — same
/// discipline as `McpRegistry`). This file is the ONLY place komet records an
/// enable/disable decision for an agent-native extension; agent config files
/// are never written.
///
/// Resolution order for `enabled_for`:
/// 1. explicit overlay entry;
/// 2. the enabled flag the agent's own config declares (`ExtSource.enabled`);
/// 3. `true` — presence in an agent's config means enabled by default.
#[derive(Debug)]
pub struct ExtOverlay {
    path: PathBuf,
    /// `"{kind}|{agent}|{id}"` → enabled.
    entries: BTreeMap<String, bool>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct VersionedWrapper {
    version: u32,
    entries: BTreeMap<String, bool>,
}

fn key(kind: ExtKind, agent: HarnessId, id: &str) -> String {
    format!("{}|{}|{}", kind.as_str(), agent.as_str(), id)
}

impl ExtOverlay {
    /// In-memory empty overlay rooted at `path` (no file I/O).
    pub fn empty(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            entries: BTreeMap::new(),
        }
    }

    /// Load from `path`; missing or empty file starts empty. Malformed JSON is
    /// an error — silently resetting user toggles would be worse.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut entries = BTreeMap::new();
        if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            if !data.trim().is_empty() {
                let wrapper: VersionedWrapper = serde_json::from_str(&data)?;
                entries = wrapper.entries;
            }
        }
        Ok(Self { path, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Effective enabled state of `item` for `agent`. Returns `None` when the
    /// item is not available to that agent at all.
    pub fn enabled_for(&self, item: &ExtItem, agent: HarnessId) -> Option<bool> {
        if !item.available_to(agent) {
            return None;
        }
        Some(
            self.entries
                .get(&key(item.kind, agent, &item.id))
                .copied()
                .unwrap_or_else(|| item.hint_for(agent).unwrap_or(true)),
        )
    }

    /// Record an explicit decision and persist. `false` removes nothing — an
    /// explicit `false` must survive a later `true` hint change, so both
    /// values are stored.
    pub fn set_enabled(
        &mut self,
        kind: ExtKind,
        agent: HarnessId,
        id: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.entries.insert(key(kind, agent, id), enabled);
        self.persist()
    }

    /// Raw explicit decision for one item, if the user ever toggled it.
    pub fn decision(&self, kind: ExtKind, agent: HarnessId, id: &str) -> Option<bool> {
        self.entries.get(&key(kind, agent, id)).copied()
    }

    /// Whether the user has ANY explicit entry for `kind`+`agent` — launch
    /// code uses this to decide if komet must mediate that agent's native
    /// config at all (e.g. Claude's `--strict-mcp-config`).
    pub fn touches(&self, kind: ExtKind, agent: HarnessId) -> bool {
        let prefix = format!("{}|{}|", kind.as_str(), agent.as_str());
        self.entries.keys().any(|k| k.starts_with(&prefix))
    }

    /// Ids of `kind` items with an explicit `true` overlay entry for `agent`.
    /// Launch-time consumers use this to re-enable agent-native items the
    /// user switched on; absence of an entry never implies enabled.
    pub fn explicitly_enabled(
        &self,
        kind: ExtKind,
        agent: HarnessId,
    ) -> std::collections::BTreeSet<String> {
        let prefix = format!("{}|{}|", kind.as_str(), agent.as_str());
        self.entries
            .iter()
            .filter(|(_, enabled)| **enabled)
            .filter_map(|(k, _)| k.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    /// Drop the explicit entry so resolution falls back to the agent-native
    /// hint again ("reset to agent default").
    pub fn clear(&mut self, kind: ExtKind, agent: HarnessId, id: &str) -> anyhow::Result<()> {
        self.entries.remove(&key(kind, agent, id));
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let wrapper = VersionedWrapper {
            version: 1,
            entries: self.entries.clone(),
        };
        let data = serde_json::to_string_pretty(&wrapper)?;
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        std::fs::write(&tmp, &data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&tmp) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&tmp, perms);
            }
        }
        std::fs::rename(&tmp, &self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&self.path) {
                let mut perms = meta.permissions();
                if perms.mode() & 0o777 != 0o600 {
                    perms.set_mode(0o600);
                    let _ = std::fs::set_permissions(&self.path, perms);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::model::{ExtItem, ExtSource, Origin};

    fn item(id: &str) -> ExtItem {
        ExtItem {
            kind: ExtKind::Skill,
            id: id.into(),
            name: id.into(),
            origin: Origin::External,
            sources: vec![ExtSource {
                agent: HarnessId::Opencode,
                path: "/tmp/x".into(),
                enabled: Some(false),
            }],
        }
    }

    #[test]
    fn overlay_entry_wins_over_native_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.json");
        let mut overlay = ExtOverlay::load(&path).unwrap();
        let it = item("foo");
        // Native hint says disabled → default follows it.
        assert_eq!(overlay.enabled_for(&it, HarnessId::Opencode), Some(false));
        // Explicit overlay enable wins.
        overlay
            .set_enabled(ExtKind::Skill, HarnessId::Opencode, "foo", true)
            .unwrap();
        assert_eq!(overlay.enabled_for(&it, HarnessId::Opencode), Some(true));
        // Other agents: item not available → None.
        assert_eq!(overlay.enabled_for(&it, HarnessId::Codex), None);
    }

    #[test]
    fn clear_restores_native_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.json");
        let mut overlay = ExtOverlay::load(&path).unwrap();
        overlay
            .set_enabled(ExtKind::Skill, HarnessId::Opencode, "foo", true)
            .unwrap();
        overlay
            .clear(ExtKind::Skill, HarnessId::Opencode, "foo")
            .unwrap();
        assert_eq!(
            overlay.enabled_for(&item("foo"), HarnessId::Opencode),
            Some(false)
        );
    }

    #[test]
    fn persists_across_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.json");
        let mut overlay = ExtOverlay::load(&path).unwrap();
        overlay
            .set_enabled(ExtKind::McpServer, HarnessId::ClaudeCode, "gh", false)
            .unwrap();
        let reloaded = ExtOverlay::load(&path).unwrap();
        let it = ExtItem {
            kind: ExtKind::McpServer,
            id: "gh".into(),
            name: "gh".into(),
            origin: Origin::External,
            sources: vec![ExtSource {
                agent: HarnessId::ClaudeCode,
                path: "/home/u/.claude.json".into(),
                enabled: None,
            }],
        };
        assert_eq!(
            reloaded.enabled_for(&it, HarnessId::ClaudeCode),
            Some(false)
        );
    }
}
