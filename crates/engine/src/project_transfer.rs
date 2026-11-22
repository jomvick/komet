//! Project transfer (`ProjectTransfer*`): preview, export and import of one
//! project folder between workspaces (this computer ⇄ a paired machine, or two
//! paired machines).
//!
//! The project is COPIED, never moved, and its chats stay behind — a chat
//! belongs to the workspace that hosts it. Two transfer modes exist:
//!
//! - **git** (preferred): the destination clones the source's remote at the
//!   current branch. Komet moves no bytes; only committed+ pushed work arrives.
//! - **archive**: a `.tar.zst` is built on the source and streamed to the
//!   destination in chunks. `.git` is included by default so the history and
//!   uncommitted work survive.
//!
//! Exclusions: whatever `.gitignore` rules out (`node_modules`, `target`, …)
//! plus a secret list (`.env*`, `*.pem`, `.git-credentials`, SSH keys) that only
//! an explicit opt-in can include. The destination extraction refuses absolute
//! paths, `..` components and symlinks that escape the target, and never
//! overwrites an existing folder silently.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::EngineError;

/// A file the plan will carry (path is relative to the project root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferFile {
    pub path: String,
    pub bytes: u64,
    /// Matched the secret list — carried only with `includeSecrets`.
    pub secret: bool,
}

/// Why the transfer may not do what the user expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferWarning {
    /// The source checkout has uncommitted changes: a git-mode import loses
    /// them.
    DirtyCheckout,
    /// The repository has no remote: git mode is impossible.
    NoRemote,
    /// Some files matched the secret list and will be skipped.
    SecretsExcluded,
    /// Ignored files (build outputs, `node_modules`) exist and stay behind.
    IgnoredFilesExcluded,
    /// The project is large; the user should confirm.
    LargeProject,
    /// macOS/Windows destination: two names differ only by case, which collides
    /// on a case-insensitive filesystem.
    CaseCollision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    pub remote: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTransferPlan {
    /// Absolute source folder on the source device.
    pub path: String,
    /// Project name (last path component).
    pub name: String,
    pub files: u64,
    pub bytes: u64,
    /// Total files skipped because `.gitignore` excluded them.
    pub ignored_files: u64,
    /// Secret-looking files that stay behind unless `includeSecrets`.
    pub secrets: Vec<TransferFile>,
    pub git: Option<GitInfo>,
    pub warnings: Vec<TransferWarning>,
    /// Git mode is possible (remote present).
    pub git_ready: bool,
    /// Archive/plan body still fits the transfer's size ceiling.
    pub within_limit: bool,
}

/// A project over this many bytes invites an explicit confirmation.
const LARGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Hard ceiling for one archive transfer (the plan refuses above it).
const TRANSFER_LIMIT_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Names/patterns that look like credentials. Deliberately simple and
/// over-inclusive: a false positive only costs one confirmation, a false
/// negative leaks a key.
pub fn is_secret_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if lower == ".git-credentials"
        || lower == "id_rsa"
        || lower == "id_ed25519"
        || lower == "id_ecdsa"
        || lower == "id_dsa"
        || lower == ".netrc"
        || lower == ".pgpass"
    {
        return true;
    }
    if lower.starts_with(".env") {
        return true;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()),
        Some(ext) if ext == "pem" || ext == "key" || ext == "p12" || ext == "pfx"
    )
}

/// True when `path` is a `.git/config` file whose contents look credentialed:
/// a URL with `user:password@` userinfo, or an `extraHeader` key (which git
/// uses for Authorization headers). Read is bounded — config files are tiny.
fn git_config_has_credentials(path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) != Some("config")
        || path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            != Some(".git")
    {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut text = String::new();
    use std::io::Read as _;
    if file.take(256 * 1024).read_to_string(&mut text).is_err() {
        return false;
    }
    text.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            return false;
        }
        let key = line.split('=').next().unwrap_or("").trim();
        if key.to_ascii_lowercase().ends_with("extraheader") {
            return true;
        }
        has_credentialed_url(line)
    })
}

/// `scheme://user:password@host` anywhere in the line. A bare `user@host`
/// (no password) is not a secret.
fn has_credentialed_url(line: &str) -> bool {
    let mut rest = line;
    while let Some(idx) = rest.find("://") {
        rest = &rest[idx + 3..];
        let end = rest.find(['/', '?', '#', ' ', '\t']).unwrap_or(rest.len());
        let authority = &rest[..end];
        if let Some(at) = authority.rfind('@')
            && authority[..at].contains(':')
        {
            return true;
        }
        rest = &rest[end..];
    }
    false
}

