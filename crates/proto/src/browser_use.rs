//! Structured result of a browser-use run (port of freebuff's
//! `agents/browser-use` output schema). The agent writes this JSON object in
//! its final reply; the UI parses it best-effort and renders a readable card
//! instead of the raw JSON. Nothing in the engine validates it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserUseStatus {
    Success,
    Failure,
    Partial,
}

impl BrowserUseStatus {
    pub fn label(self) -> &'static str {
        match self {
            BrowserUseStatus::Success => "success",
            BrowserUseStatus::Failure => "failure",
            BrowserUseStatus::Partial => "partial",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUseStep {
    pub name: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUseConsoleError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUseReport {
    pub overall_status: BrowserUseStatus,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_page_title: Option<String>,
    #[serde(default)]
    pub results: Vec<BrowserUseStep>,
    #[serde(default)]
    pub console_errors: Vec<BrowserUseConsoleError>,
    #[serde(default)]
    pub lessons: Vec<String>,
}

impl BrowserUseReport {
    /// Best-effort extraction from an agent's final text: the whole text, a
    /// fenced ```json block, or the last `{…}` span containing
    /// `"overallStatus"`. `None` when nothing parses — the caller then shows
    /// the text as-is.
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        if let Ok(report) = serde_json::from_str::<Self>(trimmed) {
            return Some(report);
        }
        if !trimmed.contains("\"overallStatus\"") {
            return None;
        }
        for candidate in fenced_blocks(trimmed) {
            if let Ok(report) = serde_json::from_str::<Self>(candidate) {
                return Some(report);
            }
        }
        // Walk every `{` that precedes `"overallStatus"` and try the balanced
        // object starting there, nearest first.
        let key_at = trimmed.rfind("\"overallStatus\"")?;
        let mut starts: Vec<usize> = trimmed[..key_at]
            .match_indices('{')
            .map(|(i, _)| i)
            .collect();
        starts.reverse();
        for start in starts {
            if let Some(end) = balanced_end(trimmed, start)
                && let Ok(report) = serde_json::from_str::<Self>(&trimmed[start..=end])
            {
                return Some(report);
            }
        }
        None
    }

    /// Markdown card for the transcript: status line, steps, console errors
    /// and lessons. Deterministic so row versions stay stable across renders.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let icon = match self.overall_status {
            BrowserUseStatus::Success => "✅",
            BrowserUseStatus::Failure => "❌",
            BrowserUseStatus::Partial => "⚠️",
        };
        out.push_str(&format!(
            "**Browser use — {} {}**\n\n{}\n",
            icon,
            self.overall_status.label(),
            self.summary.trim()
        ));
        if let Some(url) = self.final_url.as_deref().filter(|u| !u.trim().is_empty()) {
            match self
                .final_page_title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
            {
                Some(title) => out.push_str(&format!("\nFinal page: {title} — `{url}`\n")),
                None => out.push_str(&format!("\nFinal page: `{url}`\n")),
            }
        }
        if !self.results.is_empty() {
            out.push_str("\n**Steps**\n\n");
            for step in &self.results {
                let mark = if step.passed { "✓" } else { "✗" };
                out.push_str(&format!("- {mark} {}", step.name.trim()));
                if let Some(details) = step.details.as_deref().filter(|d| !d.trim().is_empty()) {
                    out.push_str(&format!(" — {}", details.trim()));
                }
                if let Some(url) = step.url.as_deref().filter(|u| !u.trim().is_empty()) {
                    out.push_str(&format!(" (`{url}`)"));
                }
                out.push('\n');
            }
        }
        if !self.console_errors.is_empty() {
            out.push_str(&format!(
                "\n**Console errors ({})**\n\n",
                self.console_errors.len()
            ));
            for err in &self.console_errors {
                out.push_str(&format!("- `{}`", err.message.trim()));
                if let Some(url) = err.url.as_deref().filter(|u| !u.trim().is_empty()) {
                    out.push_str(&format!(" — {url}"));
                }
                out.push('\n');
            }
        }
        if !self.lessons.is_empty() {
            out.push_str("\n**Lessons**\n\n");
            for lesson in &self.lessons {
                out.push_str(&format!("- {}\n", lesson.trim()));
            }
        }
        out
    }
}

fn fenced_blocks(text: &str) -> impl Iterator<Item = &str> {
    text.split("```").skip(1).step_by(2).map(|block| {
        // Drop the info string (`json`) on the opening fence line.
        match block.find('\n') {
            Some(nl)
                if block[..nl]
                    .trim()
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric()) =>
            {
                block[nl + 1..].trim()
            }
            _ => block.trim(),
        }
    })
}

/// Index of the `}` closing the object opened at `start`, string-aware.
fn balanced_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            match b {
                b'\\' if !escaped => escaped = true,
                b'"' if !escaped => in_str = false,
                _ => escaped = false,
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"{"overallStatus":"partial","summary":"Login form renders; submit failed.","finalUrl":"http://localhost:3000/login","finalPageTitle":"Login","results":[{"name":"Open login page","passed":true,"url":"http://localhost:3000/login"},{"name":"Submit form","passed":false,"details":"500 from /api/login"}],"consoleErrors":[{"message":"TypeError: x is undefined","url":"http://localhost:3000/login"}],"lessons":["Wait for the spinner before snapshotting"]}"#;

    #[test]
    fn parses_bare_json() {
        let r = BrowserUseReport::parse(REPORT).unwrap();
        assert_eq!(r.overall_status, BrowserUseStatus::Partial);
        assert_eq!(r.results.len(), 2);
        assert_eq!(r.console_errors.len(), 1);
        assert_eq!(r.lessons.len(), 1);
    }

    #[test]
    fn parses_fenced_block_and_prose_wrapped_object() {
        let fenced = format!("Here is the result:\n\n```json\n{REPORT}\n```\n");
        assert!(BrowserUseReport::parse(&fenced).is_some());
        let prose = format!("Done. {REPORT} Let me know if you want more.");
        let r = BrowserUseReport::parse(&prose).unwrap();
        assert_eq!(r.summary, "Login form renders; submit failed.");
    }

    #[test]
    fn braces_inside_strings_do_not_break_extraction() {
        let text = format!(
            "Note: the page shows `{{}}`. {}",
            REPORT.replace("500 from /api/login", "got {\\\"code\\\":500}")
        );
        let r = BrowserUseReport::parse(&text).unwrap();
        assert_eq!(r.results[1].details.as_deref(), Some("got {\"code\":500}"));
    }

    #[test]
    fn rejects_plain_text_and_unrelated_json() {
        assert!(BrowserUseReport::parse("All good, no errors.").is_none());
        assert!(BrowserUseReport::parse(r#"{"status":"ok"}"#).is_none());
        assert!(BrowserUseReport::parse(r#"{"overallStatus":"weird","summary":"x"}"#).is_none());
    }

    #[test]
    fn markdown_card_lists_steps_errors_and_lessons() {
        let md = BrowserUseReport::parse(REPORT).unwrap().to_markdown();
        assert!(md.starts_with("**Browser use — ⚠️ partial**"));
        assert!(md.contains("- ✓ Open login page (`http://localhost:3000/login`)"));
        assert!(md.contains("- ✗ Submit form — 500 from /api/login"));
        assert!(md.contains("**Console errors (1)**"));
        assert!(md.contains("- `TypeError: x is undefined` — http://localhost:3000/login"));
        assert!(md.contains("- Wait for the spinner before snapshotting"));
        assert!(md.contains("Final page: Login — `http://localhost:3000/login`"));
    }
}
