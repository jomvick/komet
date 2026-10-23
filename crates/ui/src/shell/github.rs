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
    /// Returns the trigger with its exact width so the pane strip can budget it.
    pub(super) fn render_github_button(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<(AnyElement, f32)> {
        let Some(key) = self.github_target(cx) else {
            self.github.key = None;
            self.github.status = None;
            self.github.task = None;
            return None;
        };
        self.ensure_github_status(key, false, cx);
        let status = self.github.status.clone()?;

        let pr_badge = status
            .pull_request
            .as_ref()
            .map(|pr| (format!("#{}", pr.number), pr_state_color(pr, theme)));
        let width = trigger_width(pr_badge.as_ref().map(|(label, _)| label.as_str()));
        let fade = "github-menu-trigger-fade";
        let open = self.github_menu.get().is_some();
        let mut trigger = div()
            .id("github-menu-trigger")
            .h(px(28.0))
            .w(px(width))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(5.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if open {
                crate::theme::hairline(0.18)
            } else {
                crate::theme::hairline(0.10)
            })
            .cursor_pointer()
            .bg(if open {
                crate::theme::wash(0.12)
            } else {
                motion::hover_blend(fade, crate::theme::wash(0.03), crate::theme::wash(0.11))
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
                    .text_color(theme.text.opacity(0.9)),
            )
            .when_some(pr_badge, |el, (label, color)| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(div().size(px(6.0)).rounded_full().bg(color))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text.opacity(0.85))
                                .child(SharedString::from(label)),
                        ),
                )
            })
            .child(chevron(open, theme));
        if open {
            let closing = self.github_menu.closing_since();
            let menu = self.render_github_menu(&status, theme, cx);
            // Right-aligned to the trigger: the card grows LEFT over the
            // conversation instead of covering the right pane.
            trigger = trigger.relative().child(popover::anchored_menu_below_end(
                "github-menu",
                menu,
                closing,
                8.0,
            ));
        }
        Some((trigger.into_any_element(), width))
    }

    fn render_github_menu(
        &mut self,
        status: &GitHubStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detached = status.branch == "HEAD";
        // Diff surfaces resolve through the selected chat; on the new-session
        // canvas they would sit on "Preparing" forever, so rows stay info-only.
        let can_open_diffs = self.state.read(cx).selected_chat_row().is_some();
        let repo_name = status
            .repo_url
            .strip_prefix("https://github.com/")
            .unwrap_or(&status.repo_url)
            .to_string();

        // ── Header: repo + branch, external-link to GitHub ──
        let repo_url = status.repo_url.clone();
        let branch_url =
            (!detached).then(|| format!("{}/tree/{}", status.repo_url, encode_ref(&status.branch)));
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(10.0))
            .pt(px(8.0))
            .pb(px(10.0))
            .child(
                div()
                    .size(px(30.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .bg(crate::theme::wash(0.08))
                    .child(icon(icons::GITHUB).size(px(17.0)).text_color(theme.text)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from(repo_name)),
                    )
                    .child(self.branch_chip(status, branch_url, theme, cx)),
            )
            .child(self.icon_link(
                "github-open-repo",
                icons::ARROW_UP_RIGHT,
                repo_url,
                theme,
                cx,
            ));

        let mut body = div().flex().flex_col().child(header).child(divider());

        // ── Pull request ──
        body = body.child(section_label("Pull request", None, theme));
        match &status.pull_request {
            Some(pr) => body = body.child(self.github_pr_card(pr, &status.branch, theme, cx)),
            None if !detached => {
                let url = format!(
                    "{}/compare/{}?expand=1",
                    status.repo_url,
                    encode_ref(&status.branch)
                );
                body = body.child(
                    popover::menu_row(theme, false, "github-create-pr")
                        .id("github-create-pr")
                        .mx(px(4.0))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.open_url(&url);
                            this.close_github_menu(cx);
                        }))
                        .child(
                            div()
                                .size(px(22.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(crate::theme::hairline(0.14))
                                .child(icon(icons::PLUS).size(px(11.0)).text_color(theme.text)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(SharedString::from("Create pull request"))
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(
                                            "No pull request for this branch",
                                        )),
                                ),
                        ),
                );
            }
            None => {
                body = body.child(empty_note(
                    "Detached HEAD — no branch to open a PR from",
                    theme,
                ));
            }
        }

        // ── Recent commits: click pins the commit's diff in the right pane ──
        body = body.child(divider()).child(section_label(
            "Recent commits",
            (!status.recent_commits.is_empty()).then(|| status.recent_commits.len().to_string()),
            theme,
        ));
        if status.recent_commits.is_empty() {
            body = body.child(empty_note("No commits yet", theme));
        }
        let now = chrono::Utc::now();
        for (ix, commit) in status.recent_commits.iter().enumerate() {
            let short: String = commit.sha.chars().take(7).collect();
            let ago = chrono::DateTime::parse_from_rfc3339(&commit.authored_at)
                .ok()
                .map(|t| crate::state::format_time_ago(t.with_timezone(&chrono::Utc), now));
            let commit_for_click = commit.clone();
            body = body.child(
                popover::menu_row(theme, false, format!("github-commit-{ix}"))
                    .id(("github-commit-row", ix))
                    .mx(px(4.0))
                    .py(px(5.0))
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .when(can_open_diffs, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.close_github_menu(cx);
                            this.open_commit_from_github_menu(commit_for_click.clone(), cx);
                        }))
                    })
                    .when(!can_open_diffs, |el| el.cursor_default())
                    .child(
                        div()
                            .flex_none()
                            .px(px(5.0))
                            .rounded(px(4.0))
                            .bg(crate::theme::wash(0.07))
                            .text_size(px(11.0))
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
                    )
                    .when_some(ago, |el, ago| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(ago)),
                        )
                    }),
            );
        }

        // ── Changes: stats + proportion bar; click focuses the Git diff tab ──
        let (ins, del) = (status.insertions, status.deletions);
        body = body.child(divider()).child(
            popover::menu_row(theme, false, "github-changes")
                .id("github-changes-row")
                .mx(px(4.0))
                .mb(px(2.0))
                .when(can_open_diffs, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| {
                        this.close_github_menu(cx);
                        this.open_working_tree_diff(cx);
                    }))
                })
                .when(!can_open_diffs, |el| el.cursor_default())
                .child(
                    icon(icons::FILE_DIFF)
                        .size(px(15.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(5.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(div().flex_1().child(SharedString::from("Changes")))
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(6.0))
                                        .text_size(px(12.0))
                                        .font_family(theme.font_mono.clone())
                                        .child(
                                            div()
                                                .text_color(theme.success)
                                                .child(SharedString::from(format!("+{ins}"))),
                                        )
                                        .child(
                                            div()
                                                .text_color(theme.danger)
                                                .child(SharedString::from(format!("−{del}"))),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .h(px(4.0))
                                .w_full()
                                .flex()
                                .gap(px(2.0))
                                .rounded_full()
                                .overflow_hidden()
                                .bg(crate::theme::wash(0.08))
                                .when(ins > 0, |el| {
                                    el.child(
                                        div()
                                            .h_full()
                                            .flex_grow(ins as f32)
                                            .flex_basis(px(0.0))
                                            .bg(theme.success),
                                    )
                                })
                                .when(del > 0, |el| {
                                    el.child(
                                        div()
                                            .h_full()
                                            .flex_grow(del as f32)
                                            .flex_basis(px(0.0))
                                            .bg(theme.danger),
                                    )
                                }),
                        ),
                )
                .when(can_open_diffs, |el| {
                    el.child(
                        icon(icons::ALT_ARROW_RIGHT)
                            .size(px(12.0))
                            .text_color(theme.text_faint),
                    )
                }),
        );

        popover::popover_card(theme)
            .w(px(MENU_WIDTH))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_github_menu(cx)))
            .child(body)
            .into_any_element()
    }

    /// Branch name chip; clickable (opens the branch on GitHub) when attached.
    fn branch_chip(
        &self,
        status: &GitHubStatus,
        url: Option<String>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = if url.is_some() {
            status.branch.clone()
        } else {
            "Detached HEAD".to_string()
        };
        let fade = "github-branch-chip";
        let chip = div()
            .id("github-branch-chip")
            .max_w_full()
            .flex()
            .items_center()
            .gap(px(4.0))
            .text_size(px(11.5))
            .text_color(motion::hover_blend(fade, theme.text_muted, theme.text))
            .on_hover(motion::hover_listener(fade))
            .child(
                icon(icons::GIT_BRANCH)
                    .size(px(11.0))
                    .text_color(theme.text_muted),
            )
            .child(div().min_w_0().truncate().child(SharedString::from(label)));
        match url {
            Some(url) => chip
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.open_url(&url);
                    this.close_github_menu(cx);
                }))
                .into_any_element(),
            None => chip.into_any_element(),
        }
    }

    fn icon_link(
        &self,
        id: &'static str,
        glyph: &'static str,
        url: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .size(px(26.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                id,
                crate::theme::wash(0.0),
                crate::theme::wash(0.10),
            ))
            .on_hover(motion::hover_listener(id))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.open_url(&url);
                this.close_github_menu(cx);
            }))
            .child(icon(glyph).size(px(13.0)).text_color(theme.text_muted))
    }

    fn github_pr_card(
        &self,
        pr: &GitHubPullRequest,
        branch: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let color = pr_state_color(pr, theme);
        let state = match pr.state.as_str() {
            "MERGED" => "Merged",
            "CLOSED" => "Closed",
            _ if pr.is_draft => "Draft",
            _ => "Open",
        };
        let checks = pr.checks;
        let (checks_color, checks_label) = if checks.failed > 0 {
            (theme.danger, format!("{} failing", checks.failed))
        } else if checks.pending > 0 {
            (theme.warning, format!("{} running", checks.pending))
        } else {
            (theme.success, "All checks passed".to_string())
        };
        let url = pr.url.clone();
        let fade = "github-pr-card";
        div()
            .id("github-pr-card")
            .mx(px(6.0))
            .mb(px(4.0))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(crate::theme::hairline(0.10))
            .cursor_pointer()
            .bg(motion::hover_blend(
                fade,
                crate::theme::wash(0.03),
                crate::theme::wash(0.08),
            ))
            .on_hover(motion::hover_listener(fade))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.open_url(&url);
                this.close_github_menu(cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .px(px(6.0))
                            .py(px(1.0))
                            .rounded_full()
                            .bg(color.opacity(0.16))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(color)
                            .child(icon(icons::PULL_REQUEST).size(px(11.0)).text_color(color))
                            .child(SharedString::from(state)),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("#{}", pr.number))),
                    )
                    .child(div().flex_1())
                    .child(
                        icon(icons::ARROW_UP_RIGHT)
                            .size(px(12.0))
                            .text_color(theme.text_faint),
                    ),
            )
            .child(
                div()
                    .line_clamp(2)
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(SharedString::from(pr.title.clone())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .child(SharedString::from(if pr.base_ref.is_empty() {
                                branch.to_string()
                            } else {
                                format!("{} ← {branch}", pr.base_ref)
                            })),
                    )
                    .when(checks.total() > 0, |el| {
                        el.child(div().flex_1()).child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .child(check_dot(checks_color))
                                .child(SharedString::from(checks_label))
                                .child(div().text_color(theme.text_faint).child(
                                    SharedString::from(format!(
                                        "{}/{}",
                                        checks.passed,
                                        checks.total()
                                    )),
                                )),
                        )
                    }),
            )
    }
}

