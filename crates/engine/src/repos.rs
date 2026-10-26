//! Repos — this device's git repositories, branches, worktrees, and the folder
//! browser (feature-inventory §3.5; port of komet's `repos.ts` + `folder-lister.ts`).
//!
//! Repos are device-local (paths differ per machine), so the known set is a plain
//! JSON list (`{data_dir}/repos.json`) — no sync. Existing repos can live anywhere
//! the user points us; cloned/created ones land in `{data_dir}/repos`. Worktrees are
//! created under `~/.komet/worktrees/<repoName>/<worktreeName>` (NOT the data
//! dir — worktrees are user-facing working checkouts), with an auto-generated name +
//! matching `komet/<name>` branch. `KOMET_WORKTREES_DIR` overrides the root.
//!
//! All git access is via subprocess (`tokio::process`) — never libgit2.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use komet_proto::{
    FileSearchMatch, FolderEntry, FolderListing, GitHistoryCommit, GitHistoryPage, GitHistoryRef,
    GitHistoryRefKind, GitHubChecks, GitHubPullRequest, GitHubStatus, Repo, RepoRef, Worktree,
};

use crate::EngineError;

/// Existence probe timeout for user-chosen / remembered paths, which can point at
/// dead network mounts where a bare `stat` hangs for minutes.
const PATH_EXISTS_TIMEOUT: Duration = Duration::from_secs(2);
/// Hard wall-clock ceiling for a folder listing (the walk runs in a disposable
/// blocking task; on expiry the caller unblocks and the task is abandoned).
const FOLDER_LIST_TIMEOUT: Duration = Duration::from_secs(6);
/// Cap on returned folder entries (bounds response size).
const FOLDER_LIST_MAX_ENTRIES: usize = 500;
/// File mentions should remain responsive even in very large checkouts.
const FILE_SEARCH_MAX_RESULTS: usize = 8;
/// A dead network mount must not leave the composer search spinning forever.
const FILE_SEARCH_TIMEOUT: Duration = Duration::from_secs(6);
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_WORKTREE_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_NETWORK_TIMEOUT: Duration = Duration::from_secs(600);
pub const GIT_HISTORY_DEFAULT_LIMIT: usize = 100;
pub const GIT_HISTORY_MAX_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectOpenTarget {
    Cursor,
    VsCode,
    Files,
}

fn project_open_command(linux: &str, macos: &str, windows: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        command.args(["-a", macos, "--args"]);
        command
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (linux, macos);
        std::process::Command::new(windows)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (macos, windows);
        std::process::Command::new(linux)
    }
}

fn file_manager_command() -> std::process::Command {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer.exe")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::process::Command::new("xdg-open")
    }
}

const ADJECTIVES: &[&str] = &[
    "swift", "calm", "bright", "bold", "keen", "brave", "clever", "lucky", "quiet", "warm", "cool",
    "sharp", "gentle", "vivid", "amber", "cobalt",
];
const NOUNS: &[&str] = &[
    "otter", "harbor", "falcon", "cedar", "meadow", "komet", "delta", "ember", "lynx", "maple",
    "onyx", "quartz", "raven", "summit", "willow", "aspen",
];

