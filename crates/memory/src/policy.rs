//! Privacy policy for the activity index: project identity, path
//! relativization, exclusion lists, and secret-like value detection.
//!
//! Everything here is pure and cheap — it runs on the engine's publish path,
//! so no allocation-heavy work and no I/O beyond one canonicalize attempt per
//! distinct cwd (cached by the caller).

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// Directory names that never produce a `relative_path`, anywhere in the
/// path. Build outputs and VCS internals are noise; secret-bearing dot dirs
/// are covered by [`SECRET_NAME_PATTERNS`] as well.
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".output",
    ".turbo",
    ".vercel",
    ".cache",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".venv",
    "venv",
    ".idea",
    ".vs",
    "coverage",
];

/// File/dir name fragments that mark a path as secret-bearing. Matched
/// case-insensitively against every path component.
const SECRET_NAME_PATTERNS: &[&str] = &[
    ".env",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".keystore",
    ".jks",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".htpasswd",
    ".pgpass",
    ".my.cnf",
    ".kubeconfig",
    "credentials",
    "secret",
    "secrets",
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    "known_hosts",
];

/// Credential-shaped content check used on any stored free-form string
/// (paths today; kept public for the engine's own scrubbing needs).
///
/// Prefix patterns are ANCHORED: the prefix must start at a token boundary
/// (preceded by start/non-token char) and be followed by a realistic secret
/// tail — otherwise `task-runner.ts`, `disk-usage.rs` or `asian-markets`
/// would be silently dropped. Name patterns (`.env`, `*.pem`, `secret`, …)
/// stay liberal: failing safe on a path name costs one row, missing a real
/// secret costs a leak.
pub fn contains_secret_value(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin") {
        return true;
    }
    SECRET_VALUE_PREFIXES.iter().any(|(prefix, min_tail)| {
        let mut search_from = 0usize;
        while let Some(pos) = lower[search_from..].find(prefix) {
            let start = search_from + pos;
            let preceded_ok = start == 0 || !is_token_char(lower.as_bytes()[start - 1]);
            if preceded_ok {
                let tail: usize = lower.as_bytes()[start + prefix.len()..]
                    .iter()
                    .take_while(|b| is_token_char(**b))
                    .count();
                if tail >= *min_tail {
                    return true;
                }
            }
            search_from = start + prefix.len();
        }
        false
    })
}

/// `[a-z0-9_-]` — the charset real tokens are drawn from (and what must
/// NOT precede a prefix for the boundary check to hold).
fn is_token_char(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
}

/// (prefix, minimum trailing token chars) for well-known credential shapes.
/// The tail minimums are deliberately conservative: a false negative on an
/// exotic format is covered by the name patterns, a false positive silently
/// deletes legitimate paths from memory.
const SECRET_VALUE_PREFIXES: &[(&str, usize)] = &[
    ("sk-", 20),         // OpenAI-style keys
    ("sk_live_", 10),    // Stripe live secret
    ("rk_live_", 10),    // Stripe restricted live
    ("pk_live_", 10),    // Stripe publishable (still credential-shaped)
    ("ghp_", 20),        // GitHub PAT (classic)
    ("gho_", 20),        // GitHub OAuth
    ("ghu_", 20),        // GitHub user-to-server
    ("ghs_", 20),        // GitHub server-to-server
    ("ghr_", 20),        // GitHub refresh
    ("github_pat_", 20), // GitHub fine-grained PAT
    ("glpat-", 15),      // GitLab PAT
    ("xoxb-", 10),       // Slack bot
    ("xoxp-", 10),       // Slack user
    ("xapp-", 10),       // Slack app-level
    ("xoxa-", 10),       // Slack app config
    ("xoxr-", 10),       // Slack refresh
    ("xoxs-", 10),       // Slack signing secret? (config token)
    ("akia", 16),        // AWS access key id
    ("asia", 16),        // AWS temporary key id
    ("aiza", 20),        // Google API key
    ("ya29.", 10),       // Google OAuth access token
    ("dop_v1_", 10),     // DigitalOcean PAT
    ("shpat_", 10),      // Shopify access token
    ("npm_", 20),        // npm access token
    ("pypi-", 10),       // PyPI token
    ("hf_", 20),         // Hugging Face token
    ("ntn_", 20),        // Notion token
    ("vercel_", 10),     // Vercel token
    ("lin_api_", 10),    // Linear API key
];

