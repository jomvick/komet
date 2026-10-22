//! Titlebar GitHub control (issue #50): a GitHub mark + chevron left of the
//! right-pane toggle, shown only when the active checkout has a GitHub
//! remote. Its dropdown mirrors t3code's git card — branch, the branch's pull
//! request with its checks, recent commits, and the uncommitted/branch diff
//! stats — and routes clicks to GitHub (browser) or the right pane's Git
//! surfaces.

use komet_proto::{GitHistoryCommit, GitHubPullRequest, GitHubStatus};

use super::*;

/// `(repo_path, target_device_id)` — the checkout the control describes.
type GitHubKey = (String, Option<String>);

/// Button footprint, budgeted by the titlebar strip so the pane header never
/// overflows when the control is visible.
pub(super) const GITHUB_BUTTON_WIDTH: f32 = 44.0;

#[derive(Default)]
pub(super) struct GitHubHeader {
    key: Option<GitHubKey>,
    status: Option<GitHubStatus>,
    task: Option<Task<()>>,
    branch_open: bool,
    commits_open: bool,
}

impl Shell {
    /// Same checkout resolution as the composer footer: the chat's cwd (its
    /// worktree) else the space folder, relayed to the owning device.
    fn github_target(&self, cx: &App) -> Option<GitHubKey> {
        let state = self.state.read(cx);
        let space = state.selected_space_row().filter(|s| s.git_detected)?;
        let chat = state
            .selected_chat
            .as_ref()
            .and_then(|_| state.selected_chat_row());
        let repo_path = chat
            .and_then(|chat| chat.cwd.clone())
            .unwrap_or_else(|| space.path.clone());
        let device = chat
            .map(|chat| chat.device_id.as_str())
            .unwrap_or(space.device_id.as_str());
        let target = (state.local_device_id.as_deref() != Some(device)).then(|| device.to_string());
        Some((repo_path, target))
    }