/// Canonical identity shared by every chat operating in this exact worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutIdentity {
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)` — device-scoped, path-stable.
    pub id: String,
    /// Canonical worktree root (`rev-parse --show-toplevel`, symlinks resolved).
    pub root: PathBuf,
    /// Canonical git dir (worktree-specific for linked worktrees).
    pub git_dir: PathBuf,
}

/// Best-effort home directory (the `ListFolders` default and worktree root base).
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where new worktrees live. Deliberately NOT under the backend data dir —
/// worktrees are user-facing working checkouts. `KOMET_WORKTREES_DIR` overrides
/// (test isolation); empty reads as unset.
fn default_worktrees_root() -> PathBuf {
    std::env::var_os("KOMET_WORKTREES_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".komet").join("worktrees"))
}

struct ReposInner {
    data_dir: PathBuf,
    device_id: String,
    worktrees_root: PathBuf,
    file_searches: std::sync::Mutex<HashMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone)]
pub struct Repos {
    inner: std::sync::Arc<ReposInner>,
}

impl Repos {
    /// `data_dir` holds `repos.json` + cloned/created repos; the worktree root
    /// comes from `$KOMET_WORKTREES_DIR` or `~/.komet/worktrees`.
    pub fn new(data_dir: &Path, device_id: &str) -> Self {
        Self::with_worktrees_root(data_dir, device_id, default_worktrees_root())
    }

    /// Explicit worktree root (tests).
    pub fn with_worktrees_root(data_dir: &Path, device_id: &str, worktrees_root: PathBuf) -> Self {
        Self {
            inner: std::sync::Arc::new(ReposInner {
                data_dir: data_dir.to_path_buf(),
                device_id: device_id.to_string(),
                worktrees_root,
                file_searches: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn open_project(
        &self,
        repo_path: &Path,
        target: ProjectOpenTarget,
    ) -> Result<(), EngineError> {
        let path = std::fs::canonicalize(repo_path)?;
        if !path.is_dir() {
            return Err(EngineError::Other(format!(
                "project path is not a directory: {}",
                path.display()
            )));
        }

        let mut command = match target {
            ProjectOpenTarget::Cursor => project_open_command("cursor", "Cursor", "cursor.exe"),
            ProjectOpenTarget::VsCode => {
                project_open_command("code", "Visual Studio Code", "code.exe")
            }
            ProjectOpenTarget::Files => file_manager_command(),
        };
        command
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| {
                EngineError::Other(format!("could not open project in {target:?}: {error}"))
            })?;
        Ok(())
    }

    // ── registry (repos.json) ───────────────────────────────────────────────

    fn registry_path(&self) -> PathBuf {
        self.inner.data_dir.join("repos.json")
    }

    fn load_paths(&self) -> Vec<String> {
        std::fs::read_to_string(self.registry_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    fn save_paths(&self, paths: &[String]) -> Result<(), EngineError> {
        let mut seen = HashSet::new();
        let deduped: Vec<&String> = paths.iter().filter(|p| seen.insert(p.as_str())).collect();
        let json = serde_json::to_string_pretty(&deduped)
            .map_err(|e| EngineError::Other(format!("repos registry serialize: {e}")))?;
        std::fs::create_dir_all(&self.inner.data_dir)?;
        std::fs::write(self.registry_path(), json)?;
        Ok(())
    }

    fn register(&self, path: &str) -> Result<(), EngineError> {
        let mut paths = self.load_paths();
        paths.push(path.to_string());
        self.save_paths(&paths)
    }

    // ── git plumbing ────────────────────────────────────────────────────────

    /// Run `git <args>` (optionally under `cwd`), returning trimmed stdout.
    async fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, EngineError> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
        let operation = args.first().copied().unwrap_or("unknown");
        let timeout = match operation {
            "clone" | "fetch" => GIT_NETWORK_TIMEOUT,
            "worktree" => GIT_WORKTREE_TIMEOUT,
            _ => GIT_COMMAND_TIMEOUT,
        };
        let output = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return Err(EngineError::Other(format!("git spawn failed: {error}")));
            }
            Err(_) => {
                return Err(EngineError::Other(format!(
                    "git {operation} timed out after {} seconds",
                    timeout.as_secs()
                )));
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(EngineError::Other(if message.is_empty() {
                format!(
                    "git {} failed ({})",
                    args.first().unwrap_or(&"?"),
                    output.status
                )
            } else {
                format!("git: {message}")
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn remote_branches_matching(
        &self,
        cwd: &Path,
        branch: &str,
    ) -> Result<Vec<String>, EngineError> {
        let remotes = self.git(&["remote"], Some(cwd)).await?;
        let mut matches = Vec::new();
        for remote in remotes.lines().filter(|remote| !remote.is_empty()) {
            let remote_branch = format!("{remote}/{branch}");
            if self
                .git(
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/remotes/{remote_branch}"),
                    ],
                    Some(cwd),
                )
                .await
                .is_ok()
            {
                matches.push(remote_branch);
            }
        }
        Ok(matches)
    }

    /// Async existence probe with a timeout: a wedged network mount just reads
    /// as "gone" instead of hanging every caller.
    async fn path_exists(path: &Path) -> bool {
        let path = path.to_path_buf();
        matches!(
            tokio::time::timeout(PATH_EXISTS_TIMEOUT, tokio::fs::metadata(path)).await,
            Ok(Ok(_))
        )
    }

    /// Is `path` inside a git work tree? (Also the SpacesSync git-presence probe.)
    pub async fn is_repo(&self, path: &Path) -> bool {
        matches!(
            self.git(&["rev-parse", "--is-inside-work-tree"], Some(path)).await,
            Ok(out) if out == "true"
        )
    }

    /// The branch currently checked out at a repo/worktree path (`"HEAD"` when detached).
    pub async fn current_branch(&self, path: &Path) -> Result<String, EngineError> {
        let branch = self.git(&["branch", "--show-current"], Some(path)).await?;
        Ok(if branch.is_empty() {
            "HEAD".to_string()
        } else {
            branch
        })
    }

    /// Fetch every configured remote without pruning or integrating anything
    /// into the active branch. This intentionally updates refs only.
    pub async fn fetch_all(&self, repo_path: &Path) -> Result<(), EngineError> {
        self.git(&["fetch", "--all", "--quiet"], Some(repo_path))
            .await
            .map(drop)
    }

    /// The absolute Git `HEAD` file for event-driven external branch reconciliation.
    pub async fn git_head_path(&self, path: &Path) -> Result<PathBuf, EngineError> {
        let git_dir = self
            .git(&["rev-parse", "--absolute-git-dir"], Some(path))
            .await?;
        Ok(PathBuf::from(git_dir).join("HEAD"))
    }

    /// Canonical identity shared by every chat operating in this exact worktree:
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)`.
    pub async fn checkout_identity(&self, path: &Path) -> Result<CheckoutIdentity, EngineError> {
        let root = self
            .git(&["rev-parse", "--show-toplevel"], Some(path))
            .await?;
        let git_dir = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-dir"],
                Some(path),
            )
            .await?;
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(&root));
        let canonical_git_dir =
            std::fs::canonicalize(&git_dir).unwrap_or_else(|_| PathBuf::from(&git_dir));
        let mut hasher = Sha256::new();
        hasher.update(self.inner.device_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical_git_dir.to_string_lossy().as_bytes());
        let id = hex(&hasher.finalize());
        Ok(CheckoutIdentity {
            id,
            root: canonical_root,
            git_dir: canonical_git_dir,
        })
    }

    async fn to_repo(&self, path: &Path) -> Result<Repo, EngineError> {
        let branch = self.current_branch(path).await.ok();
        Ok(Repo {
            path: path.to_string_lossy().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string()),
            default_branch: branch,
        })
    }

    // ── ListRepos / AddRepo / CloneRepo / CreateRepo ────────────────────────

    /// Known repos that still exist, each with its current branch. Never fails:
    /// vanished paths and non-repos are silently dropped.
    pub async fn list(&self) -> Vec<Repo> {
        let mut repos = Vec::new();
        for path in self.load_paths() {
            let path = PathBuf::from(path);
            if !Self::path_exists(&path).await || !self.is_repo(&path).await {
                continue;
            }
            match self.to_repo(&path).await {
                Ok(repo) => repos.push(repo),
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "repo listing skip")
                }
            }
        }
        repos
    }

    /// Remember an existing repository the user pointed us at.
    pub async fn add(&self, path: &str) -> Result<Repo, EngineError> {
        let abs = absolutize(Path::new(path));
        if !Self::path_exists(&abs).await {
            return Err(EngineError::Other(format!(
                "No such folder: {}",
                abs.display()
            )));
        }
        if !self.is_repo(&abs).await {
            return Err(EngineError::Other(format!(
                "Not a git repository: {}",
                abs.display()
            )));
        }
        self.register(&abs.to_string_lossy())?;
        self.to_repo(&abs).await
    }

    /// `git clone <url>` under `{data_dir}/repos`. (Named `clone_repo` to keep
    /// `Clone::clone` unambiguous on the service handle.)
    pub async fn clone_repo(&self, url: &str) -> Result<Repo, EngineError> {
        let trimmed = url.trim().trim_end_matches('/');
        let name = trimmed
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("repo")
            .to_string();
        let repos_dir = self.inner.data_dir.join("repos");
        let target = repos_dir.join(&name);
        if target.exists() {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&repos_dir)?;
        self.git(&["clone", trimmed, &target.to_string_lossy()], None)
            .await?;
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    /// `git init -b main` a fresh repository under `{data_dir}/repos`.
    pub async fn create(&self, name: &str) -> Result<Repo, EngineError> {
        let clean: String = name
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if clean.is_empty() || clean.chars().all(|c| c == '-' || c == '.') {
            return Err(EngineError::Other("Invalid repository name".into()));
        }
        let target = self.inner.data_dir.join("repos").join(&clean);
        if target.exists() {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&target)?;
        self.git(&["init", "-b", "main"], Some(&target)).await?;
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    // ── branches ────────────────────────────────────────────────────────────

    /// All branches (`git branch -a`), local first, deduped against their remote
    /// counterparts, with the repo's default branch first.
    pub async fn branches(&self, repo_path: &Path) -> Result<Vec<String>, EngineError> {
        let out = self
            .git(&["branch", "-a", "--format=%(refname)"], Some(repo_path))
            .await?;
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |name: &str| {
            if !name.is_empty() && name != "HEAD" && seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        };
        // Locals first, then remote-only branches (stripped of their remote prefix).
        for line in out.lines().map(str::trim) {
            if let Some(local) = line.strip_prefix("refs/heads/") {
                push(local);
            }
        }
        for line in out.lines().map(str::trim) {
            if let Some(remote) = line.strip_prefix("refs/remotes/")
                && let Some((_, name)) = remote.split_once('/')
            {
                push(name);
            }
        }
        // Default branch first: origin/HEAD's target, else the checked-out branch.
        let default = match self
            .git(
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                Some(repo_path),
            )
            .await
        {
            Ok(short) => short.split_once('/').map(|(_, b)| b.to_string()),
            Err(_) => None,
        };
        let default = match default {
            Some(branch) => Some(branch),
            None => self
                .current_branch(repo_path)
                .await
                .ok()
                .filter(|b| b != "HEAD"),
        };
        if let Some(default) = default
            && let Some(pos) = names.iter().position(|n| *n == default)
        {
            let head = names.remove(pos);
            names.insert(0, head);
        }
        Ok(names)
    }

    /// Resolve the first configured GitHub remote to a credential-free HTTPS
    /// repository URL. `origin` wins when several remotes point to GitHub.
    pub async fn github_repo_url(&self, repo_path: &Path) -> Result<Option<String>, EngineError> {
        let remotes = self.git(&["remote"], Some(repo_path)).await?;
        let mut names: Vec<&str> = remotes.lines().filter(|name| !name.is_empty()).collect();
        names.sort_by_key(|name| (*name != "origin", *name));
        for name in names {
            let Ok(remote_url) = self
                .git(&["remote", "get-url", name], Some(repo_path))
                .await
            else {
                continue;
            };
            if let Some(url) = github_url_from_remote(&remote_url) {
                return Ok(Some(url));
            }
        }
        Ok(None)
    }

    /// Everything the titlebar GitHub control shows for a checkout, or `None`
    /// when no remote points at GitHub. The pull request comes from the
    /// user's own `gh` CLI (its auth, never a Komet-held token) and is simply
    /// absent when `gh` is missing, signed out, offline, or knows no PR.
    pub async fn github_status(
        &self,
        repo_path: &Path,
    ) -> Result<Option<GitHubStatus>, EngineError> {
        self.github_status_for_branch(repo_path, None).await
    }

    /// GitHub panel data for the checked-out branch or a branch selected for
    /// read-only browsing. The requested ref is validated and resolved before
    /// it is passed to Git or `gh`.
    pub async fn github_status_for_branch(
        &self,
        repo_path: &Path,
        branch: Option<&str>,
    ) -> Result<Option<GitHubStatus>, EngineError> {
        let Some(repo_url) = self.github_repo_url(repo_path).await? else {
            return Ok(None);
        };
        let checkout_branch = self.current_branch(repo_path).await?;
        let branch = match branch {
            Some(branch) => {
                if !self
                    .branches(repo_path)
                    .await?
                    .iter()
                    .any(|available| available == branch)
                {
                    return Err(EngineError::Other(
                        "Branch is not available in this repository".into(),
                    ));
                }
                self.git(&["check-ref-format", "--branch", branch], Some(repo_path))
                    .await
                    .map_err(|_| EngineError::Other("Invalid branch name".into()))?;
                let revision = format!("{branch}^{{commit}}");
                self.git(
                    &["rev-parse", "--verify", "--quiet", &revision],
                    Some(repo_path),
                )
                .await
                .map_err(|_| EngineError::Other("Branch is not available locally".into()))?;
                branch.to_string()
            }
            None => checkout_branch.clone(),
        };
        let detached = branch == "HEAD";
        let read_only_branch = branch != checkout_branch;
        let (pull_request, recent_commits) = tokio::join!(
            async {
                if detached {
                    None
                } else {
                    github_pull_request(repo_path, Some(&branch)).await
                }
            },
            self.recent_commits(repo_path, GITHUB_RECENT_COMMITS, &branch),
        );
        let base = self
            .changes_base(
                repo_path,
                &branch,
                pull_request.as_ref().map(|pr| pr.base_ref.as_str()),
            )
            .await;
        let diff_args = if read_only_branch {
            vec!["diff", "--shortstat", base.as_str(), branch.as_str()]
        } else {
            vec!["diff", "--shortstat", base.as_str()]
        };
        let (insertions, deletions) = self
            .git(&diff_args, Some(repo_path))
            .await
            .map(|out| parse_shortstat(&out))
            .unwrap_or_default();
        Ok(Some(GitHubStatus {
            repo_url,
            branch,
            pull_request,
            recent_commits,
            insertions,
            deletions,
        }))
    }

    /// Latest `limit` commits reachable from `revision`.
    async fn recent_commits(
        &self,
        repo_path: &Path,
        limit: usize,
        revision: &str,
    ) -> Vec<GitHistoryCommit> {
        let count = format!("-n{limit}");
        self.git(
            &[
                "log",
                "--no-color",
                "--no-show-signature",
                &count,
                "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
                revision,
            ],
            Some(repo_path),
        )
        .await
        .map(|out| parse_history_log(&out, &HashMap::new()))
        .unwrap_or_default()
    }

    /// The commit the branch's changes are measured from: the merge base with
    /// the PR base, else with `origin/HEAD`, else the current checkout HEAD.
    async fn changes_base(
        &self,
        repo_path: &Path,
        revision: &str,
        pr_base: Option<&str>,
    ) -> String {
        let mut candidates = Vec::new();
        if let Some(base) = pr_base.filter(|base| !base.is_empty()) {
            candidates.push(format!("origin/{base}"));
        }
        if let Ok(default) = self
            .git(
                &["rev-parse", "--abbrev-ref", "origin/HEAD"],
                Some(repo_path),
            )
            .await
        {
            candidates.push(default);
        }
        for candidate in candidates {
            if let Ok(base) = self
                .git(&["merge-base", revision, &candidate], Some(repo_path))
                .await
                && !base.is_empty()
            {
                return base;
            }
        }
        "HEAD".to_string()
    }

    /// [`Self::branches`] enriched with checkout state: which branch the MAIN
    /// folder has checked out (`current`) and which branches are materialized
    /// as linked worktrees (`worktree_path`). Feeds the composer's ref picker
    /// and its checkout-kind selector.
    pub async fn refs(&self, repo_path: &Path) -> Result<Vec<RepoRef>, EngineError> {
        let names = self.branches(repo_path).await?;
        let current = self.current_branch(repo_path).await?;
        // `git worktree list --porcelain`: stanzas of `worktree <path>` /
        // `HEAD <sha>` / `branch refs/heads/<name>`. The first stanza is the
        // main checkout — excluded (it's `current`, not a linked worktree).
        let mut worktrees: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let out = self
            .git(&["worktree", "list", "--porcelain"], Some(repo_path))
            .await?;
        let mut stanza = 0usize;
        let mut path: Option<String> = None;
        for line in out.lines().map(str::trim) {
            if let Some(p) = line.strip_prefix("worktree ") {
                stanza += 1;
                // The first stanza is the main checkout, not a linked tree.
                path = (stanza > 1).then(|| p.to_string());
            } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
                && let Some(path) = path.take()
            {
                worktrees.insert(branch.to_string(), path);
            }
        }
        Ok(names
            .into_iter()
            .map(|name| RepoRef {
                current: current == name,
                worktree_path: worktrees.get(&name).cloned(),
                name,
            })
            .collect())
    }

    /// Public commit history in topological order. Only user-facing branches,
    /// remotes, and tags seed the walk, so Komet's internal refs never leak
    /// into the graph or keep otherwise-unreachable checkpoints visible.
    pub async fn history(
        &self,
        repo_path: &Path,
        cursor: usize,
        limit: usize,
    ) -> Result<GitHistoryPage, EngineError> {
        let limit = limit.clamp(1, GIT_HISTORY_MAX_LIMIT);
        let head_sha = self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(repo_path))
            .await
            .ok()
            .filter(|sha| !sha.is_empty());

        let refs_out = self
            .git(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)%00%(symref)%00",
                    "refs/heads",
                    "refs/remotes",
                    "refs/tags",
                ],
                Some(repo_path),
            )
            .await?;
        let refs_by_sha = parse_history_refs(&refs_out);

        if head_sha.is_none() && refs_by_sha.is_empty() {
            return Ok(GitHistoryPage {
                commits: Vec::new(),
                head_sha: None,
                next_cursor: None,
                total_count: Some(0),
                head_commit_count: Some(0),
            });
        }

        let skip = format!("--skip={cursor}");
        let max_count = format!("--max-count={}", limit + 1);
        let mut log_args = vec![
            "log",
            "--topo-order",
            "--no-color",
            "--no-decorate",
            "--no-show-signature",
            "--no-patch",
            skip.as_str(),
            max_count.as_str(),
            "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
        ];
        if head_sha.is_some() {
            log_args.push("HEAD");
        }
        log_args.extend(["--branches", "--remotes", "--tags"]);
        let log = self.git(&log_args, Some(repo_path)).await?;
        let mut commits = parse_history_log(&log, &refs_by_sha);
        let has_next = commits.len() > limit;
        commits.truncate(limit);

        let total_count = if cursor == 0 {
            let mut count_args = vec!["rev-list", "--count"];
            if head_sha.is_some() {
                count_args.push("HEAD");
            }
            count_args.extend(["--branches", "--remotes", "--tags"]);
            self.git(&count_args, Some(repo_path))
                .await
                .ok()
                .and_then(|count| count.parse().ok())
        } else {
            None
        };
        let head_commit_count = if cursor == 0 && head_sha.is_some() {
            self.git(&["rev-list", "--count", "HEAD"], Some(repo_path))
                .await
                .ok()
                .and_then(|count| count.parse().ok())
        } else {
            None
        };

        Ok(GitHistoryPage {
            next_cursor: has_next.then_some(cursor + commits.len()),
            commits,
            head_sha,
            total_count,
            head_commit_count,
        })
    }

    /// Whether `candidate` is the repository root or one of its linked
    /// worktrees. Filesystem resolution happens on a disposable thread because
    /// user-selected paths may be dead mounts.
    pub async fn workspace_checkout(&self, repo_path: &Path, candidate: &Path) -> Option<PathBuf> {
        let repo_path = repo_path.to_path_buf();
        let candidate = candidate.to_path_buf();
        let worktrees: Vec<_> = self
            .refs(&repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.worktree_path.map(PathBuf::from))
            .collect();
        disposable_worker("checkout-auth", move || {
            let candidate = std::fs::canonicalize(candidate).ok()?;
            std::iter::once(repo_path)
                .chain(worktrees)
                .filter_map(|path| std::fs::canonicalize(path).ok())
                .any(|path| path == candidate)
                .then_some(candidate)
        })
        .await
        .flatten()
    }

    /// Switch the checkout at `cwd` (a main folder OR a linked worktree) to
    /// `ref_name` — the t3code `switchRef` port: an existing local branch is
    /// checked out directly; a remote-only branch gets a local tracking
    /// branch (`checkout --track origin/<ref>`). A dirty tree or a branch
    /// already checked out in another worktree fails with git's own message.
    /// Returns the resulting current branch.
    pub async fn switch_ref(&self, cwd: &Path, ref_name: &str) -> Result<String, EngineError> {
        if ref_name == "HEAD" {
            return Err(EngineError::Other(
                "Detached HEAD is not a selectable branch".into(),
            ));
        }
        self.git(&["check-ref-format", "--branch", ref_name], Some(cwd))
            .await
            .map_err(|_| EngineError::Other("Invalid branch name".into()))?;
        let local = self
            .git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{ref_name}"),
                ],
                Some(cwd),
            )
            .await
            .is_ok();
        if local {
            self.git(&["checkout", ref_name], Some(cwd)).await?;
        } else {
            let matches = self.remote_branches_matching(cwd, ref_name).await?;
            match matches.as_slice() {
                [remote_branch] => {
                    self.git(&["checkout", "--track", remote_branch], Some(cwd))
                        .await?;
                }
                [] => return Err(EngineError::Other(format!("Branch not found: {ref_name}"))),
                _ => {
                    return Err(EngineError::Other(format!(
                        "Branch '{ref_name}' exists on multiple remotes; create a local branch or remove the ambiguity first"
                    )));
                }
            }
        }
        let out = self.git(&["branch", "--show-current"], Some(cwd)).await?;
        Ok(out.trim().to_string())
    }

    // ── worktrees ───────────────────────────────────────────────────────────

    /// `git worktree add` an isolated checkout under
    /// `{worktrees_root}/<repoName>/<generatedName>`, on a fresh `komet/<name>`
    /// branch off `branch`.
    pub async fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<Worktree, EngineError> {
        if branch == "HEAD" {
            return Err(EngineError::Other(
                "Detached HEAD is not a selectable worktree base".into(),
            ));
        }
        self.git(&["check-ref-format", "--branch", branch], Some(repo_path))
            .await
            .map_err(|_| EngineError::Other("Invalid branch name".into()))?;
        let local_branch = self
            .git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
                Some(repo_path),
            )
            .await
            .is_ok();
        let base_ref = if local_branch {
            branch.to_string()
        } else {
            match self
                .remote_branches_matching(repo_path, branch)
                .await?
                .as_slice()
            {
                [remote_branch] => remote_branch.clone(),
                [] => return Err(EngineError::Other(format!("Branch not found: {branch}"))),
                _ => {
                    return Err(EngineError::Other(format!(
                        "Branch '{branch}' exists on multiple remotes; create a local branch or remove the ambiguity first"
                    )));
                }
            }
        };
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let base = self.inner.worktrees_root.join(&repo_name);
        std::fs::create_dir_all(&base)?;
        // Auto-generate a name colliding with neither an existing dir nor branch.
        let existing: HashSet<String> = self.branches(repo_path).await?.into_iter().collect();
        let mut name = None;
        for attempt in 0..50u64 {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(attempt)
                .wrapping_add(attempt.wrapping_mul(0x9E37_79B9));
            let candidate = format!(
                "{}-{}",
                ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize],
                NOUNS[((seed / 31) % NOUNS.len() as u64) as usize]
            );
            if !base.join(&candidate).exists() && !existing.contains(&format!("komet/{candidate}"))
            {
                name = Some(candidate);
                break;
            }
        }
        let name =
            name.ok_or_else(|| EngineError::Other("Could not allocate a worktree name".into()))?;
        let path = base.join(&name);
        let branch_name = format!("komet/{name}");
        self.git(
            &[
                "worktree",
                "add",
                "-b",
                &branch_name,
                &path.to_string_lossy(),
                &base_ref,
            ],
            Some(repo_path),
        )
        .await?;
        let checkout = self.checkout_identity(&path).await?;
        Ok(Worktree {
            repo_path: repo_path.to_string_lossy().to_string(),
            path: path.to_string_lossy().to_string(),
            branch: branch_name,
            name,
            checkout_id: Some(checkout.id),
        })
    }

    async fn branch_exists(&self, path: &Path, branch: &str) -> bool {
        self.git(
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            Some(path),
        )
        .await
        .is_ok()
    }

    /// Rename a komet-created worktree branch after its chat's generated title
    /// (port of komet's `renameWorktreeBranch`). Guards:
    /// - respect an external checkout/rename: only act while the worktree is still
    ///   on `expected_branch` AND that branch is the original `komet/<folderName>`;
    /// - a title-slug collision gets a stable 6-hex suffix (hash of the worktree
    ///   path); a collision on THAT too fails.
    ///
    /// Returns the branch the worktree ends up on (re-read after the rename so a
    /// concurrent external checkout always wins the metadata race).
    pub async fn rename_worktree_branch(
        &self,
        worktree_path: &Path,
        expected_branch: &str,
        title: &str,
    ) -> Result<String, EngineError> {
        let current = self.current_branch(worktree_path).await?;
        let folder = worktree_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if current != expected_branch || expected_branch != format!("komet/{folder}") {
            return Ok(current);
        }
        let preferred = worktree_branch_from_title(title);
        if preferred == current {
            return Ok(current);
        }
        let mut hasher = Sha256::new();
        hasher.update(worktree_path.to_string_lossy().as_bytes());
        let suffix = &hex(&hasher.finalize())[..6];
        let target = if self.branch_exists(worktree_path, &preferred).await {
            format!("{preferred}-{suffix}")
        } else {
            preferred
        };
        if self.branch_exists(worktree_path, &target).await {
            return Err(EngineError::Other(format!(
                "Branch already exists: {target}"
            )));
        }
        self.git(
            &["branch", "-m", "--", &current, &target],
            Some(worktree_path),
        )
        .await?;
        self.current_branch(worktree_path).await
    }

    /// Best-effort worktree removal (if it still exists), then prune stale refs.
    /// Deletes the worktree's branch ONLY when komet created it (`komet/…`) — the
    /// user may have checked out their own branch inside the worktree.
    pub async fn delete_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
    ) -> Result<(), EngineError> {
        if !worktree_path.exists() {
            self.git(&["worktree", "prune"], Some(repo_path)).await?;
            return Ok(());
        }

        let requested = std::fs::canonicalize(worktree_path)?;
        let worktrees = self
            .git(&["worktree", "list", "--porcelain"], Some(repo_path))
            .await?;
        let mut stanza = 0usize;
        let mut is_linked_worktree = false;
        for line in worktrees.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                stanza += 1;
                if stanza > 1 && std::fs::canonicalize(path).ok().as_ref() == Some(&requested) {
                    is_linked_worktree = true;
                    break;
                }
            }
        }
        if !is_linked_worktree {
            return Err(EngineError::Other(format!(
                "Not a linked worktree: {}",
                requested.display()
            )));
        }

        let branch = self.current_branch(&requested).await?;
        self.git(
            &[
                "worktree",
                "remove",
                "--force",
                &requested.to_string_lossy(),
            ],
            Some(repo_path),
        )
        .await?;
        self.git(&["worktree", "prune"], Some(repo_path)).await?;
        if branch.starts_with("komet/") && self.branch_exists(repo_path, &branch).await {
            self.git(&["branch", "-D", "--", &branch], Some(repo_path))
                .await?;
        }
        Ok(())
    }

    // ── ListFolders ─────────────────────────────────────────────────────────

    /// One directory level (home by default): dotfiles hidden, directories first,
    /// capped at [`FOLDER_LIST_MAX_ENTRIES`] with a `truncated` flag. The walk runs
    /// in a spawned blocking task under a 6s wall-clock ceiling — a wedged path
    /// (dead mount, permission-gated folder) fails this listing without blocking
    /// anything else; the abandoned task unwinds on its own thread.
    pub async fn list_folders(&self, path: Option<String>) -> Result<FolderListing, EngineError> {
        self.list_folders_with(path, FOLDER_LIST_TIMEOUT, false)
            .await
    }

    /// Search a checkout's files and directories by fuzzy relative path. The
    /// `ignore` walker honors `.gitignore`, `.ignore`, and global git excludes.
    /// Dotfiles remain searchable; only repository metadata is always pruned.
    pub async fn search_files(
        &self,
        root: PathBuf,
        query: String,
        featured_paths: Vec<String>,
    ) -> Result<Vec<FileSearchMatch>, EngineError> {
        let deadline = tokio::time::Instant::now() + FILE_SEARCH_TIMEOUT;
        let gate = {
            let mut searches = self
                .inner
                .file_searches
                .lock()
                .map_err(|_| EngineError::Other("file search registry poisoned".into()))?;
            if let Some(gate) = searches.get(&root).and_then(std::sync::Weak::upgrade) {
                gate
            } else {
                let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                searches.insert(root.clone(), std::sync::Arc::downgrade(&gate));
                gate
            }
        };
        let gate = tokio::time::timeout_at(deadline, gate.lock_owned())
            .await
            .map_err(|_| EngineError::Other("file search timed out".into()))?;
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let worker_cancelled = cancelled.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("file-search".into())
            .spawn(move || {
                let _gate = gate;
                let _ = tx.send(search_files_blocking_with_cancel(
                    &root,
                    &query,
                    &featured_paths,
                    || worker_cancelled.load(Ordering::Relaxed),
                ));
            })
            .map_err(|e| EngineError::Other(format!("file search failed: {e}")))?;
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(EngineError::Other("file search worker exited".into())),
            Err(_) => Err(EngineError::Other("file search timed out".into())),
        }
    }

    /// `hang_for_test` makes the worker never respond — exercises the timeout path.
    ///
    /// The walk runs on a DETACHED OS thread (not the tokio blocking pool): a
    /// readdir wedged in the kernel can't be cancelled, and a poisoned blocking
    /// pool — or a runtime shutdown waiting on it — must never be possible. On
    /// timeout the thread is simply abandoned (the komet backend's disposable
    /// worker, minus the terminate()).
    #[doc(hidden)]
    pub async fn list_folders_with(
        &self,
        path: Option<String>,
        timeout: Duration,
        hang_for_test: bool,
    ) -> Result<FolderListing, EngineError> {
        let target = match path.filter(|p| !p.trim().is_empty()) {
            Some(p) => absolutize(Path::new(&p)),
            None => home_dir(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("folder-list".into())
            .spawn(move || {
                if hang_for_test {
                    // Hold the sender without responding (detached thread; process
                    // exit reclaims it) — the caller must hit its timeout.
                    std::thread::sleep(Duration::from_secs(3600));
                }
                let _ = tx.send(list_folders_blocking(&target));
            });
        if let Err(err) = spawned {
            return Err(EngineError::Other(format!("folder listing failed: {err}")));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(EngineError::Other("folder listing worker exited".into())),
            Err(_) => Err(EngineError::Other(
                "folder listing timed out on the device".into(),
            )),
        }
    }
}

struct CancelOnDrop(std::sync::Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn disposable_worker<T: Send + 'static>(
    name: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.await.ok()
}

/// The blocking walk: ONE readdir of the target; `is_repo` is a cheap `.git`
/// existence probe per directory entry.
fn list_folders_blocking(target: &Path) -> Result<FolderListing, EngineError> {
    let read = std::fs::read_dir(target).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            EngineError::Other("Komet doesn't have access to this folder on the device.".into())
        }
        _ => EngineError::Other(format!("could not read that folder: {e}")),
    })?;
    let mut entries: Vec<FolderEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_repo = is_dir && entry.path().join(".git").exists();
        entries.push(FolderEntry {
            name,
            is_dir,
            is_repo,
        });
    }
    // Directories first, each group name-sorted (case-insensitive).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > FOLDER_LIST_MAX_ENTRIES;
    entries.truncate(FOLDER_LIST_MAX_ENTRIES);
    Ok(FolderListing {
        path: target.to_string_lossy().to_string(),
        entries,
        truncated,
    })
}

