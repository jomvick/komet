use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const COMPOSIO_SERVER_ID: &str = "composio";
const COMPOSIO_KEY: &str = "x-consumer-api-key";
const COMPOSIO_PLATFORM_API_KEY: &str = "composio-platform-api-key";
const KEYRING_SERVICE: &str = "dev.komet.mcp";

trait CredentialBackend: Send + Sync {
    fn get(&self, account: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, account: &str, value: &str) -> anyhow::Result<()>;
    fn delete(&self, account: &str) -> anyhow::Result<()>;
}

struct SystemCredentialBackend;

impl CredentialBackend for SystemCredentialBackend {
    fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn set(&self, account: &str, value: &str) -> anyhow::Result<()> {
        keyring::Entry::new(KEYRING_SERVICE, account)?.set_password(value)?;
        Ok(())
    }

    fn delete(&self, account: &str) -> anyhow::Result<()> {
        match keyring::Entry::new(KEYRING_SERVICE, account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Local secret store for MCP servers.
///
/// Generic MCP values use an atomically replaced 0600 JSON file. Composio
/// credentials use the OS credential store; the Platform key is read only by
/// the Composio API client, while legacy MCP secrets are resolved for launch.
pub struct McpSecretStore {
    path: PathBuf,
    /// outer key = server id, inner = secret key -> value (header name or env var).
    /// Interior mutability: the store is shared as `Arc` between the sessions
    /// engine (resolve at run time) and the RPC surface (write-only upserts —
    /// secret values flow in via RPC, never back out).
    secrets: Mutex<HashMap<String, HashMap<String, String>>>,
    credentials: Arc<dyn CredentialBackend>,
}

impl Clone for McpSecretStore {
    /// Snapshot copy of the in-memory secrets (std `Mutex` is not `Clone`).
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            secrets: Mutex::new(self.lock_secrets().clone()),
            credentials: self.credentials.clone(),
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
            credentials: Arc::new(SystemCredentialBackend),
        };
        // Best-effort load; empty on failure.
        if let Err(error) = store.load() {
            tracing::warn!(error = %error, "mcp secrets could not be loaded");
        }
        store
    }

    #[cfg(test)]
    fn with_backend(path: impl AsRef<Path>, credentials: Arc<dyn CredentialBackend>) -> Self {
        let mut store = Self {
            path: path.as_ref().to_path_buf(),
            secrets: Mutex::new(HashMap::new()),
            credentials,
        };
        if let Err(error) = store.load() {
            tracing::warn!(error = %error, "mcp secrets could not be loaded");
        }
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
            credentials: Arc::new(SystemCredentialBackend),
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

    pub fn composio_platform_api_key(&self) -> anyhow::Result<Option<String>> {
        self.credentials.get(COMPOSIO_PLATFORM_API_KEY)
    }

    pub fn set_composio_platform_api_key(&self, value: &str) -> anyhow::Result<()> {
        let value = value.trim();
        if value.is_empty() {
            anyhow::bail!("Composio Platform API key cannot be empty");
        }
        self.credentials.set(COMPOSIO_PLATFORM_API_KEY, value)
    }

    pub fn delete_composio_platform_api_key(&self) -> anyhow::Result<()> {
        self.credentials.delete(COMPOSIO_PLATFORM_API_KEY)
    }

    fn load(&mut self) -> anyhow::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let data = std::fs::read_to_string(&self.path)?;
        if data.trim().is_empty() {
            return Ok(());
        }
        let mut parsed: HashMap<String, HashMap<String, String>> = serde_json::from_str(&data)?;
        let mut migrated = false;
        if let Some(value) = parsed
            .get_mut(COMPOSIO_SERVER_ID)
            .and_then(|secrets| secrets.remove(COMPOSIO_KEY))
        {
            self.credentials.set(COMPOSIO_KEY, &value)?;
            migrated = true;
        }
        if parsed
            .get(COMPOSIO_SERVER_ID)
            .is_some_and(HashMap::is_empty)
        {
            parsed.remove(COMPOSIO_SERVER_ID);
        }
        *self.lock_secrets() = parsed;
        if migrated {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        let parent = parent_dir(&self.path);
        std::fs::create_dir_all(parent)?;
        let secrets = self.lock_secrets();
        let data = serde_json::to_vec_pretty(&*secrets)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        temp.write_all(&data)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        let server_count = secrets.len();
        drop(secrets);
        tracing::debug!(
            path = %self.path.display(),
            keys = server_count,
            "mcp secrets saved (values masked)"
        );
        Ok(())
    }

    /// Store or update a secret for a server id.
    pub fn set_secret(&self, id: &str, key: &str, value: &str) -> anyhow::Result<()> {
        if is_keyring_secret(id, key) {
            self.credentials.set(key, value)?;
            let mut secrets = self.lock_secrets();
            if let Some(entry) = secrets.get_mut(id) {
                entry.remove(key);
                if entry.is_empty() {
                    secrets.remove(id);
                }
            }
            drop(secrets);
            self.save()?;
            tracing::debug!(server_id = %id, key = %key, "mcp secret stored in OS credential store");
            return Ok(());
        }
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
        let mut resolved = self.lock_secrets().get(id).cloned().unwrap_or_default();
        if id == COMPOSIO_SERVER_ID {
            match self.credentials.get(COMPOSIO_KEY) {
                Ok(Some(value)) => {
                    resolved.insert(COMPOSIO_KEY.to_string(), value);
                }
                Ok(None) => {}
                Err(_) => {
                    tracing::warn!(server_id = %id, "mcp credential could not be read from OS credential store")
                }
            }
        }
        resolved
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
        if is_keyring_secret(id, key) {
            self.credentials.delete(key)?;
        }
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

impl Default for McpSecretStore {
    fn default() -> Self {
        Self::with_default_path()
    }
}

fn is_keyring_secret(id: &str, key: &str) -> bool {
    id == COMPOSIO_SERVER_ID && key == COMPOSIO_KEY
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemoryCredentialBackend(Mutex<HashMap<String, String>>);

    impl CredentialBackend for MemoryCredentialBackend {
        fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }

        fn set(&self, account: &str, value: &str) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(account.to_string(), value.to_string());
            Ok(())
        }

        fn delete(&self, account: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[test]
    fn composio_credential_uses_backend_not_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-secrets.json");
        let store =
            McpSecretStore::with_backend(&path, Arc::new(MemoryCredentialBackend::default()));

        store
            .set_secret(COMPOSIO_SERVER_ID, COMPOSIO_KEY, "consumer-secret")
            .unwrap();

        assert_eq!(
            store.resolve(COMPOSIO_SERVER_ID)[COMPOSIO_KEY],
            "consumer-secret"
        );
        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(!persisted.contains("consumer-secret"));
        assert!(
            serde_json::from_str::<HashMap<String, HashMap<String, String>>>(&persisted)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn composio_platform_key_uses_os_credential_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-secrets.json");
        let backend = Arc::new(MemoryCredentialBackend::default());
        let store = McpSecretStore::with_backend(&path, backend.clone());

        store
            .set_composio_platform_api_key("platform-secret")
            .unwrap();
        assert_eq!(
            store.composio_platform_api_key().unwrap().as_deref(),
            Some("platform-secret")
        );
        assert!(
            !path.exists()
                || !std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("platform-secret")
        );

        store.delete_composio_platform_api_key().unwrap();
        assert_eq!(backend.get(COMPOSIO_PLATFORM_API_KEY).unwrap(), None);
    }

    #[test]
    fn secret_file_replacement_keeps_valid_json_and_restrictive_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-secrets.json");
        let store =
            McpSecretStore::with_backend(&path, Arc::new(MemoryCredentialBackend::default()));

        store.set_secret("other", "TOKEN", "value").unwrap();

        let persisted: HashMap<String, HashMap<String, String>> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["other"]["TOKEN"], "value");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
