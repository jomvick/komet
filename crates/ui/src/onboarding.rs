//! First-run onboarding: Welcome → Agents → Appearance → Permissions → Done.
//!
//! The tour mounts instead of the ready page for as long as it is alive, then
//! persists `UiSettings::onboarding_completed` and hands the window back to the
//! shell. Sync deliberately stays out of the flow — Komet is local-first and
//! enables sync from Settings (the gate's "Cancel sync setup" keeps it opt-in),
//! so the first launch never asks for an account.
//!
//! `KOMET_ONBOARDING=<step>` replays the tour from that step regardless of the
//! saved flag (`welcome|agents|appearance|permissions|done`) — the demo script
//! and styling passes use it.

use gpui::{
    AnyElement, App, Context, Empty, Entity, EventEmitter, IntoElement, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use komet_engine::registry::HarnessDescriptor;
use komet_proto::{HarnessId, SandboxLevel};

use crate::appearance;
use crate::settings::appearance::AppearancePage;
use crate::settings::providers::ProvidersPage;
use crate::settings::security::{SecurityDefaults, SecurityEvent, SecurityPage, level_title};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// One page of the tour. Transitions are pure so the order is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Agents,
    Appearance,
    Permissions,
    Done,
}

impl Step {
    /// Total pages, for the progress indicator.
    pub fn total(self) -> usize {
        5
    }

    /// 1-based position in the tour.
    pub fn number(self) -> usize {
        match self {
            Step::Welcome => 1,
            Step::Agents => 2,
            Step::Appearance => 3,
            Step::Permissions => 4,
            Step::Done => 5,
        }
    }

    /// Next page; `None` on the last one.
    pub fn next(self) -> Option<Step> {
        match self {
            Step::Welcome => Some(Step::Agents),
            Step::Agents => Some(Step::Appearance),
            Step::Appearance => Some(Step::Permissions),
            Step::Permissions => Some(Step::Done),
            Step::Done => None,
        }
    }

    /// Previous page; `None` on the first one.
    pub fn previous(self) -> Option<Step> {
        match self {
            Step::Welcome => None,
            Step::Agents => Some(Step::Welcome),
            Step::Appearance => Some(Step::Agents),
            Step::Permissions => Some(Step::Appearance),
            Step::Done => Some(Step::Permissions),
        }
    }

    /// `KOMET_ONBOARDING` value → step. Accepts the page name and its number.
    pub fn parse(value: &str) -> Option<Step> {
        match value.trim().to_ascii_lowercase().as_str() {
            "welcome" | "1" => Some(Step::Welcome),
            "agents" | "2" => Some(Step::Agents),
            "appearance" | "3" => Some(Step::Appearance),
            "permissions" | "security" | "4" => Some(Step::Permissions),
            "done" | "5" => Some(Step::Done),
            _ => None,
        }
    }
}

/// What the Agents step reports from the REAL catalog (never the placeholder
/// rows, which all claim `installed: true`). Mock is not an agent a user can
/// connect to, so it never counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentsSummary {
    pub installed: usize,
    pub total: usize,
}