    /// Fetch the status when the checkout changes (or on `force`, e.g. each
    /// menu open so PR checks are fresh). A retarget clears the old status so
    /// another repo's PR never shows under the new one.
    fn ensure_github_status(&mut self, key: GitHubKey, force: bool, cx: &mut Context<Self>) {
        if !force && self.github.key.as_ref() == Some(&key) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.github.key.as_ref() != Some(&key) {
            self.github.status = None;
            self.github.branch_open = false;
            self.github.commits_open = false;
        }
        self.github.key = Some(key.clone());
        self.github.task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), key.0.clone().into());
            if let Some(device_id) = key.1.clone() {
                params.insert("targetDeviceId".into(), device_id.into());
            }
            let result = engine
                .client()
                .call(
                    methods::GET_GITHUB_STATUS,
                    serde_json::Value::Object(params),
                )
                .await;
            this.update(cx, |shell, cx| {
                if shell.github.key.as_ref() != Some(&key) {
                    return;
                }
                match result.map(serde_json::from_value::<Option<GitHubStatus>>) {
                    Ok(Ok(status)) => shell.github.status = status,
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, "GitHub status response was invalid")
                    }
                    Err(err) => tracing::warn!(error = %err, "GitHub status lookup failed"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn close_github_menu(&mut self, cx: &mut Context<Self>) {
        if self.github_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.github_menu);
        }
        cx.notify();
    }

    fn toggle_github_menu(&mut self, cx: &mut Context<Self>) {
        if self.github_menu.take_press_was_open() || self.github_menu.is_open() {
            self.close_github_menu(cx);
            return;
        }
        self.github_menu.open(());
        if let Some(key) = self.github_target(cx) {
            self.ensure_github_status(key, true, cx);
        }
        cx.notify();
    }

    fn open_right_pane(&mut self, cx: &mut Context<Self>) {
        if !self.right_pane_open(cx) {
            self.toggle_right_pane(cx);
        }
    }

    /// Changes row: focus the working-tree Git tab, opening one if needed.
    fn open_working_tree_diff(&mut self, cx: &mut Context<Self>) {
        self.open_right_pane(cx);
        let key = self.panel_key(cx);
        let existing = self.right_tabs.get(&key).and_then(|tabs| {
            tabs.iter().copied().find(|surface| match surface {
                RightSurface::Diff(id) => self
                    .diffs
                    .get(id)
                    .is_some_and(|changes| changes.read(cx).is_working_tree()),
                _ => false,
            })
        });
        match existing {
            Some(surface) => self.set_right_active(surface, cx),
            None => self.add_diff_surface(cx),
        }
    }

    fn open_commit_from_github_menu(&mut self, commit: GitHistoryCommit, cx: &mut Context<Self>) {
        self.open_right_pane(cx);
        self.add_commit_diff_surface(commit, cx);
    }

    /// The titlebar trigger plus its dropdown; `None` (no slot at all) when
    /// the checkout has no GitHub remote or the lookup hasn't resolved yet.
    pub(super) fn render_github_button(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some(key) = self.github_target(cx) else {
            self.github.key = None;
            self.github.status = None;
            self.github.task = None;
            return None;
        };
        self.ensure_github_status(key, false, cx);
        let status = self.github.status.clone()?;

        let fade = "github-menu-trigger-fade";
        let open = self.github_menu.get().is_some();
        let mut trigger = div()
            .id("github-menu-trigger")
            .h(px(28.0))
            .w(px(GITHUB_BUTTON_WIDTH))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(3.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .bg(if open {
                crate::theme::wash(0.11)
            } else {
                motion::hover_blend(fade, crate::theme::wash(0.0), crate::theme::wash(0.11))
            })
            .on_hover(motion::hover_listener(fade))
            .tab_index(0)
            .aria_label("GitHub: pull request, commits and changes")
            // Inside the titlebar drag region: carve the rect out of it.
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.github_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_github_menu(cx);
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "enter" | "space" | " " => this.toggle_github_menu(cx),
                    "escape" => this.close_github_menu(cx),
                    _ => {}
                }
            }))
            .child(
                icon(icons::GITHUB)
                    .size(px(15.0))
                    .text_color(theme.text.opacity(0.85)),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .text_color(theme.text_muted),
            );
        if open {
            let closing = self.github_menu.closing_since();
            let menu = self.render_github_menu(&status, theme, cx);
            trigger = trigger.relative().child(popover::anchored_menu_below_gap(
                "github-menu",
                menu,
                closing,
                10.0,
            ));
        }
        Some(trigger.into_any_element())
    }

    fn render_github_menu(
        &mut self,
        status: &GitHubStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detached = status.branch == "HEAD";
        let mut rows = div().flex().flex_col().gap(px(2.0));

        // Branch: the chevron reveals the GitHub destinations.
        let branch_open = self.github.branch_open;
        rows = rows.child(
            popover::menu_row(theme, false, "github-branch")
                .id("github-branch-row")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.github.branch_open = !this.github.branch_open;
                    cx.notify();
                }))
                .child(
                    icon(icons::GIT_BRANCH)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(SharedString::from(if detached {
                            "Detached HEAD".to_string()
                        } else {
                            status.branch.clone()
                        })),
                )
                .child(chevron(branch_open, theme)),
        );
        if branch_open {
            let repo_url = status.repo_url.clone();
            rows = rows.child(self.github_link_row(
                "github-open-repo",
                icons::GITHUB,
                "Open repository",
                repo_url,
                theme,
                cx,
            ));
            if !detached {
                let url = format!("{}/tree/{}", status.repo_url, encode_ref(&status.branch));
                rows = rows.child(self.github_link_row(
                    "github-open-branch",
                    icons::ARROW_UP_RIGHT,
                    "Open branch on GitHub",
                    url,
                    theme,
                    cx,
                ));
            }
        }

        // Pull request, or a compare link to open one.
        match &status.pull_request {
            Some(pr) => rows = rows.child(self.github_pr_row(pr, theme, cx)),
            None if !detached => {
                let url = format!(
                    "{}/compare/{}?expand=1",
                    status.repo_url,
                    encode_ref(&status.branch)
                );
                rows = rows.child(self.github_link_row(
                    "github-create-pr",
                    icons::PULL_REQUEST,
                    "Create pull request",
                    url,
                    theme,
                    cx,
                ));
            }
            None => {}
        }

        // Commits: the chevron lists HEAD's latest; a click pins its diff.
        let commits_open = self.github.commits_open;
        rows = rows.child(
            popover::menu_row(theme, false, "github-commits")
                .id("github-commits-row")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.github.commits_open = !this.github.commits_open;
                    cx.notify();
                }))
                .child(
                    icon(icons::GIT_COMMIT)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Commit")),
                )
                .child(chevron(commits_open, theme)),
        );
        if commits_open {
            if status.recent_commits.is_empty() {
                rows = rows.child(
                    div()
                        .px(px(32.0))
                        .py(px(4.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("No commits yet")),
                );
            }
            for (ix, commit) in status.recent_commits.iter().enumerate() {
                let short: String = commit.sha.chars().take(7).collect();
                let commit_for_click = commit.clone();
                rows = rows.child(
                    popover::menu_row(theme, false, format!("github-commit-{ix}"))
                        .id(("github-commit-row", ix))
                        .pl(px(32.0))
                        .text_size(px(12.0))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_github_menu(cx);
                            this.open_commit_from_github_menu(commit_for_click.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.text_muted)
                                .child(SharedString::from(short)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(commit.subject.clone())),
                        ),
                );
            }
        }

        // Changes: diff stats; a click focuses the working-tree Git tab.
        rows = rows.child(
            popover::menu_row(theme, false, "github-changes")
                .id("github-changes-row")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_github_menu(cx);
                    this.open_working_tree_diff(cx);
                }))
                .child(
                    icon(icons::FILE_DIFF)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Changes")),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.0))
                        .text_size(px(12.0))
                        .font_family(theme.font_mono.clone())
                        .child(
                            div()
                                .text_color(theme.success)
                                .child(SharedString::from(format!("+{}", status.insertions))),
                        )
                        .child(
                            div()
                                .text_color(theme.danger)
                                .child(SharedString::from(format!("-{}", status.deletions))),
                        ),
                ),
        );

        popover::popover_card(theme)
            .w(px(272.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_github_menu(cx)))
            .child(rows)
            .into_any_element()
    }

    fn github_link_row(
        &self,
        id: &'static str,
        glyph: &'static str,
        label: &'static str,
        url: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        popover::menu_row(theme, false, id)
            .id(id)
            .pl(px(32.0))
            .text_size(px(12.0))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.open_url(&url);
                this.close_github_menu(cx);
            }))
            .child(icon(glyph).size(px(12.0)).text_color(theme.text_muted))
            .child(SharedString::from(label))
    }

    fn github_pr_row(
        &self,
        pr: &GitHubPullRequest,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pr_color = match pr.state.as_str() {
            "MERGED" => theme.accent,
            "CLOSED" => theme.danger,
            _ if pr.is_draft => theme.text_muted,
            _ => theme.success,
        };
        let checks = pr.checks;
        let checks_color = if checks.failed > 0 {
            theme.danger
        } else if checks.pending > 0 {
            theme.warning
        } else {
            theme.success
        };
        let url = pr.url.clone();
        popover::menu_row(theme, false, "github-pr")
            .id("github-pr-row")
            .items_start()
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.open_url(&url);
                this.close_github_menu(cx);
            }))
            .child(
                div().pt(px(2.0)).child(
                    icon(icons::PULL_REQUEST)
                        .size(px(14.0))
                        .text_color(pr_color),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().truncate().child(SharedString::from(pr.title.clone())))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("#{}", pr.number)))
                            .when(checks.total() > 0, |el| {
                                el.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(4.0))
                                        .child(check_dot(checks_color))
                                        .child(SharedString::from(format!(
                                            "{}/{}",
                                            checks.passed,
                                            checks.total()
                                        ))),
                                )
                            }),
                    ),
            )
    }
}

/// Down when collapsed (the reference), flipped up while expanded.
fn chevron(open: bool, theme: &Theme) -> impl IntoElement {
    let angle = if open { std::f32::consts::PI } else { 0.0 };
    icon(icons::ALT_ARROW_DOWN)
        .size(px(12.0))
        .text_color(theme.text_muted)
        .with_transformation(gpui::Transformation::rotate(gpui::radians(angle)))
}

/// Ring + center dot — t3code's checks-status glyph.
fn check_dot(color: gpui::Hsla) -> impl IntoElement {
    div()
        .size(px(11.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(color)
        .child(div().size(px(4.0)).rounded_full().bg(color))
}

/// Percent-encode the few characters that would break a GitHub ref URL.
fn encode_ref(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    for ch in branch.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            ' ' => out.push_str("%20"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::encode_ref;

    #[test]
    fn branch_refs_encode_url_breaking_characters() {
        assert_eq!(encode_ref("feat/github-link"), "feat/github-link");
        assert_eq!(encode_ref("fix#1 a?b%"), "fix%231%20a%3Fb%25");
    }
}
