//! Deterministic handoff briefing (Phase 1: no LLM).
//!
//! Built purely from stored activity metadata — session boundaries, tool
//! categories, file paths touched, outcomes, token counts. Sections with no
//! data are omitted rather than invented; everything is marked with its
//! recency window so a reader can judge staleness. Output targets the
//! 300–500 token budget (~1.9k chars); [`build`] returns `None` when there
//! is nothing useful to say so callers inject nothing.

use crate::record::MemoryEvent;

/// ~4 chars/token → keep the block under ~475 tokens.
const MAX_CHARS: usize = 1900;
/// Recency labels: the query window is 30d; "hot" files use 7d.
const HOT_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Build the briefing text from recent events (any order). `None` when the
/// project has no activity in the window.
pub fn build(events: &[MemoryEvent]) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    let now = events.iter().map(|e| e.occurred_at_ms).max().unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    lines.push(
        "Komet project memory — auto-generated from local agent activity (last 30 days):".into(),
    );

    session_lines(events, now, &mut lines);
    work_lines(events, now, &mut lines);
    failure_lines(events, now, &mut lines);

    if lines.len() <= 1 {
        return None;
    }
    let mut out = lines.join("\n");
    if out.chars().count() > MAX_CHARS {
        // Truncate on a line boundary, then mark the cut.
        let mut cut = String::new();
        for line in out.lines() {
            if cut.len() + line.len() + 1 > MAX_CHARS - 20 {
                break;
            }
            cut.push_str(line);
            cut.push('\n');
        }
        cut.push_str("… (truncated)");
        out = cut;
    }
    Some(out)
}

fn session_lines(events: &[MemoryEvent], now: i64, out: &mut Vec<String>) {
    let starts: Vec<&MemoryEvent> = events
        .iter()
        .filter(|e| e.kind == "session_start")
        .collect();
    let ends: Vec<&MemoryEvent> = events.iter().filter(|e| e.kind == "session_end").collect();
    if starts.is_empty() && ends.is_empty() {
        return;
    }
    let latest_start = starts.iter().max_by_key(|e| e.occurred_at_ms);
    let latest_end = ends.iter().max_by_key(|e| e.occurred_at_ms);
    let mut line = String::from("Sessions: ");
    line.push_str(&format!("{} run(s)", starts.len()));
    if let Some(s) = latest_start {
        let agent = &s.agent;
        let model = s.model.as_deref().unwrap_or("default model");
        line.push_str(&format!(
            "; last {} / {} ({})",
            agent,
            model,
            fmt_date(s.occurred_at_ms, now)
        ));
    }
    if let Some(e) = latest_end {
        let status = e.outcome.as_deref().unwrap_or("unknown");
        let dur = e
            .duration_ms
            .map(fmt_duration)
            .unwrap_or_else(|| "?".into());
        line.push_str(&format!(
            "; last run {} ({}, {})",
            status,
            dur,
            fmt_date(e.occurred_at_ms, now)
        ));
    }
    out.push(line);
}

fn work_lines(events: &[MemoryEvent], now: i64, out: &mut Vec<String>) {
    use std::collections::HashMap;
    let mut tool_counts: HashMap<&str, usize> = HashMap::new();
    let mut file_ops = 0usize;
    let mut path_counts: HashMap<&str, usize> = HashMap::new();
    let mut chats: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for e in events {
        chats.insert(e.chat_id.as_str());
        if e.kind == "tool_call" {
            if let Some(t) = e.tool_name.as_deref() {
                *tool_counts.entry(t).or_default() += 1;
            }
            if matches!(
                e.tool_name.as_deref(),
                Some("read_file" | "write_file" | "edit_file" | "apply_patch")
            ) {
                file_ops += 1;
            }
        }
        if let Some(p) = e.relative_path.as_deref()
            && now - e.occurred_at_ms <= HOT_WINDOW_MS
        {
            *path_counts.entry(p).or_default() += 1;
        }
    }
    if tool_counts.is_empty() && chats.is_empty() {
        return;
    }
    let mut line = format!(
        "Work: {} session(s), {} tool call(s), {} file operation(s)",
        chats.len(),
        tool_counts.values().sum::<usize>(),
        file_ops
    );
    let mut top: Vec<(&&str, &usize)> = tool_counts.iter().collect();
    top.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    let names: Vec<String> = top
        .iter()
        .take(6)
        .map(|(t, n)| format!("{t}×{n}"))
        .collect();
    if !names.is_empty() {
        line.push_str(&format!(" — {}", names.join(", ")));
    }
    out.push(line);
    if !path_counts.is_empty() {
        let mut paths: Vec<(&&str, &usize)> = path_counts.iter().collect();
        paths.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        let hot: Vec<String> = paths
            .iter()
            .take(8)
            .map(|(p, n)| format!("{p} ({n})"))
            .collect();
        out.push(format!("Hot files (7d): {}", hot.join(", ")));
    }
}