/// Walk `root` applying `.gitignore` + the secret list and return the plan.
///
/// Git metadata and the dirty flag come from the `git` CLI (the engine already
/// shells out for repository work); a missing git binary simply means no git
/// mode, never a failed plan.
pub fn plan(path: &Path) -> Result<ProjectTransferPlan, EngineError> {
    let path = path
        .canonicalize()
        .map_err(|err| EngineError::Other(format!("project path: {err}")))?;
    if !path.is_dir() {
        return Err(EngineError::Other(format!(
            "not a folder: {}",
            path.display()
        )));
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut ignored_files = 0u64;
    let mut secrets: Vec<TransferFile> = Vec::new();
    let mut names_seen: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut case_collision = false;

    let mut builder = ignore::WalkBuilder::new(&path);
    builder
        .hidden(false) // dotfiles ARE part of a project (.env is caught below)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        // Honour `.gitignore` even for a folder that is not (yet) a git repo:
        // projects are often transferred before their first commit.
        .require_git(false)
        .follow_links(false);
    // Ignored entries are reported, not silently dropped: "my node_modules
    // didn't come along" must be explained in the dialog.
    builder.add_custom_ignore_filename(".kometignore");

    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue, // unreadable subtree: skip, the plan still works
        };
        if entry.depth() == 0 {
            continue;
        }
        let rel = match entry.path().strip_prefix(&path) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let ignored = entry.path().components().any(|c| c.as_os_str() == ".git");
        if ignored && entry.file_type().is_some_and(|t| t.is_dir()) {
            // `.git` is carried (history + uncommitted work): count its files
            // without re-walking ignore rules, which would drop them.
            let (git_files, git_bytes) = walk_git_dir(entry.path());
            files += git_files;
            bytes += git_bytes;
            // `.git/config` is the one file whose CONTENTS can be a secret
            // even though its name is not: a remote URL may embed
            // `user:token@host`, and `http.*.extraHeader` may carry an
            // Authorization value. Surface it like any other secret so the
            // dialog warns and the archive skips it without opt-in.
            let config = entry.path().join("config");
            if git_config_has_credentials(&config) {
                let size = std::fs::metadata(&config).map(|m| m.len()).unwrap_or(0);
                files = files.saturating_sub(1);
                bytes = bytes.saturating_sub(size);
                secrets.push(TransferFile {
                    path: rel.join("config").to_string_lossy().replace('\\', "/"),
                    bytes: size,
                    secret: true,
                });
            }
            continue;
        }
        if ignored {
            continue;
        }
        if is_dir {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let size = metadata.len();
        if is_secret_path(rel) {
            secrets.push(TransferFile {
                path: rel.to_string_lossy().replace('\\', "/"),
                bytes: size,
                secret: true,
            });
            continue;
        }
        files += 1;
        bytes += size;
        // Case-insensitive filesystem collision (macOS/Windows destination).
        let key = rel.to_string_lossy().to_lowercase();
        if let Some(previous) = names_seen.insert(key, rel.to_string_lossy().to_string())
            && previous != rel.to_string_lossy()
        {
            case_collision = true;
        }
    }
    // Ignored files: a second, cheap walk that only counts what the first
    // skipped, so the dialog can say "N files (build outputs) stay behind".
    for entry in ignore::WalkBuilder::new(&path)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .build()
        .flatten()
    {
        if entry.depth() == 0 {
            continue;
        }
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        if entry
            .path()
            .strip_prefix(&path)
            .is_ok_and(|rel| !rel.components().any(|c| c.as_os_str() == ".git"))
        {
            ignored_files += 1;
        }
    }

    let git = git_info(&path);
    let mut warnings = Vec::new();
    if let Some(git) = &git {
        if git.remote.is_none() {
            warnings.push(TransferWarning::NoRemote);
        }
        if git.dirty {
            warnings.push(TransferWarning::DirtyCheckout);
        }
    } else {
        warnings.push(TransferWarning::NoRemote);
    }
    if !secrets.is_empty() {
        warnings.push(TransferWarning::SecretsExcluded);
    }
    if ignored_files > 0 {
        warnings.push(TransferWarning::IgnoredFilesExcluded);
    }
    if bytes > LARGE_BYTES {
        warnings.push(TransferWarning::LargeProject);
    }
    if case_collision {
        warnings.push(TransferWarning::CaseCollision);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    Ok(ProjectTransferPlan {
        path: path.to_string_lossy().to_string(),
        name,
        files,
        bytes,
        ignored_files,
        secrets,
        git_ready: git.as_ref().is_some_and(|g| g.remote.is_some()),
        git,
        warnings,
        within_limit: bytes <= TRANSFER_LIMIT_BYTES,
    })
}

/// Files under a `.git` directory — counted as-is (ignore rules do not apply).
fn walk_git_dir(root: &Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                files += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (files, bytes)
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_info(root: &Path) -> Option<GitInfo> {
    // `rev-parse` fails outside a work tree: no git metadata then.
    let toplevel = git_output(root, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&toplevel) != root {
        // The picked folder is INSIDE a repository; report against its root but
        // keep the transfer rooted at the picked folder.
        return None;
    }
    let remote = git_output(root, &["remote", "get-url", "origin"]).filter(|s| !s.is_empty());
    let branch = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|s| !s.is_empty() && s != "HEAD");
    let head_sha = git_output(root, &["rev-parse", "HEAD"]).filter(|s| !s.is_empty());
    let dirty = git_output(root, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    Some(GitInfo {
        remote,
        branch,
        head_sha,
        dirty,
    })
}

/// Join an archive-relative path to `root`, refusing absolute paths, `..` and
/// (on Unix) anything that would escape through a symlinked ancestor.
///
/// The archive maker never emits such names; this is the extraction-side guard
/// that makes a hostile/corrupt archive harmless.
pub fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf, EngineError> {
    let mut out = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(EngineError::Other(format!(
                    "unsafe archive path: {}",
                    rel.display()
                )));
            }
        }
    }
    // A pre-existing symlink anywhere under the root could redirect the write
    // outside it; the caller creates dirs itself, so walk what exists.
    let mut probe = root.to_path_buf();
    for component in rel.components() {
        if let Component::Normal(part) = component {
            probe.push(part);
            if let Ok(metadata) = std::fs::symlink_metadata(&probe)
                && metadata.file_type().is_symlink()
            {
                return Err(EngineError::Other(format!(
                    "archive path crosses a symlink: {}",
                    rel.display()
                )));
            }
        }
    }
    Ok(out)
}

