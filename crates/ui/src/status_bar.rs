#! StatusBar — passive component that displays the current workspace state
/// as a horizontal bar at the bottom of the main window.
///
/// Reads from the global `AppState` and re-renders on each update.
/// Icons: folder (project), monitor (environment), git-branch (branch).
use gpui::{
    div, prelude::*, px, SharedString, theme::Theme,
};
use komet_proto::WorkspaceState;

/// Renders a single stat item with icon + label.
fn stat_item(
    cx: &mut gpui::Context<Self>,
    icon: &'static str,
    label: SharedString,
) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .child(
            crate::icons::icon(icon)
                .size(px(14.0))
                .text_color(cx.theme().text_muted),
        )
        .child(
            gpui::div()
                .text_size(px(11.0))
                .text_color(cx.theme().text_muted)
                .child(label),
        )
}

//! The root StatusBar div, fixed to the bottom of the window.
pub struct StatusBar {
    /// Cached workspace state — recomputed on each render tick.
    workspace_state: WorkspaceState,
}

impl StatusBar {
    pub fn new(workspace_state: WorkspaceState) -> Self {
        Self { workspace_state }
    }
}

impl gpui::Element for StatusBar {
    type View = gpui::Div;

    fn element(cx: &mut gpui::Context<Self>) -> gpui::View {
        // Subscribe to global state changes via the AppState context.
        // The AppState is stored as a global entity; we read the workspace state
        // from it and re-subscribe when it changes.
        let app_state = cx.global::<crate::state::AppState>();
        let ws = app_state.workspace_state.clone().unwrap_or_else(|| {
            // Default fallback state.
            WorkspaceState {
                project_name: "<project>".into(),
                environment: "Local".into(),
                git_branch: "<branch>".into(),
                access_mode: komet_proto::AccessMode::FullAccess,
            }
        });

        // Update cached state if changed.
        let mut bar = Self { workspace_state: ws };
        bar.update_from_app_state(cx, &app_state);
        bar
    }

    fn update_from_app_state(&mut self, cx: &mut gpui::Context<Self>, app_state: &crate::state::AppState) {
        // Re-read workspace state from app_state — it may have been updated by
        // the engine IPC subscriber.
        if let Some(ws) = &app_state.workspace_state {
            self.workspace_state = ws.clone();
        }
        cx.notify();
    }
}

impl gpui::HostElement for StatusBar {
    fn name() -> &'static str {
        "StatusBar"
    }
}

impl gpui::Render for StatusBar {
    fn render(&mut self, cx: &mut gpui::Context<Self>) -> gpui::View {
        let Theme { text, .. } = cx.theme();

        div()
            .w_full()
            .fixed_bottom()
            .height(px(28.0))
            .padding(px(4.0))
            .background(cx.theme().surface_overlay)
            .border_t_1()
            .border_color(hairline(0.2))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .py_1()
            .text_xs()
            .text_color(rgb(0x666666))
            // Regroupement à gauche : Projet / Hôte / Branche Git
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        crate::icons::icon("folder")
                            .size(px(12.0))
                            .text_color(cx.theme().text_muted),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(10.0))
                            .text_color(cx.theme().text_muted)
                            .child(SharedString::from(self.workspace_state.project_name.clone())),
                    )
                    .child(
                        crate::icons::icon("monitor")
                            .size(px(12.0))
                            .text_color(cx.theme().text_muted),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(10.0))
                            .text_color(cx.theme().text_muted)
                            .child(SharedString::from(self.workspace_state.environment.clone())),
                    )
                    .child(
                        crate::icons::icon("git-branch")
                            .size(px(12.0))
                            .text_color(cx.theme().text_muted),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(10.0))
                            .text_color(cx.theme().text_muted)
                            .child(SharedString::from(self.workspace_state.git_branch.clone())),
                    )
            )
            // Indicateur d'état du démon à droite
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        crate::icons::icon("monitor")
                            .size(px(12.0))
                            .text_color(cx.theme().text_muted),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(10.0))
                            .text_color(cx.theme().text_muted)
                            .child(SharedString::from(
                                match self.workspace_state.access_mode {
                                    komet_proto::AccessMode::FullAccess => "Full access",
                                    komet_proto::AccessMode::ReadOnly => "Read only",
                                    komet_proto::AccessMode::Sandboxed => "Sandboxed",
                                }
                                .into(),
                            )),
                    ),
            )
    }
}