pub fn agents_summary(descriptors: &[HarnessDescriptor]) -> AgentsSummary {
    let mut summary = AgentsSummary::default();
    for descriptor in descriptors
        .iter()
        .filter(|descriptor| descriptor.id != HarnessId::Mock)
    {
        summary.total += 1;
        summary.installed += usize::from(descriptor.installed);
    }
    summary
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingEvent {
    /// The tour is over. `add_project` opens the folder picker on the chat
    /// canvas the shell returns to.
    Finish { add_project: bool },
}

pub struct Onboarding {
    state: Entity<AppState>,
    data_dir: std::path::PathBuf,
    step: Step,
    /// Created eagerly: the engine takes up to ~37s to answer ListHarnesses on
    /// a cold boot, so detection starts while the user reads the Welcome page.
    /// Held for the whole tour — recreating it would kill an in-flight login
    /// poll (the browser-wait lives on this entity).
    agents_page: Option<Entity<ProvidersPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    permissions_page: Option<Entity<SecurityPage>>,
    _security_sub: Option<Subscription>,
    /// The sandbox level chosen during the tour, mirrored for the Done summary.
    sandbox: SandboxLevel,
}

impl EventEmitter<OnboardingEvent> for Onboarding {}

impl Onboarding {
    pub fn new(
        state: Entity<AppState>,
        data_dir: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let agents_page = cx.new(|cx| ProvidersPage::new_embedded(state.clone(), cx));
        let sandbox = SecurityDefaults::load(&data_dir).default_sandbox;
        let step = std::env::var("KOMET_ONBOARDING")
            .ok()
            .and_then(|value| Step::parse(&value))
            .unwrap_or(Step::Welcome);
        Self {
            state,
            data_dir,
            step,
            agents_page: Some(agents_page),
            appearance_page: None,
            permissions_page: None,
            _security_sub: None,
            sandbox,
        }
    }

    fn advance(&mut self, cx: &mut Context<Self>) {
        let Some(next) = self.step.next() else {
            self.finish(false, cx);
            return;
        };
        self.step = next;
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(previous) = self.step.previous() {
            self.step = previous;
            cx.notify();
        }
    }

    fn finish(&mut self, add_project: bool, cx: &mut Context<Self>) {
        cx.emit(OnboardingEvent::Finish { add_project });
    }

    /// Whether the current page lets the user continue. Agents is the only
    /// page that can block: with no agent installed there is nothing to talk
    /// to, so "Continue" is off and a "Later" escape hatch appears instead.
    fn can_advance(&self, cx: &App) -> bool {
        if self.step != Step::Agents {
            return true;
        }
        let Some(page) = self.agents_page.as_ref() else {
            return false;
        };
        page.read(cx)
            .harnesses()
            .ready()
            .is_some_and(|list| agents_summary(list).installed > 0)
    }

    /// The one-line status under the Agents heading. The engine attaches in
    /// the background on a cold boot, so "Looking" and "Starting" are normal,
    /// not failures.
    fn agents_note(&self, cx: &App) -> (SharedString, bool) {
        let loadable = self
            .agents_page
            .as_ref()
            .map(|page| page.read(cx).harnesses());
        match loadable {
            Some(crate::popover::Loadable::Ready(list)) => {
                let summary = agents_summary(list);
                if summary.installed > 0 {
                    (
                        format!(
                            "{} of {} agents installed — sign in to any one to continue.",
                            summary.installed, summary.total
                        )
                        .into(),
                        false,
                    )
                } else {
                    (
                        "No agent CLI found. Install one, then retry from Settings, or continue later."
                            .into(),
                        false,
                    )
                }
            }
            Some(crate::popover::Loadable::Error(_)) if self.state.read(cx).engine().is_none() => {
                ("Starting the engine…".into(), false)
            }
            Some(crate::popover::Loadable::Error(message)) => (message.clone().into(), true),
            _ => ("Looking for agents…".into(), false),
        }
    }

    fn render_body(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        match self.step {
            Step::Welcome => div()
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .pt(px(48.0))
                .child(crate::blobatar::blobatar(
                    "onboarding-logo",
                    48.0,
                    Some(theme.text),
                ))
                .child(
                    div()
                        .mt(px(24.0))
                        .text_size(px(24.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(SharedString::from("Welcome to Komet")),
                )
                .child(
                    div()
                        .mt(px(10.0))
                        .max_w(px(480.0))
                        .text_size(px(14.0))
                        .line_height(px(22.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "One window for every coding agent on this machine. Your work stays local — sync and accounts are optional and live in Settings.",
                        )),
                )
                .into_any_element(),
            Step::Agents => {
                if self.agents_page.is_none() {
                    self.agents_page =
                        Some(cx.new(|cx| ProvidersPage::new_embedded(self.state.clone(), cx)));
                }
                let (note, failed) = self.agents_note(cx);
                let page = self.agents_page.clone();
                div()
                    .flex()
                    .flex_col()
                    .child(widgets::page_header(theme, "Connect your agents", None))
                    .child(widgets::page_subtitle(
                        theme,
                        "Komet drives the agent CLIs already installed here.",
                    ))
                    .child(
                        div()
                            .mt(px(10.0))
                            .text_size(px(12.5))
                            .line_height(px(18.0))
                            .text_color(if failed {
                                theme.danger_muted
                            } else {
                                theme.text_muted
                            })
                            .child(note),
                    )
                    .child(div().mt(px(20.0)).children(page))
                    .into_any_element()
            }
            Step::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new_embedded));
                }
                div()
                    .flex()
                    .flex_col()
                    .child(widgets::page_header(theme, "Appearance", None))
                    .child(widgets::page_subtitle(
                        theme,
                        "Choose a theme for this device.",
                    ))
                    .child(div().mt(px(20.0)).children(self.appearance_page.clone()))
                    .into_any_element()
            }
            Step::Permissions => {
                if self.permissions_page.is_none() {
                    let level = self.sandbox;
                    let page = cx.new(|cx| SecurityPage::new_embedded(level, cx));
                    self._security_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Onboarding, _, event: &SecurityEvent, cx| {
                            let SecurityEvent::DefaultSandboxChanged(level) = *event;
                            this.sandbox = level;
                            if let Err(err) = (SecurityDefaults {
                                default_sandbox: level,
                            })
                            .save(&this.data_dir)
                            {
                                tracing::warn!(error = %err, "security-defaults save failed");
                            }
                            this.state
                                .update(cx, |state, _| state.set_new_chat_access(level));
                            cx.notify();
                        },
                    ));
                    self.permissions_page = Some(page);
                }
                div()
                    .flex()
                    .flex_col()
                    .child(widgets::page_header(theme, "Permissions", None))
                    .child(widgets::page_subtitle(
                        theme,
                        "Default agent access for new chats. The composer can still override it per chat.",
                    ))
                    .child(div().mt(px(20.0)).children(self.permissions_page.clone()))
                    .into_any_element()
            }
            Step::Done => {
                let agents = self.agents_page.as_ref().and_then(|page| {
                    page.read(cx).harnesses().ready().map(|list| agents_summary(list))
                });
                let agents_label: SharedString = match agents {
                    Some(summary) if summary.installed > 0 => {
                        format!("{} installed", summary.installed).into()
                    }
                    Some(_) => "None detected".into(),
                    None => "Not detected".into(),
                };
                let appearance = appearance::mode(cx).label().to_string();
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(24.0))
                    .child(crate::blobatar::blobatar(
                        "onboarding-done",
                        40.0,
                        Some(theme.text),
                    ))
                    .child(
                        div()
                            .mt(px(20.0))
                            .text_size(px(22.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from("You're all set")),
                    )
                    .child(
                        div()
                            .mt(px(8.0))
                            .text_size(px(13.0))
                            .line_height(px(20.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "Everything here can be changed later from Settings.",
                            )),
                    )
                    .child(
                        div()
                            .mt(px(28.0))
                            .w_full()
                            .max_w(px(420.0))
                            .flex()
                            .flex_col()
                            .child(summary_row(theme, "Agents", agents_label))
                            .child(summary_row(
                                theme,
                                "Appearance",
                                SharedString::from(appearance),
                            ))
                            .child(summary_row(
                                theme,
                                "Permissions",
                                SharedString::from(level_title(self.sandbox)),
                            )),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_footer(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let step = self.step;
        let can_advance = self.can_advance(cx);
        let mut actions = div().flex().flex_row().items_center().gap(px(10.0));
        if step == Step::Agents && !can_advance {
            actions = actions.child(
                widgets::ghost_action(theme)
                    .id("onboarding-later")
                    .text_size(px(13.0))
                    .hover(|s| widgets::ghost_hover(theme, s))
                    .on_click(cx.listener(|this, _, _, cx| this.advance(cx)))
                    .child(SharedString::from("Later")),
            );
        }
        let primary = if step == Step::Done {
            primary_button(theme, "onboarding-start", "Start using Komet")
                .on_click(cx.listener(|this, _, _, cx| this.finish(false, cx)))
        } else {
            let button = primary_button(
                theme,
                "onboarding-continue",
                if step == Step::Welcome {
                    "Get started"
                } else {
                    "Continue"
                },
            );
            if can_advance {
                button.on_click(cx.listener(|this, _, _, cx| this.advance(cx)))
            } else {
                button.opacity(0.4)
            }
        };
        actions = actions.child(primary);

        let back = match step.previous() {
            Some(_) => widgets::ghost_action(theme)
                .id("onboarding-back")
                .text_size(px(13.0))
                .hover(|s| widgets::ghost_hover(theme, s))
                .on_click(cx.listener(|this, _, _, cx| this.back(cx)))
                .child(SharedString::from("Back"))
                .into_any_element(),
            None => Empty.into_any_element(),
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .when(step == Step::Done, |el| {
                el.child(
                    widgets::ghost_action(theme)
                        .id("onboarding-add-project")
                        .text_size(px(13.0))
                        .hover(|s| widgets::ghost_hover(theme, s))
                        .on_click(cx.listener(|this, _, _, cx| this.finish(true, cx)))
                        .child(SharedString::from("Add a project")),
                )
            })
            .when(step != Step::Done, |el| el.child(back))
            .child(actions)
            .into_any_element()
    }
}

impl Render for Onboarding {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = self.render_body(&theme, cx);
        let footer = self.render_footer(&theme, cx);
        div()
            .id("onboarding")
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.bg)
            .text_color(theme.text)
            .child(crate::shell::gate::grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(div().mt(px(36.0)).child(progress(&theme, self.step)))
                    .child(
                        div()
                            .id("onboarding-body")
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .max_w(px(720.0))
                            .px(px(24.0))
                            .py(px(24.0))
                            .overflow_y_scroll()
                            .child(body),
                    )
                    .child(
                        div()
                            .w_full()
                            .max_w(px(720.0))
                            .px(px(24.0))
                            .pb(px(32.0))
                            .pt(px(12.0))
                            .child(footer),
                    ),
            )
    }
}