/// Expand a leading `~` in a destination typed by the user. The dialog seeds
/// `~` when no space matched, and `std::path` has no shell to expand it.
pub fn expand_base(base: &Path) -> PathBuf {
    let text = base.to_string_lossy();
    let rest = text
        .strip_prefix('~')
        .filter(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'));
    match rest {
        Some(rest) => {
            let rest = rest.trim_start_matches(['/', '\\']);
            let home = crate::repos::home_dir();
            if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        }
        None => base.to_path_buf(),
    }
}

/// Refuse to import onto an existing non-empty folder; suggest a free suffix.
pub fn resolve_destination(base: &Path, name: &str) -> Result<PathBuf, EngineError> {
    let candidate = base.join(name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    if crate::repos::is_empty_dir(&candidate) {
        return Ok(candidate);
    }
    for n in 2..1000 {
        let suffixed = base.join(format!("{name}-{n}"));
        if !suffixed.exists() {
            return Ok(suffixed);
        }
    }
    Err(EngineError::Other(format!(
        "no free destination for {name} in {}",
        base.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_base_handles_tilde_and_passes_other_paths_through() {
        let home = crate::repos::home_dir();
        assert_eq!(expand_base(Path::new("~")), home);
        assert_eq!(expand_base(Path::new("~/projects")), home.join("projects"));
        // Not a home prefix: `~other` is a literal folder name.
        assert_eq!(
            expand_base(Path::new("~other/x")),
            PathBuf::from("~other/x")
        );
        assert_eq!(expand_base(Path::new("/tmp/x")), PathBuf::from("/tmp/x"));
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn plan_counts_files_and_skips_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join("src/main.rs"), b"fn main() {}");
        write(&root.join("README.md"), b"hello");
        write(&root.join(".env"), b"SECRET=1");
        write(&root.join("certs/server.pem"), b"key");

        let plan = plan(&root).expect("plan");
        assert_eq!(plan.name, "proj");
        assert_eq!(plan.files, 2, "{plan:?}");
        assert_eq!(plan.bytes, 12 + 5);
        let secrets: Vec<&str> = plan.secrets.iter().map(|f| f.path.as_str()).collect();
        assert!(secrets.contains(&".env"));
        assert!(secrets.contains(&"certs/server.pem"));
        assert!(plan.warnings.contains(&TransferWarning::SecretsExcluded));
    }

    #[test]
    fn plan_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        write(&root.join("target/debug/app"), b"binary");
        write(&root.join("node_modules/x/index.js"), b"js");
        write(&root.join("src/lib.rs"), b"code");

        let plan = plan(&root).expect("plan");
        assert_eq!(plan.files, 2, "{plan:?}"); // .gitignore + src/lib.rs
        assert!(plan.ignored_files >= 2);
        assert!(
            plan.warnings
                .contains(&TransferWarning::IgnoredFilesExcluded)
        );
    }

    #[test]
    fn credentialed_git_config_is_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join("src/lib.rs"), b"code");
        write(
            &root.join(".git/config"),
            b"[remote \"origin\"]\n\turl = https://user:tok_abc123@github.com/o/r.git\n",
        );
        write(&root.join(".git/HEAD"), b"ref: refs/heads/main\n");

        let plan = plan(&root).expect("plan");
        let secrets: Vec<&str> = plan.secrets.iter().map(|f| f.path.as_str()).collect();
        assert!(secrets.contains(&".git/config"), "{secrets:?}");
        assert!(plan.warnings.contains(&TransferWarning::SecretsExcluded));
        // The rest of `.git` still counts toward the transfer.
        assert!(plan.files >= 2, "{plan:?}");
    }

    #[test]
    fn clean_git_config_is_not_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join("src/lib.rs"), b"code");
        write(
            &root.join(".git/config"),
            b"[remote \"origin\"]\n\turl = git@github.com:o/r.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        );

        let plan = plan(&root).expect("plan");
        assert!(plan.secrets.is_empty(), "{:?}", plan.secrets);
    }

    #[test]
    fn credentialed_url_detection() {
        assert!(has_credentialed_url(
            "url = https://user:pass@github.com/o/r.git"
        ));
        assert!(has_credentialed_url(
            "[url \"https://a:b@x\"] insteadOf = https://y"
        ));
        // Username without a password is not a secret.
        assert!(!has_credentialed_url(
            "url = https://user@github.com/o/r.git"
        ));
        assert!(!has_credentialed_url("url = git@github.com:o/r.git"));
        assert!(!has_credentialed_url("url = https://github.com/o/r.git"));
    }

    #[test]
    fn safe_join_refuses_traversal_and_absolute_paths() {
        let root = Path::new("/tmp/dest");
        assert!(safe_join(root, Path::new("a/b")).is_ok());
        for bad in ["../escape", "/etc/passwd", "a/../../b"] {
            assert!(
                safe_join(root, Path::new(bad)).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn safe_join_refuses_paths_crossing_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dest");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(safe_join(&root, Path::new("link/file")).is_err());
    }

    #[test]
    fn destination_suffixes_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_destination(dir.path(), "proj").unwrap(),
            dir.path().join("proj")
        );
        write(&dir.path().join("proj/keep.txt"), b"x");
        assert_eq!(
            resolve_destination(dir.path(), "proj").unwrap(),
            dir.path().join("proj-2")
        );
        // An EMPTY existing dir is reusable.
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        assert_eq!(
            resolve_destination(dir.path(), "empty").unwrap(),
            dir.path().join("empty")
        );
    }

    #[test]
    fn case_collision_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join("Readme.md"), b"a");
        write(&root.join("README.md"), b"b");
        let plan = plan(&root).expect("plan");
        assert!(plan.warnings.contains(&TransferWarning::CaseCollision));
    }
}

