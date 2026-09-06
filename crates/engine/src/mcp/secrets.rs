use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Local secret store for MCP servers.
///
/// Persists to `~/.komet/mcp-secrets.json` (chmod 600) or any path provided.
/// OS keychain fallback is noted but not required for Task 1 — file store is
/// the canonical implementation. Secrets never leave this store except via
/// `resolve` for in-memory provider launch.
#[derive(Default)]
pub struct McpSecretStore {
    path: PathBuf,
    /// outer key = server id, inner = secret key -> value (header name or env var).
    /// Interior mutability: the store is shared as `Arc` between the sessions
    /// engine (resolve at run time) and the RPC surface (write-only upserts —
    /// secret values flow in via RPC, never back out).
    secrets: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl Clone for McpSecretStore {
    /// Snapshot copy of the in-memory secrets (std `Mutex` is not `Clone`).
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            secrets: Mutex::new(self.lock_secrets().clone()),
        }
    }
}

impl std::fmt::Debug for McpSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSecretStore")
            .field("path", &self.path)
            .field(
                "secrets",
                &format_args!("{{{} servers masked}}", self.lock_secrets().len()),
            )
            .finish()
    }
}

impl McpSecretStore {
    /// Lock the inner map, recovering from poisoning (a panicked peer must not
    /// wedge the MCP surface; individual entries remain valid).
    fn lock_secrets(&self) -> std::sync::MutexGuard<'_, HashMap<String, HashMap<String, String>>> {
        self.secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Create a store backed by the given path. Loads existing file if present.
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut store = Self {
            path,
            secrets: Mutex::new(HashMap::new()),
        };
        // Best-effort load; empty on failure.
        let _ = store.load();
        store
    }

    /// Ephemeral in-memory store for tests — uses a unique temp path (pid + nanos) to avoid collisions
    /// across parallel test processes/instances. No file I/O until `set_secret`.
    pub fn ephemeral() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        // Use thread id hash for extra uniqueness within same process
        let tid = format!("{:?}", std::thread::current().id());
        let tid_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            tid.hash(&mut h);
            h.finish()
        };
        let path = std::env::temp_dir().join(format!(
            "komet-mcp-secrets-ephemeral-{}-{}-{}.json",
            pid, tid_hash, nanos
        ));
        Self {
            path,
            secrets: Mutex::new(HashMap::new()),
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
        *self.lock_secrets() = parsed;
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&*self.lock_secrets())?;
        std::fs::write(&self.path, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&self.path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&self.path, perms)?;
        }
        tracing::debug!(
            path = %self.path.display(),
            keys = self.lock_secrets().len(),
            "mcp secrets saved (values masked)"
        );
        Ok(())
    }

    /// Store or update a secret for a server id.
    pub fn set_secret(&self, id: &str, key: &str, value: &str) -> anyhow::Result<()> {
        {
            let mut secrets = self.lock_secrets();
            let entry = secrets.entry(id.to_string()).or_default();
            entry.insert(key.to_string(), value.to_string());
        }
        // Only persist if path is not ephemeral placeholder without parent? always try.
        // For ephemeral tests with tempdir, path will be overwritten to temp file; still persist.
        // If path is the hardcoded ephemeral tmp file, we allow save as well (isolated).
        self.save()?;
        tracing::debug!(server_id = %id, key = %key, "mcp secret set (value masked)");
        Ok(())
    }

    /// Resolve all secrets for a server id (headers + env merged — caller distinguishes via config).
    pub fn resolve(&self, id: &str) -> HashMap<String, String> {
        self.lock_secrets().get(id).cloned().unwrap_or_default()
    }

    /// Alias for header resolution — currently same underlying merged map.
    /// Caller should filter by `config.headers` keys (see `get_resolved_headers_for`).
    /// Kept for backward compat; partitioned storage will be introduced in Task 5.
    pub fn get_resolved_headers(&self, id: &str) -> HashMap<String, String> {
        self.resolve(id)
    }

    /// Alias for env resolution — currently same underlying merged map.
    /// Caller should filter by `config.env` keys (see `get_resolved_env_for`).
    pub fn get_resolved_env(&self, id: &str) -> HashMap<String, String> {
        self.resolve(id)
    }

    /// Filtered header resolution: returns only keys present in `config_headers`.
    pub fn get_resolved_headers_for(
        &self,
        id: &str,
        config_headers: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        let all = self.resolve(id);
        all.into_iter()
            .filter(|(k, _)| config_headers.contains_key(k))
            .collect()
    }

    /// Filtered env resolution: returns only keys present in `config_env`.
    pub fn get_resolved_env_for(
        &self,
        id: &str,
        config_env: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        let all = self.resolve(id);
        all.into_iter()
            .filter(|(k, _)| config_env.contains_key(k))
            .collect()
    }

    /// Remove a secret.
    pub fn remove_secret(&self, id: &str, key: &str) -> anyhow::Result<()> {
        {
            let mut secrets = self.lock_secrets();
            if let Some(map) = secrets.get_mut(id) {
                map.remove(key);
                if map.is_empty() {
                    secrets.remove(id);
                }
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
