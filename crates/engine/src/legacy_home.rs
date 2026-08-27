//! One-time legacy `~/.zeron` → `~/.komet` home migration (boot, best-effort).
//!
//! Builds before the Komet rename wrote device-scoped state under `~/.zeron`:
//! the Cursor shim's per-run agent stores (`cursor-state/`), the managed npm
//! adapter installs (`adapters/<pkg>/<version>` with a `.zeron-install-ok`
//! completion marker), and managed git worktrees (`worktrees/`). The current
//! build resolves all three under `~/.komet`, so without a migration an
//! upgrader silently loses Cursor resume continuity (the shim can no longer
//! find its agent stores) and re-downloads every adapter from npm.
//!
//! [`migrate`] moves each subtree into place when the target does not already
//! exist, renames the old completion markers so the new code recognizes the
//! installs as complete, and logs one warning summarizing what moved. It is
//! deliberately forgiving: every step fails soft, a partial or skipped
//! migration only costs re-downloads / lost resume — never a boot failure.
//! A marker file records completion so later boots don't rescan.

use std::path::{Path, PathBuf};

const LEGACY_HOME_DIR: &str = ".zeron";
const NEW_HOME_DIR: &str = ".komet";
/// Records that migration ran; `{data_dir}/legacy-zeron-home-migrated`.
const MIGRATED_MARKER: &str = "legacy-zeron-home-migrated";

const OK_MARKER_LEGACY: &str = ".zeron-install-ok";
const OK_MARKER: &str = ".komet-install-ok";

/// Subtrees an old build could have left behind. Each entry migrates
/// independently.
const SUBTREES: &[&str] = &["cursor-state", "adapters", "worktrees"];

/// Run once per data dir. Returns early when there is no legacy home, the
/// migration already ran, or HOME is unset.
pub fn migrate(data_dir: &Path) {
    let Some(home) = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
    else {
        return;
    };
    migrate_home(
        &home.join(LEGACY_HOME_DIR),
        &home.join(NEW_HOME_DIR),
        data_dir,
    );
}

/// Test/ex seam: explicit roots instead of `$HOME`.
fn migrate_home(legacy_root: &Path, new_root: &Path, data_dir: &Path) {
    if !legacy_root.is_dir() || data_dir.join(MIGRATED_MARKER).exists() {
        return;
    }
    let mut moved: Vec<&str> = Vec::new();
    for subtree in SUBTREES {
        let from = legacy_root.join(subtree);
        if !from.is_dir() {
            continue;
        }
        let to = new_root.join(subtree);
        if to.exists() {
            // A newer install already populated this subtree — leave both in
            // place rather than merge directories we don't own.
            continue;
        }
        if move_dir(&from, &to) {
            if *subtree == "adapters" {
                rewrite_install_markers(&to);
            }
            moved.push(subtree);
        }
    }
    if moved.is_empty() {
        // Nothing movable (targets pre-existed): still stamp the marker so we
        // stop rescanning on every boot.
        let _ = std::fs::write(data_dir.join(MIGRATED_MARKER), "checked\n");
        return;
    }
    tracing::warn!(
        subtrees = ?moved,
        legacy = %legacy_root.display(),
        target = %new_root.display(),
        "migrated legacy ~/.zeron state to ~/.komet (Komet rename); sessions \
         created by builds before the rename keep their Cursor resume and \
         installed adapters"
    );
    let _ = std::fs::write(data_dir.join(MIGRATED_MARKER), "migrated\n");
}

/// Rename `from` → `to`, falling back to copy+delete when rename crosses a
/// filesystem boundary (EXDEV) or otherwise fails. Creates parents. Fails
/// soft: `false` just means this subtree stays where it was.
fn move_dir(from: &Path, to: &Path) -> bool {
    if let Some(parent) = to.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %err, dir = %parent.display(), "legacy-home parent create failed");
        return false;
    }
    match std::fs::rename(from, to) {
        Ok(()) => true,
        Err(_) => copy_dir_recursive(from, to)
            .is_ok()
            .then(|| remove_dir_all_best_effort(from))
            .unwrap_or(false),
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), dst)?;
        }
        // Symlinks inside these trees are not expected; skipping them is safe.
    }
    Ok(())
}

