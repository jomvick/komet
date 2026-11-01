//! The read-only « Shell agent » surface: the commands the agent ran in its
//! OWN shell.
//!
//! WHY this exists: Claude Code and Codex execute their commands inside the
//! CLI's own pty — never in a Komet terminal — so a human watching a run had no
//! window on what the agent was typing and printing. This panel is that window.
//!
//! It is a window, not a door. The security contract of this module:
//!
//! * No input of any kind. There is no `Input`, no key binding, no clickable
//!   affordance, and no path from here to a PTY: writing to an agent's shell
//!   stays impossible from this surface (unlike the embedded terminal, which is
//!   a real Komet PTY the user owns).
//! * Its data is what the harness already reported about its own tool calls —
//!   the doc's `ToolCall::Exec` chips plus the ephemeral `WatchActivity` output
//!   tail. Nothing is scraped off a terminal emulator, and nothing here is ever
//!   written to the doc or journaled.
//! * It says what is NOT carried rather than inventing it: the wording of an
//!   empty output block is driven by the harness's declared
//!   [`komet_proto::ProgressSurface`] (`capabilities::interaction_surface`), so
//!   an agent that streams nothing reads as "not carried", never as "(no
//!   output)" the agent never claimed.
//!
//! Orthogonality note for the security audit: this panel has nothing to do with
//! agent access to Komet terminals (agents may read them, never write). A human
//! reading their own agent's transcript is not a permission decision and grants
//! nothing.

use gpui::{AnyElement, SharedString, div, prelude::*, px};
use komet_proto::ProgressSurface;

use crate::theme::Theme;
use crate::transcript::{AgentShellEntry, AgentShellStatus};

/// One paint-ready command block. Pure derivation from an
/// [`AgentShellEntry`], so the wording rules are testable without gpui.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellRow {
    /// `$ <command>` — the command line exactly as the agent ran it.
    pub head: SharedString,
    pub status: AgentShellStatus,
    /// The agent that ran it, for the row's identity tag.
    pub agent: SharedString,
    pub lines: Vec<SharedString>,
    pub truncated_by: usize,
    /// What the panel says when there is no output to show — the harness's
    /// declared surface decides the wording. `None` when `lines` carry content.
    pub note: Option<SharedString>,
}

/// The honest empty-output wording for one state of one declared surface.
fn no_output_note(status: AgentShellStatus, surface: ProgressSurface) -> &'static str {
    match (status, surface) {
        (AgentShellStatus::Running, ProgressSurface::Streaming) => {
            "waiting for the command's output…"
        }
        (AgentShellStatus::Running, _) => "this agent reports nothing while the command runs",
        (_, ProgressSurface::Streaming) => "the command printed nothing",
        (_, ProgressSurface::Final) => {
            "no print carried: this agent settles a command's status, not its output"
        }
        (_, ProgressSurface::None) => "no output transmitted by this agent",
    }
}

/// Build the panel's rows, in transcript order.
pub fn shell_rows(
    log: &[AgentShellEntry],
    agent_label: &str,
    surface: ProgressSurface,
) -> Vec<ShellRow> {
    log.iter()
        .map(|entry| ShellRow {
            head: SharedString::from(format!("$ {}", entry.command)),
            status: entry.status,
            agent: SharedString::from(agent_label),
            lines: entry.output.clone(),
            truncated_by: entry.truncated_by,
            note: if entry.output.is_empty() {
                Some(SharedString::from(no_output_note(entry.status, surface)))
            } else {
                None
            },
        })
        .collect()
}

fn status_color(status: AgentShellStatus, theme: &Theme) -> gpui::Hsla {
    match status {
        AgentShellStatus::Running => theme.accent,
        AgentShellStatus::Done => theme.success,
        AgentShellStatus::Failed => theme.danger,
    }
}

