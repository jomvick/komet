//! Device-level memory configuration — deliberately OUTSIDE the store
//! directory (`<data_dir>/memory-config.json`, not `<data_dir>/memory/`)
//! so relocating the database never loses the settings that describe it.
//!
//! The file is small, written atomically, and tolerant: a corrupt or
//! hand-edited file falls back to defaults field by field via serde.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "memory-config.json";
pub const DEFAULT_RETENTION_DAYS: u32 = 90;
/// Sanity bounds for a hand-edited retention value.
pub const RETENTION_DAYS_MIN: u32 = 1;
pub const RETENTION_DAYS_MAX: u32 = 3650;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MemoryConfig {
    /// Global kill switch. Default ON: the index is device-local and
    /// allow-listed, and the frictionless cross-agent handoff is the point.
    /// `false` stops recording, briefing injection and catch-up — stored
    /// data is kept (forget is a separate, explicit action).
    pub enabled: bool,
    /// Raw-event retention in days; the engine's maintenance tick prunes
    /// older rows. Cursors only move forward, so pruning cannot resurrect.
    pub retention_days: u32,
    /// Custom store location (absolute path). `None` = `<data_dir>/memory`.
    /// Applied at engine start: the store files are copied over before the
    /// first open, so a bad path can never corrupt a live database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: DEFAULT_RETENTION_DAYS,
            location: None,
        }
    }
}

impl MemoryConfig {
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<MemoryConfig>(&text) {
                Ok(mut config) => {
                    config.retention_days = config
                        .retention_days
                        .clamp(RETENTION_DAYS_MIN, RETENTION_DAYS_MAX);
                    config
                }
                Err(err) => {
                    tracing::warn!(error = %err, "memory config corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never
    /// leaves a torn config.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(CONFIG_FILE)
    }

    /// The directory the store actually lives in: the configured location
    /// (with a leading `~` expanded) or the default `<data_dir>/memory`.
    pub fn store_dir(&self, data_dir: &Path) -> PathBuf {
        match self
            .location
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(raw) => expand_home(raw),
            None => data_dir.join("memory"),
        }
    }
}

/// Resolve a configured location string exactly like [`MemoryConfig::store_dir`]
/// does for its `location` field (`~` expanded, trimmed).
pub fn resolve_location(location: &str) -> PathBuf {
    expand_home(location.trim())
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty())
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_enabled_with_default_retention() {
        let config = MemoryConfig::default();
        assert!(config.enabled);
        assert_eq!(config.retention_days, DEFAULT_RETENTION_DAYS);
        assert_eq!(config.location, None);
    }

    #[test]
    fn round_trip_preserves_location() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig {
            enabled: false,
            retention_days: 30,
            location: Some("/tmp/custom-memory".into()),
        };
        config.save(dir.path()).unwrap();
        let loaded = MemoryConfig::load(dir.path());
        assert_eq!(loaded, config);
        assert_eq!(
            loaded.store_dir(dir.path()),
            PathBuf::from("/tmp/custom-memory")
        );
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(MemoryConfig::load(dir.path()), MemoryConfig::default());
        std::fs::write(MemoryConfig::path(dir.path()), "{nope").unwrap();
        assert_eq!(MemoryConfig::load(dir.path()), MemoryConfig::default());
    }

    #[test]
    fn retention_is_clamped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(MemoryConfig::path(dir.path()), r#"{"retentionDays": 0}"#).unwrap();
        assert_eq!(
            MemoryConfig::load(dir.path()).retention_days,
            RETENTION_DAYS_MIN
        );
        std::fs::write(
            MemoryConfig::path(dir.path()),
            r#"{"retentionDays": 99999}"#,
        )
        .unwrap();
        assert_eq!(
            MemoryConfig::load(dir.path()).retention_days,
            RETENTION_DAYS_MAX
        );
    }

    #[test]
    fn store_dir_defaults_and_ignores_blank_location() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::default();
        assert_eq!(config.store_dir(dir.path()), dir.path().join("memory"));
        let blank = MemoryConfig {
            location: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(blank.store_dir(dir.path()), dir.path().join("memory"));
    }

    #[test]
    fn tilde_location_expands_against_home() {
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let resolved = resolve_location("~/komet-memory");
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(resolved, home.join("komet-memory"));
    }
}