fn failure_lines(events: &[MemoryEvent], now: i64, out: &mut Vec<String>) {
    let mut failures: Vec<&MemoryEvent> = events
        .iter()
        .filter(|e| {
            (e.kind == "tool_result" && e.outcome.as_deref() == Some("error"))
                || (e.kind == "session_end"
                    && matches!(e.outcome.as_deref(), Some("errored" | "interrupted")))
        })
        .collect();
    if failures.is_empty() {
        return;
    }
    failures.sort_by_key(|e| std::cmp::Reverse(e.occurred_at_ms));
    let items: Vec<String> = failures
        .iter()
        .take(5)
        .map(|e| {
            let what = match e.kind.as_str() {
                "tool_result" => format!(
                    "{} on {}",
                    e.tool_name.as_deref().unwrap_or("tool"),
                    e.relative_path.as_deref().unwrap_or("?")
                ),
                _ => format!("run {}", e.outcome.as_deref().unwrap_or("failed")),
            };
            format!("{what} ({})", fmt_date(e.occurred_at_ms, now))
        })
        .collect();
    out.push(format!("Failures: {}", items.join("; ")));
}

fn fmt_date(ms: i64, now: i64) -> String {
    let days = ((now - ms).max(0)) / 86_400_000;
    if days == 0 {
        "today".into()
    } else if days == 1 {
        "yesterday".into()
    } else {
        format!("{days}d ago")
    }
}

fn fmt_duration(ms: i64) -> String {
    if ms <= 0 {
        return "?".into();
    }
    let mins = ms / 60_000;
    if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{}s", ms / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::MemoryEvent;

    fn ev(kind: &str, seq: i64, extra: impl Fn(&mut MemoryEvent)) -> MemoryEvent {
        let mut e = MemoryEvent {
            id: seq,
            project_id: "p_x".into(),
            chat_id: "c1".into(),
            source_seq: seq as u64,
            agent: "claude-code".into(),
            model: Some("opus".into()),
            kind: kind.into(),
            occurred_at_ms: 1_000_000,
            tool_name: None,
            relative_path: None,
            outcome: None,
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            duration_ms: None,
        };
        extra(&mut e);
        e
    }

    #[test]
    fn empty_input_yields_none() {
        assert!(build(&[]).is_none());
    }

    #[test]
    fn briefing_is_bounded_and_marks_recency() {
        let now = 10_000_000i64;
        let mut events = Vec::new();
        for i in 0..50 {
            let mut e = ev("tool_call", i, |_| {});
            e.occurred_at_ms = now - i * 1000;
            e.tool_name = Some("write_file".into());
            e.relative_path = Some(format!("src/f{i}.rs"));
            events.push(e);
        }
        events.push(ev("session_start", 100, |e| {
            e.occurred_at_ms = now - 5000;
        }));
        events.push(ev("session_end", 101, |e| {
            e.occurred_at_ms = now - 1000;
            e.outcome = Some("completed".into());
            e.duration_ms = Some(3_600_000);
        }));
        let text = build(&events).expect("briefing");
        assert!(text.chars().count() <= MAX_CHARS + 40);
        assert!(text.contains("Sessions:"));
        assert!(text.contains("Hot files"));
        assert!(text.contains("1h 0m"));
    }

    #[test]
    fn failures_surface_with_tool_and_path() {
        let now = 10_000_000i64;
        let mut e = ev("tool_result", 1, |_| {});
        e.occurred_at_ms = now;
        e.outcome = Some("error".into());
        e.tool_name = Some("exec".into());
        e.relative_path = Some("build.sh".into());
        let text = build(&[e]).unwrap();
        assert!(text.contains("Failures:"));
        assert!(text.contains("exec on build.sh"));
    }
}
