//! Persistent list of remote environments (`{data_dir}/environments.json`).
//!
//! Holds bearer tokens, so the file is 0600 and written atomically: a half-written
//! environments file would drop every environment (and its token) on next boot.

use std::path::{Path, PathBuf};

use komet_proto::Environment;
use serde::{Deserialize, Serialize};

/// Environment id reserved for the workspace on this machine.
pub const LOCAL_ENV_ID: &str = "local";

const FILE_NAME: &str = "environments.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    /// The same `(url, token)` is already stored; carries the existing id.
    Existing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvStoreError {
    /// `"local"` is the machine's own workspace and cannot be removed.
    Reserved,
    NotFound,
}

impl std::fmt::Display for EnvStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reserved => write!(f, "the local workspace is not a remote environment"),
            Self::NotFound => write!(f, "no such environment"),
        }
    }
}

impl std::error::Error for EnvStoreError {}

#[derive(Debug, Serialize, Deserialize)]
struct FileBody {
    v: u32,
    environments: Vec<Environment>,
}

/// The environment list on disk. Tokens live here, so the file is 0600 and every
/// write is a temp file + rename.
#[derive(Debug, Clone)]
pub struct EnvironmentStore {
    path: PathBuf,
    envs: Vec<Environment>,
}

impl EnvironmentStore {
    pub fn load(data_dir: &Path) -> Result<Self, std::io::Error> {
        let path = data_dir.join(FILE_NAME);
        let envs = match std::fs::read(&path) {
            Ok(bytes) => {
                let body: FileBody = serde_json::from_slice(&bytes)
                    .map_err(|err| std::io::Error::other(format!("{}: {err}", path.display())))?;
                if body.v != FILE_VERSION {
                    return Err(std::io::Error::other(format!(
                        "{}: unsupported environments file version {}",
                        path.display(),
                        body.v
                    )));
                }
                body.environments
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        Ok(Self { path, envs })
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let body = FileBody {
            v: FILE_VERSION,
            environments: self.envs.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&body).map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        write_private(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)
    }

    pub fn list(&self) -> &[Environment] {
        &self.envs
    }

    pub fn get(&self, id: &str) -> Option<&Environment> {
        self.envs.iter().find(|env| env.id == id)
    }

    /// Add or refresh an environment. The same server with the same token is the same
    /// environment, whatever id the link carried: pairing twice must not fork one host.
    pub fn upsert(&mut self, mut env: Environment) -> AddOutcome {
        normalize(&mut env);
        if let Some(existing) = self
            .envs
            .iter()
            .find(|e| e.url == env.url && e.token == env.token)
        {
            return AddOutcome::Existing(existing.id.clone());
        }
        self.envs.retain(|e| e.id != env.id);
        self.envs.push(env);
        AddOutcome::Added
    }

    pub fn remove(&mut self, id: &str) -> Result<Environment, EnvStoreError> {
        if id == LOCAL_ENV_ID {
            return Err(EnvStoreError::Reserved);
        }
        let index = self
            .envs
            .iter()
            .position(|env| env.id == id)
            .ok_or(EnvStoreError::NotFound)?;
        Ok(self.envs.remove(index))
    }
}

fn normalize(env: &mut Environment) {
    env.url = env.url.trim().trim_end_matches('/').to_string();
    env.token = env.token.trim().to_string();
    env.label = env.label.trim().to_string();
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(id: &str, url: &str, token: &str) -> komet_proto::Environment {
        komet_proto::Environment {
            id: id.into(),
            label: id.into(),
            url: url.into(),
            token: token.into(),
            namespace: None,
            added_at_ms: 1,
        }
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = EnvironmentStore::load(dir.path()).unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn round_trips_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EnvironmentStore::load(dir.path()).unwrap();
        assert_eq!(
            store.upsert(env("a", "https://one.example", "t1")),
            AddOutcome::Added
        );
        store.save().unwrap();
        let reloaded = EnvironmentStore::load(dir.path()).unwrap();
        assert_eq!(reloaded.list(), store.list());
        assert_eq!(reloaded.get("a").unwrap().token, "t1");
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut store = EnvironmentStore::load(dir.path()).unwrap();
        store.upsert(env("a", "https://one.example", "t1"));
        store.save().unwrap();
        let mode = std::fs::metadata(dir.path().join("environments.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn upsert_is_idempotent_on_url_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EnvironmentStore::load(dir.path()).unwrap();
        assert_eq!(
            store.upsert(env("a", "https://one.example", "t1")),
            AddOutcome::Added
        );
        // Same server, trailing slash and padded token: the same environment.
        assert_eq!(
            store.upsert(env("b", " https://one.example/ ", " t1 ")),
            AddOutcome::Existing("a".into())
        );
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].token, "t1");
    }

    #[test]
    fn remove_refuses_local_and_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EnvironmentStore::load(dir.path()).unwrap();
        store.upsert(env("a", "https://one.example", "t1"));
        assert_eq!(store.remove("local"), Err(EnvStoreError::Reserved));
        assert_eq!(store.remove("nope"), Err(EnvStoreError::NotFound));
        assert_eq!(store.remove("a").map(|e| e.id), Ok("a".to_string()));
        assert!(store.list().is_empty());
    }

    #[test]
    fn load_rejects_unknown_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("environments.json"),
            r#"{"v":99,"environments":[]}"#,
        )
        .unwrap();
        assert!(EnvironmentStore::load(dir.path()).is_err());
    }
}