/// Progress bars + "n / total" — the tour's fixed header, so the column never
/// jumps between pages.
fn progress(theme: &Theme, step: Step) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .children((1..=step.total()).map(|n| {
            div()
                .h(px(3.0))
                .w(px(28.0))
                .rounded(px(2.0))
                .bg(if n <= step.number() {
                    theme.text
                } else {
                    theme.border
                })
        }))
        .child(
            div()
                .ml(px(10.0))
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(format!(
                    "{} / {}",
                    step.number(),
                    step.total()
                ))),
        )
}

fn summary_row(theme: &Theme, label: &str, value: SharedString) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .py(px(10.0))
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .text_size(px(13.0))
                .text_color(theme.text)
                .child(value),
        )
}

/// Solid action button shared by first-run setup surfaces.
fn primary_button(
    theme: &Theme,
    id: &'static str,
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(36.0))
        .px(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .bg(theme.text)
        .text_size(px(13.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.on_solid)
        .cursor_pointer()
        .hover(|s| s.opacity(0.9))
        .child(SharedString::from(label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::SteeringMode;

    fn descriptor(id: HarnessId, installed: bool) -> HarnessDescriptor {
        HarnessDescriptor {
            id,
            name: format!("{id:?}"),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: Vec::new(),
            installed,
            enabled: None,
            surface: Default::default(),
        }
    }

    #[test]
    fn step_walks_forward_and_back() {
        let mut step = Step::Welcome;
        let mut forward = Vec::new();
        while let Some(next) = step.next() {
            forward.push(next);
            step = next;
        }
        assert_eq!(
            forward,
            vec![
                Step::Agents,
                Step::Appearance,
                Step::Permissions,
                Step::Done
            ]
        );
        assert!(Step::Welcome.previous().is_none());
        assert!(Step::Done.next().is_none());
        // And back down the same chain.
        let mut step = Step::Done;
        while let Some(previous) = step.previous() {
            step = previous;
        }
        assert_eq!(step, Step::Welcome);
    }

    #[test]
    fn step_numbers_are_one_based_and_match_the_order() {
        for (ix, step) in [
            Step::Welcome,
            Step::Agents,
            Step::Appearance,
            Step::Permissions,
            Step::Done,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(step.number(), ix + 1);
            assert_eq!(step.total(), 5);
        }
    }

    #[test]
    fn step_parses_names_and_numbers() {
        assert_eq!(Step::parse("welcome"), Some(Step::Welcome));
        assert_eq!(Step::parse(" 2 "), Some(Step::Agents));
        assert_eq!(Step::parse("permissions"), Some(Step::Permissions));
        assert_eq!(Step::parse("security"), Some(Step::Permissions));
        assert_eq!(Step::parse("done"), Some(Step::Done));
        assert_eq!(Step::parse("nope"), None);
    }

    #[test]
    fn agents_summary_ignores_mock_and_counts_installed() {
        let summary = agents_summary(&[
            descriptor(HarnessId::ClaudeCode, true),
            descriptor(HarnessId::Codex, false),
            descriptor(HarnessId::Mock, true),
        ]);
        assert_eq!(
            summary,
            AgentsSummary {
                installed: 1,
                total: 2
            }
        );
    }

    #[test]
    fn agents_summary_is_empty_without_real_agents() {
        assert_eq!(
            agents_summary(&[descriptor(HarnessId::Mock, true)]),
            AgentsSummary::default()
        );
    }
}
