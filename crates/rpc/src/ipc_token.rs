//! The token that authenticates connections to the local engine port.
//!
//! Any local OS user can connect to 127.0.0.1, so the port alone does not say
//! who is calling. At every launch the engine writes a fresh random token to a
//! file only its own user can read, and refuses handshakes that do not present
//! it. Clients of the same user read that file.
//!
//! The file lives in a per-user runtime directory and is named after the port,
//! not placed in `KOMET_DATA_DIR`: a UI and a daemon may use different data
//! directories while sharing one port (see `scripts/dev-demo.sh`).
//!
//! On Windows the file is not given explicit permissions; it relies on the
//! user profile directory being private to that user.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The per-user directory for token files: `KOMET_RUNTIME_DIR` when set,
/// otherwise `.komet/run` in the home directory.
pub fn default_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("KOMET_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no home directory for the engine token",
            )
        })?;
    Ok(PathBuf::from(home).join(".komet").join("run"))
}

/// Location of the token file for the engine listening on `port`.
pub fn path(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("ipc-{port}.token"))
}

/// A token file written by a running engine. Dropping it (engine stopped or
/// its server task aborted) deletes the file, unless a newer engine on the same
/// port has already replaced it.
#[derive(Debug)]
pub struct PublishedToken {
    path: PathBuf,
    token: String,
}

impl PublishedToken {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for PublishedToken {
    fn drop(&mut self) {
        let still_ours =
            std::fs::read_to_string(&self.path).is_ok_and(|current| current.trim() == self.token);
        if still_ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Generate a new token for `port` and write it, replacing any previous one.
/// The directory is made owner-only and the file is created with owner-only
/// permissions, so the token is never readable by other users, even briefly.
/// Call this only after binding the port, so an engine that failed to bind
/// never replaces the token of the engine that owns the port.
pub fn publish(dir: &Path, port: u16) -> io::Result<PublishedToken> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // Two version 4 UUIDs: 244 random bits.
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let final_path = path(dir, port);
    let temp_path = final_path.with_extension("tmp");
    let _ = std::fs::remove_file(&temp_path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temp_path, &final_path)?;
    Ok(PublishedToken {
        path: final_path,
        token,
    })
}

/// Read the token of the engine listening on `port`.
pub fn read(dir: &Path, port: u16) -> io::Result<String> {
    let file = path(dir, port);
    let token = std::fs::read_to_string(&file).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "cannot read the engine token at {} ({err}); is komet running as this user?",
                file.display()
            ),
        )
    })?;
    Ok(token.trim().to_string())
}

/// The port of a `ws://` URL that points at this machine (`127.0.0.1`,
/// `localhost` or `[::1]`), or `None` for any other URL. Tools that take a URL
/// from the command line use this before sending a token, so the token never
/// leaves the machine.
pub fn loopback_port(url: &str) -> Option<u16> {
    let authority = url.strip_prefix("ws://")?.trim_end_matches('/');
    if authority.contains(['/', '@', '?', '#']) {
        return None;
    }
    let (host, port) = authority.rsplit_once(':')?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    port.parse().ok()
}

/// Read the token of the engine on `port` from [`default_dir`].
pub fn read_default(port: u16) -> io::Result<String> {
    read(&default_dir()?, port)
}

/// Compare two tokens without stopping at the first difference, so response
/// time does not reveal how much of a guessed token was correct.
pub(crate) fn matches(presented: &[u8], expected: &[u8]) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .iter()
        .zip(expected)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}
