use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Local secret store for MCP servers.
///
/// Persists to `~/.komet/mcp-secrets.json` (chmod 600) or any path provided.
/// OS keychain fallback is noted but not required for Task 1 — file store is
/// the canonical implementation. Secrets never leave this store except via
/// `resolve` for in-memory provider launch.
#[derive(Clone, Default)]
pub struct McpSecretStore {
    path: PathBuf,
    /// outer key = server id, inner = secret key -> value (header name or env var)
    secrets: HashMap<String, HashMap<String, String>>,
}

impl std::fmt::Debug for McpSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSecretStore")
            .field("path", &self.path)
            .field("secrets", &format_args!("{{{} servers masked}}", self.secrets.len()))
            .finish()
    }
}

impl McpSecretStore {
    /// Create a store backed by the given path. Loads existing file if present.
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut store = Self {
            path,
            secrets: HashMap::new(),
        };
        // Best-effort load; empty on failure.
        let _ = store.load();
        store
    }

    /// Ephemeral in-memory store for tests (no file I/O on drop unless set_secret called with temp path).
    pub fn ephemeral() -> Self {
        Self {
            path: PathBuf::from("/tmp/komet-mcp-secrets-ephemeral.json"),
            secrets: HashMap::new(),
        }
    }

    /// Default path: `~/.komet/mcp-secrets.json` or `$KOMET_DATA_DIR` if set, falling back to temp.
    pub fn default_path() -> PathBuf {
        if let Ok(dir) = std::env::var("KOMET_DATA_DIR") {
            return PathBuf::from(dir).join("mcp-secrets.json");
        }
        if let Some(home) = dirs::home_dir() {
            return home.join(".komet").join("mcp-secrets.json");
        }
        PathBuf::from("/tmp/komet-mcp-secrets.json")
    }

    /// Create with default path and load.
    pub fn with_default_path() -> Self {
        Self::new(Self::default_path())
    }

    fn load(&mut self) -> anyhow::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let data = std::fs::read_to_string(&self.path)?;
        if data.trim().is_empty() {
            return Ok(());
        }
        let parsed: HashMap<String, HashMap<String, String>> = serde_json::from_str(&data)?;
        self.secrets = parsed;
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&self.secrets)?;
        std::fs::write(&self.path, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&self.path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&self.path, perms)?;
        }
        tracing::debug!(path = %self.path.display(), keys = self.secrets.len(), "mcp secrets saved (values masked)");
        Ok(())
    }

    /// Store or update a secret for a server id.
    pub fn set_secret(&mut self, id: &str, key: &str, value: &str) -> anyhow::Result<()> {
        let entry = self.secrets.entry(id.to_string()).or_default();
        entry.insert(key.to_string(), value.to_string());
        // Only persist if path is not ephemeral placeholder without parent? always try.
        // For ephemeral tests with tempdir, path will be overwritten to temp file; still persist.
        // If path is the hardcoded ephemeral tmp file, we allow save as well (isolated).
        self.save()?;
        tracing::debug!(server_id = %id, key = %key, "mcp secret set (value masked)");
        Ok(())
    }

    /// Resolve all secrets for a server id (headers + env merged — caller distinguishes via config).
    pub fn resolve(&self, id: &str) -> HashMap<String, String> {
        self.secrets.get(id).cloned().unwrap_or_default()
    }

    /// Alias for header resolution — same underlying store.
    pub fn get_resolved_headers(&self, id: &str) -> HashMap<String, String> {
        self.resolve(id)
    }

    /// Resolve env secrets for a server id.
    pub fn get_resolved_env(&self, id: &str) -> HashMap<String, String> {
        self.resolve(id)
    }

    /// Remove a secret.
    pub fn remove_secret(&mut self, id: &str, key: &str) -> anyhow::Result<()> {
        if let Some(map) = self.secrets.get_mut(id) {
            map.remove(key);
            if map.is_empty() {
                self.secrets.remove(id);
            }
        }
        self.save()?;
        Ok(())
    }

    /// For Task 2 registry integration: return path for diagnostics (masked).
    pub fn path(&self) -> &Path {
        &self.path
    }
}
