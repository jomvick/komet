//! Composer widget strip: the Changes / subagents / processes chips that sit
//! above the prompt composer. Read-only views over data the shell already
//! holds (no RPC, no writes to the agent).
//!
//! Pure helpers live here first; the gpui rendering is added in later tasks.

use crate::changes::FileDiff;

/// A counter rendered in a chip: plain below a thousand, one decimal above
/// (`15190` → `15.2k`) so the pill stays short.
pub fn format_count(n: u32) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let text = format!("{:.1}k", f64::from(n) / 1000.0);
    text.replace(".0k", "k")
}

/// Files touched, additions and deletions — `None` when the tree is clean
/// (nothing to show) or the diff could not be read.
pub fn summarize_diff(files: &[FileDiff]) -> Option<(usize, u32, u32)> {
    if files.is_empty() {
        return None;
    }
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    Some((files.len(), additions, deletions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(additions: u32, deletions: u32) -> FileDiff {
        FileDiff {
            path: "a.rs".into(),
            old_path: None,
            status: crate::changes::FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: Vec::new(),
            additions,
            deletions,
            max_line: 0,
        }
    }

    #[test]
    fn format_count_keeps_small_numbers() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
    }

    #[test]
    fn format_count_uses_one_decimal_above_a_thousand() {
        assert_eq!(format_count(1000), "1k");
        assert_eq!(format_count(1200), "1.2k");
        assert_eq!(format_count(15190), "15.2k");
    }

    #[test]
    fn summarize_diff_is_none_without_files() {
        assert_eq!(summarize_diff(&[]), None);
    }

    #[test]
    fn summarize_diff_sums_files_and_lines() {
        let files = [file(5, 1), file(0, 2)];
        assert_eq!(summarize_diff(&files), Some((2, 5, 3)));
    }
}