/// Case-insensitive subsequence score. Lower is better: adjacent and earlier
/// characters win, while still allowing `cmp rs` to find `composer.rs`.
fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let candidate = candidate.to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .try_fold(0usize, |total, term| {
            let mut at = 0;
            let mut score = 0usize;
            let mut previous_end = None;
            for needle in term.chars() {
                let found = candidate[at..].find(needle)? + at;
                score += found;
                if previous_end == Some(found) {
                    score = score.saturating_sub(2);
                }
                at = found + needle.len_utf8();
                previous_end = Some(at);
            }
            Some(total.saturating_add(score))
        })
}

type RankedFileMatch = (Option<usize>, usize, String, bool);

fn compare_file_matches(
    query: &str,
    (featured_a, score_a, path_a, dir_a): &RankedFileMatch,
    (featured_b, score_b, path_b, dir_b): &RankedFileMatch,
) -> std::cmp::Ordering {
    let empty_query = query.trim().is_empty();
    featured_a
        .is_none()
        .cmp(&featured_b.is_none())
        .then_with(|| featured_a.cmp(featured_b))
        .then_with(|| score_a.cmp(score_b))
        .then_with(|| {
            if empty_query {
                path_a.split('/').count().cmp(&path_b.split('/').count())
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| {
            if empty_query {
                dir_a.cmp(dir_b)
            } else {
                dir_b.cmp(dir_a)
            }
        })
        .then_with(|| path_a.len().cmp(&path_b.len()))
        .then_with(|| path_a.cmp(path_b))
}

#[cfg(test)]
fn search_files_blocking(
    root: &Path,
    query: &str,
    featured_paths: &[String],
) -> Result<Vec<FileSearchMatch>, EngineError> {
    search_files_blocking_with_cancel(root, query, featured_paths, || false)
}

fn search_files_blocking_with_cancel<F: Fn() -> bool>(
    root: &Path,
    query: &str,
    featured_paths: &[String],
    cancelled: F,
) -> Result<Vec<FileSearchMatch>, EngineError> {
    let root = std::fs::canonicalize(root)
        .map_err(|e| EngineError::Other(format!("could not search workspace: {e}")))?;
    let featured: HashMap<String, usize> = featured_paths
        .iter()
        .filter_map(|path| {
            let path = Path::new(path);
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let canonical = std::fs::canonicalize(full).ok()?;
            let relative = canonical.strip_prefix(&root).ok()?;
            Some(relative.to_string_lossy().replace('\\', "/"))
        })
        .enumerate()
        .fold(HashMap::new(), |mut paths, (rank, path)| {
            paths.entry(path).or_insert(rank);
            paths
        });
    let mut matches = Vec::new();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
        .build();
    for entry in walker {
        if cancelled() {
            return Err(EngineError::Other("file search cancelled".into()));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(%err, "file mention search walk skipped entry");
                continue;
            }
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.starts_with(".git/") || relative == ".git" {
            continue;
        }
        let Some(path_score) = fuzzy_score(query, &relative) else {
            continue;
        };
        let score = relative
            .rsplit('/')
            .next()
            .and_then(|name| fuzzy_score(query, name))
            .unwrap_or_else(|| path_score.saturating_add(1_000));
        matches.push((
            featured.get(&relative).copied(),
            score,
            relative,
            entry.file_type().is_some_and(|kind| kind.is_dir()),
        ));
        if matches.len() > FILE_SEARCH_MAX_RESULTS {
            matches.sort_by(|a, b| compare_file_matches(query, a, b));
            matches.truncate(FILE_SEARCH_MAX_RESULTS);
        }
    }
    matches.sort_by(|a, b| compare_file_matches(query, a, b));
    Ok(matches
        .into_iter()
        .map(|(_, _, path, is_dir)| FileSearchMatch { path, is_dir })
        .collect())
}

/// Turn a generated chat title into the semantic portion of a Komet branch
/// (port of komet's `worktreeBranchFromTitle`). Komet NFKD-normalizes accented
/// letters first; native keeps it ASCII-only (generated titles are Title Case
/// English), so non-ASCII characters collapse into the `-` separator.
pub fn worktree_branch_from_title(title: &str) -> String {
    let mut slug = String::new();
    for c in title.trim().chars() {
        if matches!(c, '\'' | '"' | '`') {
            continue; // dropped entirely (cafe's → cafes), not a separator
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(48);
    let slug = slug.trim_matches('-');
    format!("komet/{}", if slug.is_empty() { "update" } else { slug })
}

fn bounded_field(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn github_url_from_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.is_empty() || remote.contains(['?', '#']) {
        return None;
    }

    let (host, path) = if let Some((scheme, rest)) = remote.split_once("://") {
        if !matches!(scheme.to_ascii_lowercase().as_str(), "https" | "ssh") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host_port = authority.rsplit('@').next()?;
        // `ssh://git@github.com:22/…` — only the hostname is compared.
        let host = match host_port.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
            _ => host_port,
        };
        (host, path)
    } else {
        let (authority, path) = remote.split_once(':')?;
        let host = authority.rsplit('@').next()?;
        (host, path)
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }

    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    };
    if parts.next().is_some() || !valid_segment(owner) || !valid_segment(repo) {
        return None;
    }

    Some(format!("https://github.com/{owner}/{repo}"))
}

const GITHUB_RECENT_COMMITS: usize = 5;
const GH_TIMEOUT: Duration = Duration::from_secs(8);

/// `gh pr view` for the checked-out branch. Every failure (no `gh`, signed
/// out, offline, no PR, timeout) reads as "no pull request".
async fn github_pull_request(repo_path: &Path, branch: Option<&str>) -> Option<GitHubPullRequest> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(["pr", "view"]);
    if let Some(branch) = branch {
        cmd.arg(branch);
    }
    cmd.args([
        "--json",
        "number,title,url,state,isDraft,baseRefName,statusCheckRollup,reviewDecision",
    ])
    .current_dir(repo_path)
    .env("GH_PROMPT_DISABLED", "1")
    .env("GH_NO_UPDATE_NOTIFIER", "1")
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);
    let output = tokio::time::timeout(GH_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gh_pull_request(&output.stdout)
}

fn parse_gh_pull_request(json: &[u8]) -> Option<GitHubPullRequest> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let mut checks = GitHubChecks::default();
    for check in value["statusCheckRollup"].as_array().into_iter().flatten() {
        // CheckRun carries `status` + `conclusion`; StatusContext only `state`.
        let finished = check["status"]
            .as_str()
            .is_none_or(|status| status.eq_ignore_ascii_case("COMPLETED"));
        let outcome = check["conclusion"]
            .as_str()
            .filter(|c| !c.is_empty())
            .or_else(|| check["state"].as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        match outcome.as_str() {
            _ if !finished => checks.pending += 1,
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => checks.passed += 1,
            "" | "PENDING" | "EXPECTED" => checks.pending += 1,
            _ => checks.failed += 1,
        }
    }
    Some(GitHubPullRequest {
        number: value["number"].as_u64()?,
        title: value["title"].as_str()?.to_string(),
        url: value["url"].as_str()?.to_string(),
        state: value["state"].as_str().unwrap_or("OPEN").to_string(),
        is_draft: value["isDraft"].as_bool().unwrap_or(false),
        base_ref: value["baseRefName"].as_str().unwrap_or("").to_string(),
        checks,
        review_decision: value["reviewDecision"]
            .as_str()
            .filter(|decision| !decision.is_empty())
            .map(str::to_string),
    })
}

/// `(insertions, deletions)` from `git diff --shortstat` output.
fn parse_shortstat(out: &str) -> (u64, u64) {
    let mut stats = (0, 0);
    for part in out.split(',') {
        let mut words = part.split_whitespace();
        let (Some(count), Some(kind)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        if kind.starts_with("insertion") {
            stats.0 = count;
        } else if kind.starts_with("deletion") {
            stats.1 = count;
        }
    }
    stats
}

fn parse_history_log(
    output: &str,
    refs_by_sha: &HashMap<String, Vec<GitHistoryRef>>,
) -> Vec<GitHistoryCommit> {
    let fields: Vec<&str> = output.split('\0').collect();
    fields
        .chunks(6)
        .filter_map(|record| {
            if record.len() != 6 {
                return None;
            }
            let sha = record[0].trim_start_matches(['\r', '\n']);
            if sha.is_empty() {
                return None;
            }
            Some(GitHistoryCommit {
                sha: sha.to_string(),
                parent_shas: record[1]
                    .split_ascii_whitespace()
                    .map(str::to_string)
                    .collect(),
                subject: bounded_field(record[2], 4_096),
                author_name: bounded_field(record[3], 512),
                author_email: bounded_field(record[4], 512),
                authored_at: bounded_field(record[5], 128),
                refs: refs_by_sha.get(sha).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

fn parse_history_refs(output: &str) -> HashMap<String, Vec<GitHistoryRef>> {
    let fields: Vec<&str> = output.split('\0').collect();
    let mut refs_by_sha: HashMap<String, Vec<GitHistoryRef>> = HashMap::new();
    for record in fields.chunks(6) {
        if record.len() != 6 {
            continue;
        }
        let full_name = record[0].trim_start_matches(['\r', '\n']);
        let object_sha = record[1];
        let object_type = record[2];
        let peeled_sha = record[3];
        let peeled_type = record[4];
        let symbolic_target = record[5];
        if full_name.is_empty() || !symbolic_target.is_empty() {
            continue;
        }
        let Some((kind, label)) = (if let Some(label) = full_name.strip_prefix("refs/heads/") {
            Some((GitHistoryRefKind::Branch, label))
        } else if let Some(label) = full_name.strip_prefix("refs/remotes/") {
            Some((GitHistoryRefKind::Remote, label))
        } else {
            full_name
                .strip_prefix("refs/tags/")
                .map(|label| (GitHistoryRefKind::Tag, label))
        }) else {
            continue;
        };
        let target_sha = if object_type == "commit" {
            object_sha
        } else if object_type == "tag" && peeled_type == "commit" {
            peeled_sha
        } else {
            continue;
        };
        refs_by_sha
            .entry(target_sha.to_string())
            .or_default()
            .push(GitHistoryRef {
                kind,
                label: bounded_field(label, 1_024),
            });
    }
    for refs in refs_by_sha.values_mut() {
        refs.sort_by(|left, right| {
            let order = |kind| match kind {
                GitHistoryRefKind::Branch => 0,
                GitHistoryRefKind::Tag => 1,
                GitHistoryRefKind::Remote => 2,
            };
            order(left.kind)
                .cmp(&order(right.kind))
                .then_with(|| left.label.cmp(&right.label))
        });
    }
    refs_by_sha
}

/// Absolute form of a possibly-relative path (no filesystem access).
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_open_targets_use_the_rpc_names() {
        assert!(matches!(
            serde_json::from_str::<ProjectOpenTarget>("\"vsCode\""),
            Ok(ProjectOpenTarget::VsCode)
        ));
        assert!(matches!(
            serde_json::from_str::<ProjectOpenTarget>("\"files\""),
            Ok(ProjectOpenTarget::Files)
        ));
    }

    #[test]
    fn opening_a_missing_project_returns_an_error() {
        let data = tempfile::tempdir().unwrap();
        let repos = Repos::new(data.path(), "device");
        let missing = data.path().join("missing-project");

        assert!(
            repos
                .open_project(&missing, ProjectOpenTarget::Cursor)
                .is_err()
        );
    }

    #[test]
    fn github_remote_urls_normalize_https_and_ssh_forms() {
        for remote in [
            "https://github.com/owner/repo",
            "https://github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com:22/owner/repo.git",
            "https://github.com:443/owner/repo.git",
        ] {
            assert_eq!(
                github_url_from_remote(remote).as_deref(),
                Some("https://github.com/owner/repo"),
                "{remote}"
            );
        }
    }

    #[test]
    fn github_remote_urls_strip_credentials_and_reject_other_hosts() {
        assert_eq!(
            github_url_from_remote("https://local-user@github.com/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
        for remote in [
            "https://gitlab.com/owner/repo.git",
            "https://github.com.evil.test/owner/repo.git",
            "ssh://git@github.com:abc/owner/repo.git",
            "https://github.com/owner",
            "https://github.com/owner/repo/tree/main",
            "https://github.com/owner/repo?token=secret",
            "file:///owner/repo",
        ] {
            assert_eq!(github_url_from_remote(remote), None, "{remote}");
        }
    }

    #[tokio::test]
    async fn github_repo_url_prefers_origin_and_returns_none_without_a_github_remote() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        repos
            .git(&["init", "--quiet"], Some(root.path()))
            .await
            .unwrap();

        assert_eq!(repos.github_repo_url(root.path()).await.unwrap(), None);

        repos
            .git(
                &["remote", "add", "origin", "git@github.com:owner/origin.git"],
                Some(root.path()),
            )
            .await
            .unwrap();
        repos
            .git(
                &[
                    "remote",
                    "add",
                    "upstream",
                    "https://github.com/owner/upstream.git",
                ],
                Some(root.path()),
            )
            .await
            .unwrap();
        assert_eq!(
            repos.github_repo_url(root.path()).await.unwrap().as_deref(),
            Some("https://github.com/owner/origin")
        );
    }

    #[test]
    fn gh_pull_request_json_counts_check_outcomes() {
        let json = br#"{
            "number": 52, "title": "feat(ui): add GitHub", "url": "https://github.com/o/r/pull/52",
            "state": "OPEN", "isDraft": false, "baseRefName": "main",
            "reviewDecision": "REVIEW_REQUIRED",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""},
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"__typename": "StatusContext", "state": "PENDING"},
                {"__typename": "StatusContext", "state": "SUCCESS"}
            ]
        }"#;
        let pr = parse_gh_pull_request(json).unwrap();
        assert_eq!(pr.number, 52);
        assert_eq!(pr.base_ref, "main");
        assert_eq!(pr.review_decision.as_deref(), Some("REVIEW_REQUIRED"));
        assert_eq!(
            pr.checks,
            GitHubChecks {
                passed: 2,
                failed: 1,
                pending: 2
            }
        );
        assert_eq!(parse_gh_pull_request(b"not json"), None);
    }

    #[test]
    fn shortstat_parses_insertions_and_deletions() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 280 insertions(+), 6 deletions(-)"),
            (280, 6)
        );
        assert_eq!(parse_shortstat(" 1 file changed, 1 insertion(+)"), (1, 0));
        assert_eq!(parse_shortstat(""), (0, 0));
    }

    #[tokio::test]
    async fn github_status_reports_branch_commits_and_uncommitted_changes() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        repos
            .git(&["init", "--quiet", "-b", "feature"], Some(root.path()))
            .await
            .unwrap();
        assert_eq!(repos.github_status(root.path()).await.unwrap(), None);

        repos
            .git(
                &["remote", "add", "origin", "git@github.com:owner/repo.git"],
                Some(root.path()),
            )
            .await
            .unwrap();
        std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
        repos
            .git(&["add", "a.txt"], Some(root.path()))
            .await
            .unwrap();
        repos
            .git(
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@example.com",
                    "commit",
                    "--quiet",
                    "-m",
                    "first commit",
                ],
                Some(root.path()),
            )
            .await
            .unwrap();
        repos
            .git(&["branch", "review"], Some(root.path()))
            .await
            .unwrap();
        std::fs::write(root.path().join("a.txt"), "branch version\n").unwrap();
        repos
            .git(&["add", "a.txt"], Some(root.path()))
            .await
            .unwrap();
        repos
            .git(
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@example.com",
                    "commit",
                    "--quiet",
                    "-m",
                    "second commit",
                ],
                Some(root.path()),
            )
            .await
            .unwrap();
        std::fs::write(root.path().join("a.txt"), "two\nthree\n").unwrap();

        let status = repos.github_status(root.path()).await.unwrap().unwrap();
        assert_eq!(status.repo_url, "https://github.com/owner/repo");
        assert_eq!(status.branch, "feature");
        assert_eq!(status.recent_commits.len(), 2);
        assert_eq!(status.recent_commits[0].subject, "second commit");
        assert_eq!((status.insertions, status.deletions), (2, 1));

        let review = repos
            .github_status_for_branch(root.path(), Some("review"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(review.branch, "review");
        assert_eq!(review.recent_commits.len(), 1);
        assert_eq!(review.recent_commits[0].subject, "first commit");
        assert_eq!(repos.current_branch(root.path()).await.unwrap(), "feature");
        assert!(
            repos
                .github_status_for_branch(root.path(), Some("not-a-branch"))
                .await
                .is_err()
        );
    }

    #[test]
    fn fuzzy_score_matches_a_path_subsequence() {
        assert!(fuzzy_score("cmp rs", "crates/ui/src/composer.rs").is_some());
        assert!(fuzzy_score("composer crates", "crates/ui/src/composer.rs").is_some());
        assert!(fuzzy_score("xyz", "crates/ui/src/composer.rs").is_none());
    }

    #[test]
    fn search_files_obeys_gitignore_and_returns_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();
        std::fs::write(root.path().join(".secret"), "").unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored\n").unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored/nope.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "src", &[]).unwrap();
        assert!(
            matches
                .iter()
                .any(|entry| entry.path == "src" && entry.is_dir)
        );
        assert!(matches.iter().any(|entry| entry.path == "src/composer.rs"));
        assert!(
            !matches
                .iter()
                .any(|entry| entry.path.starts_with("ignored"))
        );
        assert!(
            search_files_blocking(root.path(), "secret", &[])
                .unwrap()
                .iter()
                .any(|entry| entry.path == ".secret")
        );
        assert!(!matches.iter().any(|entry| entry.path.starts_with(".git/")));
    }

    #[test]
    fn empty_search_features_recent_paths_before_shallow_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        std::fs::write(root.path().join("src/deep.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "", &["src/deep.rs".into()]).unwrap();
        assert_eq!(
            matches.first().map(|entry| entry.path.as_str()),
            Some("src/deep.rs")
        );
        assert!(matches.iter().any(|entry| entry.path == "README.md"));
    }

    #[test]
    fn search_files_prefers_filename_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("composer/docs")).unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("composer/docs/readme.md"), "").unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "composer", &[]).unwrap();
        let composer = matches
            .iter()
            .position(|entry| entry.path == "src/composer.rs")
            .unwrap();
        let path_only = matches
            .iter()
            .position(|entry| entry.path == "composer/docs/readme.md")
            .unwrap();
        assert!(composer < path_only);
    }

    #[test]
    fn cancelled_search_stops_before_walking() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        let cancelled = AtomicBool::new(true);

        let err = search_files_blocking_with_cancel(root.path(), "", &[], || {
            cancelled.load(Ordering::Relaxed)
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"));
    }

    #[tokio::test]
    async fn workspace_checkout_rejects_sibling_paths() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        assert_eq!(
            repos.workspace_checkout(root.path(), root.path()).await,
            std::fs::canonicalize(root.path()).ok()
        );
        assert!(
            repos
                .workspace_checkout(root.path(), sibling.path())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_searches_of_one_checkout_do_not_cancel_each_other() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("alpha.rs"), "").unwrap();
        std::fs::write(root.path().join("beta.rs"), "").unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        let alpha = repos.search_files(root.path().into(), "alpha".into(), Vec::new());
        let beta = repos.search_files(root.path().into(), "beta".into(), Vec::new());
        let (alpha, beta) = tokio::join!(alpha, beta);

        assert_eq!(alpha.unwrap()[0].path, "alpha.rs");
        assert_eq!(beta.unwrap()[0].path, "beta.rs");
    }
}
