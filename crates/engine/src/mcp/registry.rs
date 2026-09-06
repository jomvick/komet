use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mcp::config::{McpServerConfig, PublicMcpServerConfig};
use crate::mcp::secrets::McpSecretStore;

/// Resolved server ready for provider launch: config + resolved secret values.
/// Resolved values are in-memory only; never persisted via registry.
pub struct ResolvedMcpServer {
    pub config: McpServerConfig,
    pub resolved_headers: HashMap<String, String>,
    pub resolved_env: HashMap<String, String>,
}

impl std::fmt::Debug for ResolvedMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedMcpServer")
            .field("config", &self.config)
            .field(
                "resolved_headers",
                &format_args!("{{{} keys masked}}", self.resolved_headers.len()),
            )
            .field(
                "resolved_env",
                &format_args!("{{{} keys masked}}", self.resolved_env.len()),
            )
            .finish()
    }
}

/// Registry of external MCP servers. Persists to `<data_dir>/mcp-servers.json`.
/// Secrets are never exposed via `list_public`; resolved only via `resolve`.
pub struct McpRegistry {
    data_dir: PathBuf,
    file_path: PathBuf,
    servers: HashMap<String, McpServerConfig>,
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRegistry")
            .field("data_dir", &self.data_dir)
            .field("file_path", &self.file_path)
            .field(
                "servers",
                &format_args!("{{{} servers masked}}", self.servers.len()),
            )
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct VersionedWrapper {
    version: u32,
    servers: Vec<McpServerConfig>,
}

impl ResolvedMcpServer {
    /// Convert into a harness-launchable server config with resolved secrets in-memory.
    /// Secrets are present only in the returned `McpServerConfig` copy, never persisted.
    pub fn into_harness_config(self) -> McpServerConfig {
        let mut cfg = self.config.clone();
        // Resolved values replace placeholders for provider launch.
        if !self.resolved_headers.is_empty() {
            cfg.headers = self.resolved_headers;
        }
        if !self.resolved_env.is_empty() {
            cfg.env = self.resolved_env;
        }
        cfg
    }

    /// Public view for UI / logging — no secret values.
    pub fn public_view(&self) -> PublicMcpServerConfig {
        self.config.public_view()
    }
}

impl McpRegistry {
    /// Create an empty registry rooted at `data_dir` (no file I/O).
    pub fn empty(data_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref().to_path_buf();
        let file_path = Self::file_path_for(&data_dir);
        Self {
            data_dir,
            file_path,
            servers: HashMap::new(),
        }
    }

    fn file_path_for(data_dir: &Path) -> PathBuf {
        // If data_dir looks like a file path ending with .json, use it directly
        // to remain flexible; otherwise join mcp-servers.json.
        if data_dir.extension().is_some_and(|e| e == "json") {
            data_dir.to_path_buf()
        } else {
            data_dir.join("mcp-servers.json")
        }
    }