// ---------------------------------------------------------------------------
// Archive format (komet archive v1)
// ---------------------------------------------------------------------------
//
// Deliberately hand-rolled instead of pulling `tar` + `zstd`: the format is
// tiny, deterministic, needs no compression dependency, and gives exact control
// over what a hostile archive can do at extraction time.
//
//   magic  "KOMETARCH1\n"            (11 bytes)
//   u32 LE header length
//   header JSON {"v":1,"files":[{"path","size","mode","mtimeMs"}]}
//   file bytes, concatenated in header order
//
// Entries are regular files only. Symlinks are NOT carried: a project's git
// history reproduces them on a clone, and the alternative is an extraction
// surface that can write anywhere.

use std::io::{Read, Seek, SeekFrom, Write};

const MAGIC: &[u8] = b"KOMETARCH1\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveHeader {
    v: u32,
    files: Vec<ArchiveEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveEntry {
    path: String,
    size: u64,
    mode: u32,
    mtime_ms: i64,
}

/// What one built archive contains (counts + checksum for the import handshake).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveManifest {
    pub files: u64,
    pub bytes: u64,
    /// Hex SHA-256 over the whole archive file.
    pub sha256: String,
}

/// Build an archive of `root` at `out`. Ignored files and (unless
/// `include_secrets`) secret-looking files are skipped — the same rules the
/// plan previewed, so "it said 42 files" matches what lands.
pub fn build_archive(
    root: &Path,
    out: &Path,
    include_secrets: bool,
) -> Result<ArchiveManifest, EngineError> {
    use sha2::Digest as _;
    let root = root
        .canonicalize()
        .map_err(|err| EngineError::Other(format!("project path: {err}")))?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut entries = Vec::new();
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in builder.build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(&root) else {
            continue;
        };
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        if !include_secrets && (is_secret_path(rel) || git_config_has_credentials(&root.join(rel)))
        {
            continue;
        }
        paths.push(rel.to_path_buf());
    }
    paths.sort();
    for rel in &paths {
        let abs = root.join(rel);
        let Ok(metadata) = std::fs::metadata(&abs) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        entries.push(ArchiveEntry {
            path: rel.to_string_lossy().replace('\\', "/"),
            size: metadata.len(),
            mode: unix_mode(&metadata),
            mtime_ms: mtime_ms(&metadata),
        });
    }
    let header = ArchiveHeader {
        v: 1,
        files: entries,
    };
    let header_bytes =
        serde_json::to_vec(&header).map_err(|err| EngineError::Other(err.to_string()))?;
    let mut file = std::fs::File::create(out)?;
    file.write_all(MAGIC)?;
    file.write_all(&(header_bytes.len() as u32).to_le_bytes())?;
    file.write_all(&header_bytes)?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(MAGIC);
    hasher.update((header_bytes.len() as u32).to_le_bytes());
    hasher.update(&header_bytes);
    let mut bytes = (MAGIC.len() + 4 + header_bytes.len()) as u64;
    let mut buffer = vec![0u8; 64 * 1024];
    for entry in &header.files {
        let mut source = std::fs::File::open(root.join(&entry.path))?;
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            bytes += read as u64;
        }
    }
    file.flush()?;
    file.sync_all()?;
    Ok(ArchiveManifest {
        files: header.files.len() as u64,
        bytes,
        sha256: hex(&hasher.finalize()),
    })
}