/// The whole surface. Rendered from the caller's snapshot on every paint: the
/// panel owns no state and subscribes to nothing, which is what keeps it a
/// read-only view of the transcript rather than a second live consumer.
pub fn render(
    log: &[AgentShellEntry],
    agent_label: &str,
    surface: ProgressSurface,
    theme: &Theme,
) -> AnyElement {
    let rows = shell_rows(log, agent_label, surface);
    let header = div()
        .flex_none()
        .px(px(12.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(format!(
                    "{agent_label} · read-only — the agent runs these in its own shell, not in a Komet terminal"
                ))),
        );
    let body = div()
        .id("agent-shell-scroll")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .px(px(10.0))
        .py(px(8.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .when(rows.is_empty(), |pane| {
            pane.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No shell command from this agent yet.")),
            )
        })
        .children(rows.into_iter().map(|row| {
            div()
                .w_full()
                .min_w_0()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(crate::theme::ink(0.02))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(10.0))
                        .py(px(6.0))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .child(row.head),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(status_color(row.status, theme))
                                .child(SharedString::from(row.status.label())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child(row.agent),
                        ),
                )
                .when_some(row.note, |card, note| {
                    card.child(
                        div()
                            .px(px(10.0))
                            .pb(px(6.0))
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(note),
                    )
                })
                .when(!row.lines.is_empty(), |card| {
                    card.child(
                        div()
                            .w_full()
                            .min_w_0()
                            .border_t_1()
                            .border_color(theme.border)
                            .py(px(4.0))
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.5))
                            .children(row.lines.into_iter().map(|line| {
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .px(px(10.0))
                                    .truncate()
                                    .text_color(theme.text.opacity(0.85))
                                    .child(line)
                            }))
                            .when(row.truncated_by > 0, |block| {
                                block.child(div().px(px(10.0)).text_color(theme.text_faint).child(
                                    SharedString::from(format!(
                                        "(+{} lines not shown)",
                                        row.truncated_by
                                    )),
                                ))
                            }),
                    )
                })
        }));
    div()
        .size_full()
        .flex()
        .flex_col()
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .text_color(theme.text)
        .child(header)
        .child(body)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::SharedString;
    use komet_proto::ProgressSurface;

    use super::{no_output_note, shell_rows};
    use crate::transcript::{AgentShellEntry, AgentShellStatus};

    fn entry(
        id: &str,
        command: &str,
        status: AgentShellStatus,
        output: &[&str],
        output_live: bool,
    ) -> AgentShellEntry {
        AgentShellEntry {
            id: SharedString::from(id),
            command: SharedString::from(command),
            status,
            output: output.iter().map(|l| SharedString::from(*l)).collect(),
            truncated_by: 0,
            output_live,
        }
    }

    #[test]
    fn rows_head_the_command_and_tag_the_agent() {
        let rows = shell_rows(
            &[entry(
                "c1",
                "cargo test",
                AgentShellStatus::Done,
                &["ok"],
                true,
            )],
            "Codex",
            ProgressSurface::Streaming,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].head.to_string(), "$ cargo test");
        assert_eq!(rows[0].agent.to_string(), "Codex");
        assert_eq!(rows[0].status, AgentShellStatus::Done);
        assert_eq!(
            rows[0]
                .lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>(),
            vec!["ok"]
        );
        // Content present: no explanatory note is printed over it.
        assert_eq!(rows[0].note, None);
    }

    #[test]
    fn an_empty_output_reads_as_the_harness_declared_it_never_as_a_blank() {
        // The declared surface decides the wording: an agent that streams live
        // output and showed none really printed nothing, while one that carries
        // no print at all must say so — the panel never implies the agent was
        // silent when Komet simply never received the text.
        assert!(
            no_output_note(AgentShellStatus::Done, ProgressSurface::Streaming)
                .contains("printed nothing")
        );
        assert!(
            no_output_note(AgentShellStatus::Done, ProgressSurface::Final)
                .contains("no print carried")
        );
        assert!(
            no_output_note(AgentShellStatus::Done, ProgressSurface::None)
                .contains("no output transmitted")
        );
        assert!(
            no_output_note(AgentShellStatus::Running, ProgressSurface::Streaming)
                .contains("waiting")
        );
        // A command that settled with nothing to show gets the same note
        // whether it ended clean or failed: the panel says what is missing,
        // not why.
        assert_eq!(
            no_output_note(AgentShellStatus::Done, ProgressSurface::None),
            no_output_note(AgentShellStatus::Failed, ProgressSurface::None)
        );
        // …while a still-running command says so, since output may follow.
        assert_ne!(
            no_output_note(AgentShellStatus::Running, ProgressSurface::None),
            no_output_note(AgentShellStatus::Done, ProgressSurface::None)
        );

        let rows = shell_rows(
            &[entry("c1", "ls", AgentShellStatus::Done, &[], false)],
            "Claude Code",
            ProgressSurface::Final,
        );
        assert_eq!(
            rows[0].note.as_ref().map(|n| n.to_string()),
            Some(no_output_note(AgentShellStatus::Done, ProgressSurface::Final).to_string())
        );
    }
}
