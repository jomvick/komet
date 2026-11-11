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

/// Substrings that mark a path (or any stored string) as carrying a
/// credential-shaped value: well-known token prefixes and PEM armor.
const SECRET_VALUE_PATTERNS: &[&str] = &[
    "-----begin",
    "sk-",
    "sk_live",
    "rk_live",
    "pk_live",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "akia",
    "asia",
    "aiza",
    "ya29.",
    "dop_v1_",
    "shpat_",
    "npm_",
    "pypi-",
    "hf_",
    "ntn_",
    "vercel_",
    "lin_api_",
];

/// Stable project id for a run cwd: `p_` + sha256(canonical cwd)[..16].
///
/// Canonicalization resolves symlinks so two spellings of the same folder
/// share one memory; when the path does not exist (deleted workspace, test
/// fixtures) we hash the absolute lexical form instead — still stable.
pub fn project_id_for(cwd: &str) -> String {
    let trimmed = cwd.trim();
    let canonical = std::fs::canonicalize(trimmed)
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| absolute_lexical(trimmed));
    let digest = Sha256::digest(canonical.as_bytes());
    let hex = crate::hex(&digest[..8]);
    format!("p_{hex}")
}

/// Best-effort absolute path without touching the filesystem: expand a
/// leading `~`, keep the input otherwise. Used only when canonicalize fails.
fn absolute_lexical(path: &str) -> String {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        dirs_home()
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    };
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
/// `cwd` is the chat's run directory; `raw` is the path as the harness
/// reported it (absolute or already relative). The result uses `/`
/// separators and never contains a leading `/`.
pub fn project_relative(cwd: &str, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 4096 {
        return None;
    }
    let rel = relativize(cwd, raw)?;
    if is_default_excluded(&rel) || contains_secret_value(&rel) {
        return None;
    }
    Some(rel)
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

/// Credential-shaped content check used on any stored free-form string
/// (paths today; kept public for the engine's own scrubbing needs).
pub fn contains_secret_value(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    SECRET_VALUE_PATTERNS.iter().any(|pat| lower.contains(pat))
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

/// Strip `cwd` from an absolute `raw` path, or normalize an already-relative
/// one. Returns `/`-joined relative path or `None` when the path escapes the
/// project or cannot be normalized.
fn relativize(cwd: &str, raw: &str) -> Option<String> {
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        // Try the reported cwd, then its canonical form (symlinked workspaces),
        // then the canonicalized raw path against the canonical cwd.
        for base in [PathBuf::from(cwd), canonical(cwd)] {
            if let Ok(rel) = raw_path.strip_prefix(&base) {
                return normalize_rel(rel);
            }
        }
        if let Ok(canon_raw) = std::fs::canonicalize(raw_path) {
            let canon_cwd = canonical(cwd);
            if let Ok(rel) = canon_raw.strip_prefix(&canon_cwd) {
                return normalize_rel(rel);
            }
        }
        return None;
    }
    normalize_rel(raw_path)
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
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
        assert_eq!(
            project_relative(cwd, "/repo/src/main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            project_relative(cwd, "src/./main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            project_relative(cwd, "src/../lib/x.rs").as_deref(),
            Some("lib/x.rs")
        );
        // Outside the project → not stored.
        assert_eq!(project_relative(cwd, "/etc/passwd"), None);
        assert_eq!(project_relative(cwd, "../outside.rs"), None);
        assert_eq!(project_relative(cwd, ""), None);
    }

    #[test]
    fn secret_and_excluded_paths_are_dropped() {
        assert_eq!(project_relative("/repo", "/repo/.env"), None);
        assert_eq!(project_relative("/repo", "/repo/.env.local"), None);
        assert_eq!(project_relative("/repo", "/repo/id_rsa"), None);
        assert_eq!(project_relative("/repo", "/repo/certs/server.pem"), None);
        assert_eq!(project_relative("/repo", "/repo/.ssh/config"), None);
        assert_eq!(project_relative("/repo", "/repo/.git/config"), None);
        assert_eq!(
            project_relative("/repo", "/repo/node_modules/pkg/index.js"),
            None
        );
        // A token-shaped path segment is dropped too.
        assert_eq!(
            project_relative("/repo", "/repo/sk-abcdefghijklmnopqrstuvwxyz"),
            None
        );
        // Ordinary code paths survive.
        assert_eq!(
            project_relative("/repo", "/repo/src/env.rs").as_deref(),
            Some("src/env.rs")
        );
        assert_eq!(
            project_relative("/repo", "/repo/docs/environment.md").as_deref(),
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