/// Verify an archive's magic/header and return its entries (used by the
/// destination before writing anything). Does not touch the filesystem.
fn read_archive_header(archive: &Path) -> Result<(ArchiveHeader, u64), EngineError> {
    let mut file = std::fs::File::open(archive)?;
    let mut magic = [0u8; 11];
    file.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(EngineError::Other("not a komet archive".into()));
    }
    let mut len = [0u8; 4];
    file.read_exact(&mut len)?;
    let header_len = u32::from_le_bytes(len) as usize;
    if header_len > 64 * 1024 * 1024 {
        return Err(EngineError::Other("archive header too large".into()));
    }
    let mut header_bytes = vec![0u8; header_len];
    file.read_exact(&mut header_bytes)?;
    let header: ArchiveHeader = serde_json::from_slice(&header_bytes)
        .map_err(|err| EngineError::Other(format!("archive header: {err}")))?;
    if header.v != 1 {
        return Err(EngineError::Other(format!(
            "unsupported archive version {}",
            header.v
        )));
    }
    let data_start = (MAGIC.len() + 4 + header_len) as u64;
    Ok((header, data_start))
}

/// Extract `archive` into `dest` (created if needed, must not exist non-empty).
/// Returns the number of files written.
pub fn extract_archive(archive: &Path, dest: &Path) -> Result<u64, EngineError> {
    let (header, data_start) = read_archive_header(archive)?;
    std::fs::create_dir_all(dest)?;
    let mut file = std::fs::File::open(archive)?;
    file.seek(SeekFrom::Start(data_start))?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    for entry in &header.files {
        let rel = Path::new(&entry.path);
        let out_path = safe_join(dest, rel)?;
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        let mut remaining = entry.size;
        while remaining > 0 {
            let want = remaining.min(buffer.len() as u64) as usize;
            let read = file.read(&mut buffer[..want])?;
            if read == 0 {
                return Err(EngineError::Other(format!(
                    "archive truncated while extracting {}",
                    entry.path
                )));
            }
            out.write_all(&buffer[..read])?;
            remaining -= read as u64;
        }
        out.flush()?;
        set_mode(&out_path, entry.mode)?;
        written += 1;
    }
    Ok(written)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn unix_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.mode()
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt as _;
    // Only the permission bits: never setuid/setgid from an archive.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), EngineError> {
    Ok(())
}