const MENU_WIDTH: f32 = 340.0;

/// Exact trigger width: icon + chevron, plus the `#N` badge when a PR exists.
fn trigger_width(badge: Option<&str>) -> f32 {
    match badge {
        // dot 6 + gap 4 + ~7px per glyph at 11.5px + gap 5
        Some(label) => GITHUB_BUTTON_WIDTH + 15.0 + 7.0 * label.chars().count() as f32,
        None => GITHUB_BUTTON_WIDTH,
    }
}

fn pr_state_color(pr: &GitHubPullRequest, theme: &Theme) -> gpui::Hsla {
    match pr.state.as_str() {
        // GitHub's merged purple.
        "MERGED" => gpui::hsla(270.0 / 360.0, 0.6, 0.65, 1.0),
        "CLOSED" => theme.danger,
        _ if pr.is_draft => theme.text_muted,
        _ => theme.success,
    }
}

fn divider() -> impl IntoElement {
    div()
        .h(px(1.0))
        .mx(px(8.0))
        .my(px(4.0))
        .bg(crate::theme::hairline(0.08))
}

fn section_label(label: &'static str, count: Option<String>, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(10.0))
        .pt(px(6.0))
        .pb(px(4.0))
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(SharedString::from(label.to_uppercase()))
        .when_some(count, |el, count| {
            el.child(
                div()
                    .px(px(5.0))
                    .rounded_full()
                    .bg(crate::theme::wash(0.08))
                    .child(SharedString::from(count)),
            )
        })
}

fn empty_note(text: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .px(px(12.0))
        .pb(px(6.0))
        .text_size(px(12.0))
        .text_color(theme.text_muted)
        .child(SharedString::from(text))
}

fn chevron(open: bool, theme: &Theme) -> impl IntoElement {
    let angle = if open { std::f32::consts::PI } else { 0.0 };
    icon(icons::ALT_ARROW_DOWN)
        .size(px(11.0))
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