/// Stable project id: `p_` + sha256(identity)[..16], where identity is the
/// repository's COMMON git dir when `cwd` sits inside a git worktree, else
/// the canonical cwd.
///
/// All worktrees of one repository (main checkout, `git worktree` links,
/// submodules resolved through their `.git` file) share a memory: the id is
/// derived from `<repo>/.git` (or the resolved `commondir`), never from the
/// worktree root or a subdirectory. Chats launched in `/repo` and
/// `/repo/crates/x` likewise land on the same project.
///
/// Canonicalization resolves symlinks so two spellings of the same folder
/// share one memory; when nothing exists on disk (deleted workspace, test
/// fixtures) we hash the absolute lexical form — still stable.
pub fn project_id_for(cwd: &str) -> String {
    let trimmed = cwd.trim();
    let identity = git_common_dir(trimmed)
        .and_then(|p| p.canonicalize().ok().or(Some(p)))
        .and_then(|p| p.to_str().map(str::to_string))
        .or_else(|| {
            std::fs::canonicalize(trimmed)
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
        })
        .unwrap_or_else(|| absolute_lexical(trimmed));
    let digest = Sha256::digest(identity.as_bytes());
    let hex = crate::hex(&digest[..8]);
    format!("p_{hex}")
}

/// The shared `.git` directory for the repository containing `cwd`, found
/// by walking ancestors for a `.git` entry. A `.git` *file* is a worktree /
/// submodule pointer (`gitdir: …`); its `commondir` file resolves to the
/// repository's real git dir so linked worktrees share one identity.
/// `None` outside any git repository.
fn git_common_dir(cwd: &str) -> Option<PathBuf> {
    let base = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(absolute_lexical(cwd)));
    for dir in base.ancestors() {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return Some(dotgit);
        }
        if !dotgit.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&dotgit) else {
            continue;
        };
        let Some(target) = text.trim().strip_prefix("gitdir:") else {
            continue;
        };
        let target = target.trim();
        let gitdir = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            dir.join(target)
        };
        // Linked worktrees carry `commondir` (usually "../..") pointing at
        // the main checkout's .git. Submodules may lack it — the gitdir
        // itself is still a stable per-repo identity.
        if let Ok(common) = std::fs::read_to_string(gitdir.join("commondir")) {
            let common = common.trim();
            let common = if Path::new(common).is_absolute() {
                PathBuf::from(common)
            } else {
                gitdir.join(common)
            };
            return Some(normalize_abs(&common));
        }
        return Some(gitdir);
    }
    None
}

/// Lexically resolve `.`/`..` in an absolute path (no symlink resolution —
/// `canonicalize` is attempted by the caller afterwards).
fn normalize_abs(path: &Path) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => {
                parts.push(comp.as_os_str().to_os_string())
            }
            Component::CurDir => {}
            Component::ParentDir => {
                // `..` at the root stays at the root.
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            Component::Normal(s) => parts.push(s.to_os_string()),
        }
    }
    parts.into_iter().collect()
}

/// Best-effort absolute path without touching the filesystem: expand a
/// leading `~`, keep the input otherwise. Used only when canonicalize fails.
fn absolute_lexical(path: &str) -> String {
    let expanded = expand_tilde(path);
    if Path::new(&expanded).is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded).to_string_lossy().into_owned())
            .unwrap_or(expanded)
    }
}

fn dirs_home() -> Option<String> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string_lossy().into_owned())
}

/// Project-relative form of a tool-call path, or `None` when the path must
/// not be stored: outside the project, escaping via `..`, empty, or matching
/// the built-in secret/exclusion policy.
///
/// `cwd` is the chat's run directory; `canon_cwd` its canonical form,
/// computed once per chat by the caller (canonicalize is a syscall and must
/// not run per tool call); `raw` is the path as the harness reported it
/// (absolute or already relative). The result uses `/` separators and never
/// contains a leading `/`.
pub fn project_relative(cwd: &str, canon_cwd: &Path, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 4096 {
        return None;
    }
    let rel = relativize(cwd, canon_cwd, raw)?;
    if is_default_excluded(&rel) || contains_secret_value(&rel) {
        return None;
    }
    Some(rel)
}

