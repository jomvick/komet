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
/// Fixed raw-event retention: the engine's maintenance tick prunes older
/// rows. Cursors only move forward, so pruning cannot resurrect.
pub const RETENTION_DAYS: u32 = 90;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MemoryConfig {
    /// Global kill switch. Default ON: the index is device-local and
    /// allow-listed, and the frictionless cross-agent handoff is the point.
    /// `false` stops recording, briefing injection and catch-up — stored
    /// data is kept (forget is a separate, explicit action).
    pub enabled: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl MemoryConfig {
    /// Load tolerantly, FIELD BY FIELD: a single malformed value must not
    /// reset the whole file to defaults — that would silently re-enable
    /// memory for a user who opted out. A wholly unreadable or non-object
    /// file still yields defaults.
    pub fn load(data_dir: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path(data_dir)) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            tracing::warn!("memory config corrupt; using defaults");
            return Self::default();
        };
        let enabled = value
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        Self { enabled }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_enabled() {
        assert_eq!(MemoryConfig::default(), MemoryConfig { enabled: true });
    }

    #[test]
    fn round_trip_preserves_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig { enabled: false };
        config.save(dir.path()).unwrap();
        assert_eq!(MemoryConfig::load(dir.path()), config);
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(MemoryConfig::load(dir.path()), MemoryConfig::default());
        std::fs::write(MemoryConfig::path(dir.path()), "{nope").unwrap();
        assert_eq!(MemoryConfig::load(dir.path()), MemoryConfig::default());
    }

    #[test]
    fn malformed_field_does_not_reset_enabled_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        // Legacy fields from older configs are ignored, not fatal.
        std::fs::write(
            MemoryConfig::path(dir.path()),
            r#"{"enabled": false, "retentionDays": "bogus", "location": 42, "embeddingsEnabled": true}"#,
        )
        .unwrap();
        let loaded = MemoryConfig::load(dir.path());
        assert!(!loaded.enabled);
    }
}