fn mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod archive_tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn archive_round_trips_files_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join("src/main.rs"), b"fn main() {}");
        write(&root.join("README.md"), b"hello");
        write(&root.join(".env"), b"SECRET=1");
        let archive = dir.path().join("out.karch");
        let manifest = build_archive(&root, &archive, false).expect("build");
        assert_eq!(manifest.files, 2, "secrets skipped by default");
        assert!(manifest.bytes > 0 && manifest.sha256.len() == 64);

        let dest = dir.path().join("dest");
        let written = extract_archive(&archive, &dest).expect("extract");
        assert_eq!(written, 2);
        assert_eq!(
            std::fs::read(dest.join("src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert!(!dest.join(".env").exists());
    }

    #[test]
    fn archive_skips_credentialed_git_config_unless_opted_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join("src/lib.rs"), b"code");
        write(
            &root.join(".git/config"),
            b"[remote \"origin\"]\n\turl = https://user:tok@github.com/o/r.git\n",
        );
        write(&root.join(".git/HEAD"), b"ref: refs/heads/main\n");

        let archive = dir.path().join("out.karch");
        build_archive(&root, &archive, false).expect("build");
        let (header, _) = read_archive_header(&archive).expect("header");
        let paths: Vec<&str> = header.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&".git/HEAD"), "{paths:?}");
        assert!(!paths.contains(&".git/config"), "{paths:?}");

        let archive = dir.path().join("out-all.karch");
        build_archive(&root, &archive, true).expect("build");
        let (header, _) = read_archive_header(&archive).expect("header");
        assert!(
            header.files.iter().any(|f| f.path == ".git/config"),
            "opt-in must carry the file"
        );
    }

    #[test]
    fn archive_can_include_secrets_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join(".env"), b"SECRET=1");
        let archive = dir.path().join("out.karch");
        let manifest = build_archive(&root, &archive, true).expect("build");
        assert_eq!(manifest.files, 1);
        let dest = dir.path().join("dest");
        extract_archive(&archive, &dest).expect("extract");
        assert_eq!(std::fs::read(dest.join(".env")).unwrap(), b"SECRET=1");
    }

    #[test]
    fn a_corrupt_archive_is_refused_not_half_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.karch");
        std::fs::write(&archive, b"NOT-AN-ARCHIVE").unwrap();
        let dest = dir.path().join("dest");
        assert!(extract_archive(&archive, &dest).is_err());
    }

    #[test]
    fn a_hostile_entry_path_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("evil.karch");
        let header = serde_json::json!({
            "v": 1,
            "files": [{ "path": "../escaped.txt", "size": 1, "mode": 420, "mtimeMs": 0 }]
        });
        let header_bytes = serde_json::to_vec(&header).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&header_bytes);
        bytes.push(b'x');
        std::fs::write(&archive, bytes).unwrap();

        let dest = dir.path().join("dest");
        let err = extract_archive(&archive, &dest).expect_err("must refuse");
        assert!(err.to_string().contains("unsafe archive path"), "{err}");
        assert!(!dir.path().join("escaped.txt").exists());
    }
}

// ---------------------------------------------------------------------------
// Transfer store: staged archives (export) and staged imports
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::Mutex;

/// One in-flight transfer's on-disk staging under `<data_dir>/transfers/`.
#[derive(Debug, Clone)]
pub struct StagedTransfer {
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Debug, Default)]
struct TransferMap {
    next: u64,
    staged: HashMap<String, StagedTransfer>,
}