fn remove_dir_all_best_effort(dir: &Path) -> bool {
    std::fs::remove_dir_all(dir).is_ok()
}

/// Old builds stamped completed installs with `.zeron-install-ok`; the current
/// code only trusts `.komet-install-ok`. Rename in place (flat shape:
/// `<pkg>/<version>/.zeron-install-ok`).
fn rewrite_install_markers(adapters_dir: &Path) {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&entry.path(), found);
            } else if entry.file_name() == OK_MARKER_LEGACY {
                found.push(entry.path());
            }
        }
    }
    let mut markers = Vec::new();
    walk(adapters_dir, &mut markers);
    for legacy_marker in markers {
        let renamed = legacy_marker.with_file_name(OK_MARKER);
        if let Err(err) = std::fs::rename(&legacy_marker, &renamed) {
            tracing::warn!(
                error = %err,
                marker = %legacy_marker.display(),
                "legacy install marker rename failed (adapter will re-download)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_cursor_state_and_renames_adapter_markers() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let cursor = root.join(".zeron/cursor-state/by-agent/x");
        std::fs::create_dir_all(&cursor).unwrap();
        std::fs::write(cursor.join("agent"), "1").unwrap();
        let version = root.join(".zeron/adapters/pkg__acp/1.2.3");
        std::fs::create_dir_all(&version).unwrap();
        std::fs::write(version.join(OK_MARKER_LEGACY), "1.2.3").unwrap();
        let wt = root.join(".zeron/worktrees/repo/wt");
        std::fs::create_dir_all(&wt).unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        migrate_home(&root.join(".zeron"), &root.join(".komet"), data_dir.path());

        assert!(
            root.join(".komet/cursor-state/by-agent/x/agent")
                .exists(),
            "cursor-state moved"
        );
        assert!(!root.join(".zeron/cursor-state").exists());
        let migrated_version = root.join(".komet/adapters/pkg__acp/1.2.3");
        assert!(migrated_version.join(OK_MARKER).exists(), "marker rewritten");
        assert!(!migrated_version.join(OK_MARKER_LEGACY).exists());
        assert!(root.join(".komet/worktrees/repo/wt").exists());
        assert!(data_dir.path().join(MIGRATED_MARKER).exists());

        // Second run is a no-op (marker short-circuits).
        std::fs::create_dir_all(root.join(".zeron/cursor-state")).unwrap();
        migrate_home(&root.join(".zeron"), &root.join(".komet"), data_dir.path());
        assert!(
            !root.join(".komet/cursor-state/by-agent/x/agent").exists() == false,
            "existing migrated tree untouched"
        );
        assert!(root.join(".zeron/cursor-state").exists());
    }

    #[test]
    fn skips_subtrees_that_already_exist_under_the_new_home() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let old = root.join(".zeron/cursor-state");
        std::fs::create_dir_all(old.join("old")).unwrap();
        let new = root.join(".komet/cursor-state");
        std::fs::create_dir_all(new.join("new")).unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        migrate_home(&root.join(".zeron"), &root.join(".komet"), data_dir.path());

        assert!(old.join("old").exists(), "legacy left untouched");
        assert!(new.join("new").exists(), "existing tree untouched");
        assert!(data_dir.path().join(MIGRATED_MARKER).exists());
    }

    #[test]
    fn no_legacy_home_is_a_clean_noop() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        migrate_home(
            &home.path().join(".zeron"),
            &home.path().join(".komet"),
            data_dir.path(),
        );
        assert!(!home.path().join(".komet").exists());
        // No marker either: a later real legacy home should still migrate.
        assert!(!data_dir.path().join(MIGRATED_MARKER).exists());
    }
}