    /// Load registry from `<data_dir>/mcp-servers.json`. Creates empty if missing.
    pub fn load(data_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let file_path = Self::file_path_for(&data_dir);
        let mut servers: HashMap<String, McpServerConfig> = HashMap::new();
        if file_path.exists() {
            let data = std::fs::read_to_string(&file_path)?;
            if !data.trim().is_empty() {
                // Prefer versioned wrapper {"version":1,"servers":[...]} for new files;
                // fallback to bare Vec and HashMap for migration.
                if let Ok(wrapped) = serde_json::from_str::<VersionedWrapper>(&data) {
                    for cfg in wrapped.servers {
                        if !cfg.id.trim().is_empty() {
                            servers.insert(cfg.id.clone(), cfg);
                        }
                    }
                } else if let Ok(vec) = serde_json::from_str::<Vec<McpServerConfig>>(&data) {
                    for cfg in vec {
                        // Don't validate on load; allow persisted invalid to be filtered at resolve.
                        // But skip empty ids to avoid map corruption.
                        if !cfg.id.trim().is_empty() {
                            servers.insert(cfg.id.clone(), cfg);
                        }
                    }
                } else {
                    // Try HashMap format
                    let parsed_map: HashMap<String, McpServerConfig> = serde_json::from_str(&data)?;
                    for (k, cfg) in parsed_map {
                        if cfg.id.trim().is_empty() {
                            continue;
                        }
                        servers.insert(k, cfg);
                    }
                }
            }
            tracing::debug!(
                server_count = servers.len(),
                path = %file_path.display(),
                "mcp registry loaded (values masked)"
            );
        } else {
            tracing::debug!(path = %file_path.display(), "mcp registry file not found, starting empty");
        }
        Ok(Self {
            data_dir,
            file_path,
            servers,
        })
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.file_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // Persist as versioned wrapper for determinism and migration support
        let mut vec: Vec<&McpServerConfig> = self.servers.values().collect();
        vec.sort_by(|a, b| a.id.cmp(&b.id));
        #[derive(Serialize)]
        struct PersistWrapper<'a> {
            version: u32,
            servers: Vec<&'a McpServerConfig>,
        }
        let wrapper = PersistWrapper {
            version: 1,
            servers: vec,
        };
        let data = serde_json::to_string_pretty(&wrapper)?;
        // Atomic write: temp file + rename
        let tmp_path = {
            let p = self.file_path.clone();
            // append .tmp to avoid colliding with with_extension logic
            let s = p.to_string_lossy().to_string() + ".tmp";
            PathBuf::from(s)
        };
        std::fs::write(&tmp_path, &data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&tmp_path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&tmp_path, perms);
            }
        }
        std::fs::rename(&tmp_path, &self.file_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&self.file_path) {
                let mut perms = meta.permissions();
                if perms.mode() & 0o777 != 0o600 {
                    perms.set_mode(0o600);
                    let _ = std::fs::set_permissions(&self.file_path, perms);
                }
            }
        }
        tracing::debug!(
            server_count = self.servers.len(),
            path = %self.file_path.display(),
            "mcp registry persisted (values masked)"
        );
        Ok(())
    }

    /// Validate and upsert a server config. Persists to disk.
    pub fn save(&mut self, config: &McpServerConfig) -> anyhow::Result<()> {
        config.validate()?;
        let id = config.id.clone();
        self.servers.insert(id.clone(), config.clone());
        self.persist()?;
        tracing::debug!(server_id = %id, "mcp server saved (values masked)");
        Ok(())
    }

    /// Alias for save — add new server.
    pub fn add(&mut self, config: McpServerConfig) -> anyhow::Result<()> {
        self.save(&config)
    }

    /// Alias for save — update existing server.
    pub fn update(&mut self, config: McpServerConfig) -> anyhow::Result<()> {
        self.save(&config)
    }

    /// Delete a server by id. No-op if missing. Persists.
    pub fn delete(&mut self, id: &str) -> anyhow::Result<()> {
        let removed = self.servers.remove(id).is_some();
        if removed {
            self.persist()?;
            tracing::debug!(server_id = %id, "mcp server deleted");
        } else {
            tracing::debug!(server_id = %id, "mcp server delete: not found");
        }
        Ok(())
    }

    /// Set enabled flag for a server. Persists.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> anyhow::Result<()> {
        let entry = self.servers.get_mut(id);
        match entry {
            Some(cfg) => {
                cfg.enabled = enabled;
                self.persist()?;
                tracing::debug!(server_id = %id, enabled = enabled, "mcp server set_enabled");
                Ok(())
            }
            None => anyhow::bail!("server not found: {}", id),
        }
    }

    /// Public view — no secret values. Sorted by id.
    pub fn list_public(&self) -> Vec<PublicMcpServerConfig> {
        let mut out: Vec<PublicMcpServerConfig> =
            self.servers.values().map(|c| c.public_view()).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        tracing::debug!(count = out.len(), "mcp list_public (values masked)");
        out
    }

    /// Resolve servers by ids: filter enabled + valid, resolve secrets via store.
    /// Logs are masked (only server_id, no header/env dumps).
    pub fn resolve(
        &self,
        ids: &[String],
        store: &McpSecretStore,
    ) -> Vec<ResolvedMcpServer> {
        let mut out = Vec::new();
        for id in ids {
            let Some(cfg) = self.servers.get(id) else {
                tracing::debug!(server_id = %id, "mcp resolve: not found (masked)");
                continue;
            };
            if !cfg.enabled {
                tracing::debug!(server_id = %id, "mcp resolve: filtered disabled");
                continue;
            }
            if let Err(e) = cfg.validate() {
                tracing::debug!(server_id = %id, error = %e, "mcp resolve: filtered invalid (masked)");
                continue;
            }
            // Resolve via store, filtered by config's declared keys
            let resolved_headers = store.get_resolved_headers_for(id, &cfg.headers);
            let resolved_env = store.get_resolved_env_for(id, &cfg.env);
            tracing::debug!(
                server_id = %id,
                headers = resolved_headers.len(),
                env = resolved_env.len(),
                "mcp resolve: ok (values masked)"
            );
            out.push(ResolvedMcpServer {
                config: cfg.clone(),
                resolved_headers,
                resolved_env,
            });
        }
        out
    }

    /// For diagnostics only — path (masked values not included).
    pub fn path(&self) -> &Path {
        &self.file_path
    }

    /// Number of servers.
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}