/// Server-side staging area. Archives are built/landed in `transfers/` and
/// removed on completion or explicit cancel, so a crashed transfer leaves at
/// most one deletable temp file (nothing is written into a project folder until
/// the verified archive is extracted).
#[derive(Debug)]
pub struct TransferStore {
    root: PathBuf,
    map: Mutex<TransferMap>,
    /// Ceiling for one archive (mirrors [`TRANSFER_LIMIT_BYTES`]).
    limit: u64,
}

impl TransferStore {
    pub fn open(data_dir: &Path) -> std::io::Result<Self> {
        let root = data_dir.join("transfers");
        std::fs::create_dir_all(&root)?;
        // A previous process's staged archives are garbage.
        for entry in std::fs::read_dir(&root)?.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
        Ok(Self {
            root,
            map: Mutex::new(TransferMap::default()),
            limit: TRANSFER_LIMIT_BYTES,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TransferMap> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn new_id(&self) -> String {
        let mut map = self.lock();
        map.next += 1;
        format!("transfer-{}-{}", std::process::id(), map.next)
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Build an export archive for `path`; returns its manifest + id.
    pub fn export(
        &self,
        path: &Path,
        include_secrets: bool,
    ) -> Result<StagedTransfer, EngineError> {
        let id = self.new_id();
        let target = self.root.join(format!("{id}.karch"));
        let manifest = build_archive(path, &target, include_secrets)?;
        let staged = StagedTransfer {
            id: id.clone(),
            path: target,
            sha256: manifest.sha256,
            bytes: manifest.bytes,
            files: manifest.files,
        };
        self.lock().staged.insert(id, staged.clone());
        Ok(staged)
    }

    pub fn get(&self, id: &str) -> Option<StagedTransfer> {
        self.lock().staged.get(id).cloned()
    }

    /// Read one chunk of a staged archive (`max` bytes, capped at one chunk).
    pub fn read_chunk(
        &self,
        id: &str,
        offset: u64,
        max: u64,
    ) -> Result<(Vec<u8>, u64, bool), EngineError> {
        use std::io::{Read as _, Seek as _};
        let staged = self
            .get(id)
            .ok_or_else(|| EngineError::Other(format!("unknown transfer {id}")))?;
        let mut file = std::fs::File::open(&staged.path)?;
        let len = file.metadata()?.len();
        if offset > len {
            return Err(EngineError::Other("offset past the archive end".into()));
        }
        let want = max.clamp(1, CHUNK_BYTES).min(len - offset);
        file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0u8; want as usize];
        file.read_exact(&mut buffer)?;
        let next = offset + want;
        Ok((buffer, next, next >= len))
    }

    /// Reserve a staging file for an import of the announced size.
    pub fn begin_import(
        &self,
        name: &str,
        bytes: u64,
        sha256: &str,
    ) -> Result<StagedTransfer, EngineError> {
        if bytes == 0 {
            return Err(EngineError::Other("empty transfer".into()));
        }
        if bytes > self.limit {
            return Err(EngineError::Other(format!(
                "transfer of {bytes} bytes exceeds the {} byte limit",
                self.limit
            )));
        }
        let id = self.new_id();
        let path = self.root.join(format!("{id}.incoming"));
        std::fs::File::create(&path)?;
        let staged = StagedTransfer {
            id: id.clone(),
            path,
            sha256: sha256.to_string(),
            bytes,
            files: 0,
        };
        self.lock().staged.insert(id, staged.clone());
        let _ = name; // the destination folder name is chosen at commit
        Ok(staged)
    }

    /// Write one chunk at `offset` (idempotent: re-sending a chunk after a
    /// dropped connection overwrites the same range).
    pub fn write_chunk(&self, id: &str, offset: u64, data: &[u8]) -> Result<u64, EngineError> {
        use std::io::{Seek as _, Write as _};
        let staged = self
            .get(id)
            .ok_or_else(|| EngineError::Other(format!("unknown transfer {id}")))?;
        if offset.saturating_add(data.len() as u64) > staged.bytes {
            return Err(EngineError::Other("chunk past the announced size".into()));
        }
        let mut file = std::fs::OpenOptions::new().write(true).open(&staged.path)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        Ok(offset + data.len() as u64)
    }

    /// Verify the received archive's SHA-256 and size.
    pub fn verify(&self, id: &str) -> Result<StagedTransfer, EngineError> {
        use sha2::Digest as _;
        let staged = self
            .get(id)
            .ok_or_else(|| EngineError::Other(format!("unknown transfer {id}")))?;
        let len = std::fs::metadata(&staged.path)?.len();
        if len != staged.bytes {
            return Err(EngineError::Other(format!(
                "transfer incomplete: {len} of {} bytes",
                staged.bytes
            )));
        }
        let mut file = std::fs::File::open(&staged.path)?;
        let mut hasher = sha2::Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = {
                use std::io::Read as _;
                file.read(&mut buffer)?
            };
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = hex(&hasher.finalize());
        if !staged.sha256.is_empty() && actual != staged.sha256 {
            return Err(EngineError::Other(format!(
                "checksum mismatch: expected {}, got {actual}",
                staged.sha256
            )));
        }
        Ok(staged)
    }

    /// Extract a verified import into `base/name` (suffixing if needed).
    /// Returns the created folder.
    pub fn commit_import(&self, id: &str, base: &Path, name: &str) -> Result<PathBuf, EngineError> {
        let staged = self.verify(id)?;
        let base = &expand_base(base);
        std::fs::create_dir_all(base)?;
        let dest = resolve_destination(base, name)?;
        let written = extract_archive(&staged.path, &dest)?;
        if written == 0 {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(EngineError::Other("archive contained no files".into()));
        }
        let _ = std::fs::remove_file(&staged.path);
        self.lock().staged.remove(id);
        Ok(dest)
    }

    pub fn cancel(&self, id: &str) {
        if let Some(staged) = self.lock().staged.remove(id) {
            let _ = std::fs::remove_file(&staged.path);
        }
    }
}

/// One chunk of an archive stream. 1 MiB keeps a chunk well under the device
/// relay's frame ceiling while amortising the RPC round-trip.
pub const CHUNK_BYTES: u64 = 1024 * 1024;

#[cfg(test)]
mod store_tests {
    use super::*;

    #[test]
    fn export_read_chunks_import_commit_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src-proj");
        std::fs::create_dir_all(source.join("a")).unwrap();
        std::fs::write(source.join("a/one.txt"), b"one").unwrap();
        std::fs::write(source.join("two.txt"), b"two!").unwrap();

        let store = TransferStore::open(&dir.path().join("data")).expect("store");
        let staged = store.export(&source, false).expect("export");
        assert_eq!(staged.files, 2);

        // Stream it out in small chunks, like the RPC path does.
        let mut received = Vec::new();
        let mut offset = 0;
        loop {
            let (chunk, next, done) = store.read_chunk(&staged.id, offset, 7).expect("chunk");
            received.extend_from_slice(&chunk);
            offset = next;
            if done {
                break;
            }
        }
        assert_eq!(received.len() as u64, staged.bytes);

        // Land it on the "other side".
        let dest_store = TransferStore::open(&dir.path().join("data2")).expect("store2");
        let incoming = dest_store
            .begin_import("src-proj", received.len() as u64, &staged.sha256)
            .expect("begin");
        dest_store
            .write_chunk(&incoming.id, 0, &received)
            .expect("write");
        let target_base = dir.path().join("dest");
        let created = dest_store
            .commit_import(&incoming.id, &target_base, "src-proj")
            .expect("commit");
        assert_eq!(created, target_base.join("src-proj"));
        assert_eq!(std::fs::read(created.join("a/one.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(created.join("two.txt")).unwrap(), b"two!");
    }

    #[test]
    fn a_truncated_import_is_refused_at_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = TransferStore::open(&dir.path().join("data")).expect("store");
        let incoming = store.begin_import("p", 100, "deadbeef").expect("begin");
        store.write_chunk(&incoming.id, 0, b"short").expect("write");
        let err = store
            .commit_import(&incoming.id, dir.path(), "p")
            .expect_err("must refuse");
        assert!(err.to_string().contains("incomplete"), "{err}");
    }

    #[test]
    fn oversize_imports_are_refused_up_front() {
        let dir = tempfile::tempdir().unwrap();
        let store = TransferStore::open(&dir.path().join("data")).expect("store");
        let limit = store.limit();
        assert!(store.begin_import("p", limit + 1, "").is_err());
    }
}