/// Canonical form of `cwd` for [`project_relative`]: resolved symlinks when
/// the path exists, the absolute lexical form otherwise. Callers cache the
/// result per chat.
pub fn canonical_cwd(cwd: &str) -> PathBuf {
    let expanded = if cwd.trim().is_empty() {
        cwd.trim().to_string()
    } else {
        expand_tilde(cwd.trim())
    };
    std::fs::canonicalize(&expanded).unwrap_or_else(|_| PathBuf::from(absolute_lexical(&expanded)))
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs_home()
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    }
}

/// Whether a project-relative path hits the built-in denylist (VCS dirs,
/// dependency/build output, secret-bearing names).
pub fn is_default_excluded(rel: &str) -> bool {
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        let lower = part.to_ascii_lowercase();
        if EXCLUDED_DIRS.contains(&lower.as_str()) {
            return true;
        }
        if SECRET_NAME_PATTERNS.iter().any(|pat| lower.contains(pat)) {
            return true;
        }
    }
    false
}

/// Whether `rel` matches one of the project's user-configured exclusion
/// patterns. Patterns are matched against the whole relative path:
/// `*` matches any run of characters (including `/`), `?` one character;
/// a pattern without wildcards matches as a path-prefix on a `/` boundary
/// (`secrets` excludes `secrets/x` but not `src/secrets/x`? — it DOES match
/// any component boundary: we match if the pattern equals the path or is a
/// leading segment sequence).
pub fn user_excluded(rel: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pat| {
        let pat = pat.trim();
        if pat.is_empty() {
            return false;
        }
        if pat.contains('*') || pat.contains('?') {
            glob_match(pat, rel)
        } else {
            rel == pat
                || rel
                    .strip_prefix(pat)
                    .is_some_and(|rest| rest.starts_with('/'))
                || rel
                    .split('/')
                    .collect::<Vec<_>>()
                    .windows(pat.split('/').count())
                    .any(|w| w.join("/") == pat)
        }
    })
}

/// Small glob: `*` → any sequence, `?` → one char, everything else literal.
/// Case-sensitive; patterns are user-provided so we keep semantics obvious.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Strip `cwd` (or its canonical form) from an absolute `raw` path, or
/// normalize an already-relative one. Returns `/`-joined relative path or
/// `None` when the path escapes the project or cannot be normalized.
fn relativize(cwd: &str, canon_cwd: &Path, raw: &str) -> Option<String> {
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        // Try the reported cwd, then its canonical form (symlinked
        // workspaces), then the canonicalized raw path against canon_cwd.
        for base in [PathBuf::from(cwd), canon_cwd.to_path_buf()] {
            if let Ok(rel) = raw_path.strip_prefix(&base) {
                return normalize_rel(rel);
            }
        }
        if let Ok(canon_raw) = std::fs::canonicalize(raw_path)
            && let Ok(rel) = canon_raw.strip_prefix(canon_cwd)
        {
            return normalize_rel(rel);
        }
        return None;
    }
    normalize_rel(raw_path)
}

/// Lexical normalization of a relative path: resolve `.`/`..` where possible,
/// reject escapes above the root, emit `/`-joined form.
fn normalize_rel(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                // Escapes the project root.
                parts.pop()?;
            }
            // Absolute/prefix components inside a "relative" path: reject.
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_id_is_stable_and_path_derived() {
        let a = project_id_for("/tmp/definitely-not-here-xyz");
        let b = project_id_for("/tmp/definitely-not-here-xyz");
        assert_eq!(a, b);
        assert!(a.starts_with("p_"));
        assert_ne!(a, project_id_for("/tmp/other-xyz"));
    }

    #[test]
    fn relative_paths_stay_inside_project() {
        let cwd = "/repo";
        let canon = canonical_cwd(cwd);
        assert_eq!(
            project_relative(cwd, &canon, "/repo/src/main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            project_relative(cwd, &canon, "src/./main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            project_relative(cwd, &canon, "src/../lib/x.rs").as_deref(),
            Some("lib/x.rs")
        );
        // Outside the project → not stored.
        assert_eq!(project_relative(cwd, &canon, "/etc/passwd"), None);
        assert_eq!(project_relative(cwd, &canon, "../outside.rs"), None);
        assert_eq!(project_relative(cwd, &canon, ""), None);
    }

    #[test]
    fn secret_and_excluded_paths_are_dropped() {
        let canon = canonical_cwd("/repo");
        assert_eq!(project_relative("/repo", &canon, "/repo/.env"), None);
        assert_eq!(project_relative("/repo", &canon, "/repo/.env.local"), None);
        assert_eq!(project_relative("/repo", &canon, "/repo/id_rsa"), None);
        assert_eq!(
            project_relative("/repo", &canon, "/repo/certs/server.pem"),
            None
        );
        assert_eq!(project_relative("/repo", &canon, "/repo/.ssh/config"), None);
        assert_eq!(project_relative("/repo", &canon, "/repo/.git/config"), None);
        assert_eq!(
            project_relative("/repo", &canon, "/repo/node_modules/pkg/index.js"),
            None
        );
        // A token-shaped path segment is dropped too.
        assert_eq!(
            project_relative("/repo", &canon, "/repo/sk-abcdefghijklmnopqrstuvwxyz"),
            None
        );
        // Ordinary code paths survive.
        assert_eq!(
            project_relative("/repo", &canon, "/repo/src/env.rs").as_deref(),
            Some("src/env.rs")
        );
        assert_eq!(
            project_relative("/repo", &canon, "/repo/docs/environment.md").as_deref(),
            Some("docs/environment.md")
        );
    }

    #[test]
    fn user_exclusions_match_prefix_and_glob() {
        let pats = vec!["secrets".to_string(), "*.generated.ts".to_string()];
        assert!(user_excluded("secrets/key.txt", &pats));
        assert!(user_excluded("api/secrets/key.txt", &pats));
        assert!(user_excluded("src/foo.generated.ts", &pats));
        assert!(!user_excluded("src/foo.ts", &pats));
        assert!(!user_excluded("secretary/notes.md", &pats));
    }

    #[test]
    fn token_prefixes_require_boundary_and_tail() {
        // Real-shaped secrets are caught.
        assert!(contains_secret_value("key=sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(contains_secret_value(
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        ));
        assert!(contains_secret_value("AKIAIOSFODNN7EXAMPLE"));
        assert!(contains_secret_value("-----BEGIN RSA PRIVATE KEY-----"));
        // Lookalikes survive: prefix mid-word or without a realistic tail.
        assert!(!contains_secret_value("task-runner.ts"));
        assert!(!contains_secret_value("disk-usage.rs"));
        assert!(!contains_secret_value("risk-model.rs"));
        assert!(!contains_secret_value("asian-markets.md"));
        assert!(!contains_secret_value("sk-short"));
    }

    #[test]
    fn git_worktrees_share_one_project_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Main checkout: <root>/repo/.git is a real directory.
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("crates/engine")).unwrap();
        // Linked worktree: <root>/wt/.git is a FILE pointing at the repo's
        // gitdir, whose `commondir` resolves back to <repo>/.git.
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = repo.join(".git/worktrees/wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();

        let main_id = project_id_for(repo.to_str().unwrap());
        let subdir_id = project_id_for(repo.join("crates/engine").to_str().unwrap());
        let wt_id = project_id_for(wt.to_str().unwrap());
        assert_eq!(main_id, subdir_id, "subdir shares the repo memory");
        assert_eq!(main_id, wt_id, "linked worktree shares the repo memory");
        // A non-git directory keeps its own cwd-derived id.
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_ne!(project_id_for(plain.to_str().unwrap()), main_id);
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        // `*` spans `/` — documented semantics for user exclusions.
        assert!(glob_match("src/*.rs", "src/deep/main.rs"));
        assert!(glob_match("secrets/**", "secrets/a/b/c"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
    }
}
