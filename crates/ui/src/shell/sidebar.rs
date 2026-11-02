use gpui::{
    AnyElement, Context, IntoElement, MouseButton, SharedString, Window, div, prelude::*, px,
};
use komet_engine::environments::LOCAL_ENV_ID;
use komet_proto::{ChatIndicator, WorkspaceScope};

use super::*;
use crate::motion;
use crate::popover;
use crate::state::{WorkspaceEntry, WorkspaceStatus};
use crate::theme::Theme;

#[derive(Clone, Debug)]
pub struct SidebarSessionRow {
    pub status: ChatIndicator,
    pub chat: komet_proto::Chat,
    pub folder: String,
    pub branch: Option<String>,
}

impl Shell {
    pub(super) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat => self.render_chat_sidebar(&theme, window, cx),
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the shell surface; the
        // rounded workspace border provides the separation.
        self.pane_container(
            self.sidebar_tween,
            target,
            div().h_full().child(inner).into_any_element(),
        )
    }

    /// Settings-mode sidebar (komet settings-sidebar.tsx): window-control
    /// strip, "Settings" heading, icon section rows styled like session rows,
    /// and a Back row pinned to the bottom.
    pub(super) fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported). Device identity lives on
        // the Accounts page now — the one surface where the device matters.
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("settings-nav-scroll")
                    .flex_1()
                    .min_h_0()
                    // Keep the navigation scrollable on compact windows while
                    // keeping the Back control reachable below.
                    .overflow_y_scroll()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(2.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            let icon_tint = if selected {
                                theme.on_accent
                            } else {
                                theme.text_muted
                            };
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .rounded(px(8.0))
                                .px(px(Theme::SPACE_SM))
                                .py(px(6.0))
                                .text_size(px(13.0))
                                .when(selected, |el| {
                                    el.bg(crate::theme::glass_selected_bg())
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                })
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    div()
                                        .size(px(24.0))
                                        .flex_none()
                                        .rounded(px(7.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .when(selected, |el| el.bg(theme.accent))
                                        .child(
                                            icon(item.nav_icon())
                                                .size(px(15.0))
                                                .text_color(icon_tint),
                                        ),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back pinned to the bottom (komet settings-sidebar.tsx).
            .child(
                div().px(px(Theme::SPACE_SM)).pb(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .py(px(6.0))
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                        .child(
                            // AltArrowLeft chevron (komet settings-sidebar.tsx),
                            // not the straight history arrow.
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// One session row (komet session-row.tsx): status rail on the left
    /// (a live 2×3 mini spinner while working, a dot otherwise), title +
    /// relative time on the first line, "folder · device" underneath aligned
    /// to the title. Click selects; right-click opens the context menu.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        space_name: SharedString,
        branch: Option<SharedString>,
        harness: Option<komet_proto::HarnessId>,
        status: komet_proto::ChatIndicator,
        selected: bool,
        archived: bool,
        pinned: bool,
        theme: &Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Activity, not position (t3code Sidebar): status is a small colored
        // word + glyph in the row's top-right corner — Working animates the
        // composer-strip spinner, Done wears a check; Idle rows show the
        // relative time instead. Hovering the ROW swaps the corner for the
        // ARCHIVE button (UNARCHIVE on rows in the sidebar's archived
        // accordion), t3code's settle-on-hover.
        let corner_hovered = self.chat_status_hover.as_deref() == Some(id.as_str());
        let status_color = spaces::status_dot_color(status, theme);
        let status_label: Option<&'static str> = match status {
            komet_proto::ChatIndicator::Working => Some("Working"),
            komet_proto::ChatIndicator::AwaitingInput => Some("Input"),
            komet_proto::ChatIndicator::Errored => Some("Failed"),
            komet_proto::ChatIndicator::Completed => Some("Done"),
            komet_proto::ChatIndicator::Idle => None,
        };
        let corner_body: AnyElement = if corner_hovered {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .h(px(18.0))
                // The pill's padding bleeds right into the row's padding so
                // its TEXT right-aligns exactly where the status word/time
                // sits — the swap moves pixels around the label, not it.
                // 4px: what's left of the row's 8px padding then equals the
                // 4px of air above the pill (18px tall on the 14px line,
                // 6px row padding minus the 2px overflow).
                .px(px(4.0))
                .mr(px(-4.0))
                .rounded(px(5.0))
                .bg(crate::theme::wash(0.10))
                .hover(|s| s.bg(crate::theme::wash(0.18)))
                .child(
                    icon(if archived {
                        icons::ARCHIVE_UP_MINIMALISTIC
                    } else {
                        icons::ARCHIVE_MINIMALISTIC
                    })
                    .size(px(11.0))
                    .flex_none()
                    .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(if archived {
                            "Unarchive"
                        } else {
                            "Archive"
                        })),
                )
                .into_any_element()
        } else {
            match status_label {
                Some(label) => {
                    // Glyph slot: Done wears the check; every other status a
                    // dot in its color (the Working spinner lives at the
                    // row's bottom-right, not up here).
                    let glyph: AnyElement = if status == komet_proto::ChatIndicator::Completed {
                        icon(icons::CHECK)
                            .size(px(11.0))
                            .flex_none()
                            .text_color(status_color)
                            .into_any_element()
                    } else {
                        div()
                            .size(px(6.0))
                            .flex_none()
                            .rounded_full()
                            .bg(status_color)
                            .into_any_element()
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .child(glyph)
                        .child(
                            div()
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(status_color)
                                .child(SharedString::from(label)),
                        )
                        .into_any_element()
                }
                None => div()
                    .text_size(px(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(time_ago.clone())
                    .into_any_element(),
            }
        };
        // One stable wrapper across both states (identity keeps the hover
        // from flickering as the content swaps); the swap is driven by the
        // ROW's hover (user request — corner-only felt undiscoverable), but
        // archiving only clicks on the corner itself, so the row's own click
        // stays the selector.
        let corner: AnyElement = {
            let archive_id = id.clone();
            div()
                .id(SharedString::from(format!("chat-corner-{id}")))
                .flex_none()
                // Pin the corner to line 1's text height so the archive pill
                // (taller, padded) overflows vertically instead of growing the
                // row — the swap must not shift the card's content.
                // NO occlude: the ROW's hover drives the swap, and an
                // occluding corner un-hovered the row underneath it —
                // pill mounts, steals the pointer, row un-hovers, pill
                // unmounts, repeat (user-reported flicker). The pill's
                // stop_propagation click is separation enough.
                .h(px(14.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .when(corner_hovered, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.set_chat_archived(archive_id.clone(), !archived, cx);
                    }))
                })
                .child(corner_body)
                .into_any_element()
        };
        let (hover, text) = (theme.glass_hover(), theme.text);
        let selected_wash = crate::theme::glass_selected_bg();
        let subline = theme.text_muted.opacity(0.5);
        let select_id = id.clone();
        let menu_id = id.clone();
        // Hover fades over transition-colors (komet session-row.tsx) — both
        // the wash and the title brighten ride the same 150ms blend.
        let fade_key = format!("chat-row-{id}");
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        // A selected row must NOT drift toward the hover wash: in dark the two
        // fills are identical so the blend is a no-op, but light's hover sits
        // below its near-opaque selected fill, and blending toward it visibly
        // dimmed the active row under the pointer (user report).
        let hover_bg = if selected { selected_wash } else { hover };
        let rest_text = if selected { text } else { text.opacity(0.8) };
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            // No selection ring (user request) — the wash alone marks the
            // active row.
            // Row hover drives BOTH the wash blend and the corner's
            // status→Archive swap (one listener — gpui allows a single
            // hover listener per element).
            .on_hover({
                let fade_hover = motion::hover_listener(fade_key.clone(), cx.entity_id());
                let hover_id = id.clone();
                cx.listener(move |this, hovered: &bool, window, cx| {
                    fade_hover(hovered, window, cx);
                    if *hovered {
                        if this.chat_status_hover.as_deref() != Some(hover_id.as_str()) {
                            this.chat_status_hover = Some(hover_id.clone());
                            cx.notify();
                        }
                    } else if this.chat_status_hover.as_deref() == Some(hover_id.as_str()) {
                        this.chat_status_hover = None;
                        cx.notify();
                    }
                })
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat(select_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu.open((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            // Line 1: "project @ device", status word / time-ago right.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.0))
                            .line_height(px(14.0))
                            .text_color(subline)
                            .child(space_name),
                    )
                    .when(pinned, |el| {
                        el.child(
                            icon(icons::PIN)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.8)),
                        )
                    })
                    .child(div().text_color(subline).child(corner)),
            )
            // Line 2: the session title, flush left (t3code card line 2).
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .child(title),
            )
            // Line 3 (always): harness brand mark; worktree sessions append
            // the branch icon + name.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .when_some(
                        harness.map(crate::pickers::harness_brand_icon),
                        |el, (path, tint)| {
                            el.child(
                                icon(path)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(subline).opacity(0.8)),
                            )
                        },
                    )
                    .when_some(branch, |el, branch| {
                        el.child(
                            icon(icons::GIT_BRANCH)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(subline),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(subline)
                                .child(branch),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The workspace switcher: which machine the sidebar's lists below belong to,
    /// and the one click that moves the whole column onto another device. Rows
    /// carry their reachability dot; pairing a new device happens in Settings →
    /// Sync, which the menu's footer opens.
    pub(super) fn render_workspace_switcher(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, status, rows, active_id) = {
            let state = self.state.read(cx);
            (
                state.active_workspace_label(),
                state.active_workspace_status(),
                state.workspace_switcher(),
                state.active_workspace.clone(),
            )
        };
        let open = self.devices_menu.is_open();
        let trigger = div()
            .id("workspace-switcher")
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                "workspace-switcher",
                theme.text.opacity(0.8),
                theme.text,
            ))
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "workspace-switcher",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover(),
                )
            })
            .on_hover(motion::hover_listener("workspace-switcher", cx.entity_id()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_devices_menu(cx)))
            .child(self.workspace_dot(status, theme))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.6)),
            );
        let strip = |inner: AnyElement| {
            div()
                .flex_none()
                .px(px(Theme::SPACE_SM))
                .pt(px(8.0))
                .pb(px(2.0))
                .child(inner)
                .into_any_element()
        };
        if self.devices_menu.get().is_some() {
            let closing = self.devices_menu.closing_since();
            let menu = self.render_devices_menu(rows, active_id, theme, cx);
            return strip(
                trigger
                    .relative()
                    .child(popover::anchored_menu_below_unfrosted(
                        "workspace-switcher-menu",
                        menu,
                        closing,
                    ))
                    .into_any_element(),
            );
        }
        strip(trigger.into_any_element())
    }

    pub(super) fn render_devices_menu(
        &mut self,
        rows: Vec<WorkspaceEntry>,
        active_id: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut menu = popover::popover_card(theme)
            .w(px(self.settings.sidebar_width - 2.0 * Theme::SPACE_SM))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_devices_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(theme, "Devices"));
        for row in rows {
            let active = row.id == active_id;
            let key: SharedString = format!("workspace-row-{}", row.id).into();
            let target = row.id.clone();
            let glyph = if row.id == LOCAL_ENV_ID {
                icons::MONITOR
            } else {
                icons::CLOUD
            };
            let subline = if row.host.is_empty() {
                format!(
                    "{} device{}",
                    row.device_count,
                    if row.device_count == 1 { "" } else { "s" }
                )
            } else {
                row.host.clone()
            };
            menu = menu.child(
                popover::menu_row(theme, active, key.clone(), cx.entity_id())
                    .id(key)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_workspace(&target, cx);
                    }))
                    .child(
                        icon(glyph)
                            .size(px(15.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .truncate()
                                    .child(row.label.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(subline),
                            ),
                    )
                    .child(self.workspace_dot(row.status, theme)),
            );
        }
        menu = menu.child(popover::menu_separator()).child(
            popover::menu_row(theme, false, "workspace-pair-device", cx.entity_id())
                .id("workspace-pair-device")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_devices_menu(cx);
                    this.open_settings(SettingsSection::Sync, cx);
                }))
                .child(
                    icon(icons::PLUS)
                        .size(px(15.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(SharedString::from("Pair a device…")),
        );
        menu.into_any_element()
    }

    /// Reachability of one workspace: emerald live, amber still dialing, faint gone.
    pub(super) fn workspace_dot(&self, status: WorkspaceStatus, theme: &Theme) -> AnyElement {
        let color = match status {
            WorkspaceStatus::Online => theme.success,
            WorkspaceStatus::Connecting => theme.warning,
            WorkspaceStatus::Offline => theme.text_faint,
        };
        div()
            .size(px(7.0))
            .flex_none()
            .rounded_full()
            .bg(color)
            .into_any_element()
    }

    pub(super) fn toggle_devices_menu(&mut self, cx: &mut Context<Self>) {
        if self.devices_menu.is_open() {
            self.close_devices_menu(cx);
            return;
        }
        self.devices_menu.open(());
        cx.notify();
    }

    pub(super) fn close_devices_menu(&mut self, cx: &mut Context<Self>) {
        if self.devices_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.devices_menu);
            cx.notify();
        }
    }

    /// Show another device's workspace. The sidebar's lists, the composer's
    /// pickers and the transcript follow it; the project filter is a pick inside
    /// one workspace, so it resets.
    pub(super) fn select_workspace(&mut self, env_id: &str, cx: &mut Context<Self>) {
        self.close_devices_menu(cx);
        if self.state.read(cx).active_workspace == env_id {
            return;
        }
        self.settings.space_filter = None;
        self.settings.active_workspace = Some(env_id.to_string());
        let env_id = env_id.to_string();
        let state = self.state.clone();
        state.update(cx, |state, cx| state.switch_workspace(env_id, cx));
        self.schedule_save(cx);
        cx.notify();
    }

    /// Chat-mode sidebar (spaces overhaul): window-control strip, the Spaces
    /// section (folder + device rows, add-space), the global Active sessions
    /// list, the notice strip, and the UserMenu (§1.6).
    pub(super) fn render_chat_sidebar(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (user, workspace_scope) = {
            let state = self.state.read(cx);
            (state.auth_user().cloned(), state.workspace_scope)
        };

        // When "All projects" (filter = None) → grouped by project folder.
        // When a specific project is selected → flat list with FLIP resort.
        let list_items: Vec<AnyElement> = if self.settings.space_filter.is_none() {
            self.render_grouped_rows(theme, window, cx)
        } else {
            // Keyed rows: (stable key, estimated height, element) — the key + height
            // list drives the §1.6 resort FLIP diff below (attention-bucket
            // promotions glide; cleared rows just go).
            let keyed: Vec<(String, f32, AnyElement)> = self.render_active_rows(theme, window, cx);

            // Resort glide (§1.6 View Transitions parity): when the ORDER of a live
            // list changes (new activity resort, grouping flip), surviving rows
            // glide from their old y to the new one — layout is already at the new
            // position; the offset is a paint-only relative inset animated to 0
            // over 260ms cubic-bezier(0.22,1,0.36,1). New rows fade in; removals
            // just go (matching the original). First fill and chat switches (which
            // don't reorder) never animate.
            let order: Vec<(String, f32)> = keyed.iter().map(|(k, h, _)| (k.clone(), *h)).collect();
            if self.sidebar_prev_order != order {
                if !self.sidebar_prev_order.is_empty() {
                    let offsets =
                        resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP);
                    let prev_keys: std::collections::HashSet<&str> = self
                        .sidebar_prev_order
                        .iter()
                        .map(|(k, _)| k.as_str())
                        .collect();
                    let new_keys: std::collections::HashSet<String> = order
                        .iter()
                        .filter(|(k, _)| !prev_keys.contains(k.as_str()))
                        .map(|(k, _)| k.clone())
                        .collect();
                    if !offsets.is_empty() || !new_keys.is_empty() {
                        self.resort_epoch += 1;
                        self.sidebar_resort = offsets;
                        self.sidebar_new_keys = new_keys;
                    }
                }
                self.sidebar_prev_order = order;
            }
            let epoch = self.resort_epoch;
            keyed
                .into_iter()
                .map(|(key, _, element)| {
                    if let Some(dy) = self.sidebar_resort.get(&key).copied() {
                        let id = SharedString::from(format!("resort-{epoch}-{key}"));
                        div()
                            .child(element)
                            .with_animation(id, RESORT.animation(), move |el, t| {
                                el.relative().top(px(dy * (1.0 - t)))
                            })
                            .into_any_element()
                    } else if self.sidebar_new_keys.contains(&key) {
                        let id = SharedString::from(format!("row-in-{epoch}-{key}"));
                        motion::fade_quick(id, div().child(element)).into_any_element()
                    } else {
                        element
                    }
                })
                .collect()
        };

        // t3code's archived accordion, below the active list.
        let archived_section = self.render_archived_section(theme, cx);

        let (user_line, trigger_subline, menu_identity): (
            SharedString,
            Option<SharedString>,
            SharedString,
        ) = match workspace_scope {
            Some(WorkspaceScope::Local) => {
                let line = if matches!(self.sync_flow, SyncFlow::RestartPending { .. }) {
                    "Sync ready after restart"
                } else {
                    "Local only"
                };
                (line.into(), None, "Stored on this device".into())
            }
            Some(WorkspaceScope::Development) => (
                "Development".into(),
                Some("Local development runtime".into()),
                "Authentication disabled".into(),
            ),
            Some(WorkspaceScope::Synced) | None => {
                let line: SharedString = user
                    .as_ref()
                    .map(|u| u.name.clone().unwrap_or_else(|| u.email.clone()).into())
                    .unwrap_or_else(|| SharedString::from("Not signed in"));
                let email = user
                    .as_ref()
                    .map(|u| SharedString::from(u.email.clone()))
                    .unwrap_or_else(|| line.clone());
                (line, Some("Alpha".into()), email)
            }
        };
        // In Development mode show a plain Settings button instead of the
        // profile card — there is no real account to display in that scope.
        let is_dev = matches!(workspace_scope, Some(WorkspaceScope::Development));
        let bottom_bar: AnyElement = if is_dev {
            let open = matches!(self.route, Route::Settings(_));
            popover::menu_row(theme, open, "sidebar-settings-btn", cx.entity_id())
                .id("sidebar-settings-btn")
                .px(px(Theme::SPACE_SM))
                .py(px(Theme::SPACE_SM))
                .rounded(px(8.0))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_settings(SettingsSection::Devices, cx);
                }))
                .child(
                    icon(icons::SETTINGS_MINIMALISTIC)
                        .size(px(16.0))
                        .text_color(if open { theme.text } else { theme.text_muted }),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if open { theme.text } else { theme.text_muted })
                        .child(SharedString::from("Settings")),
                )
                .into_any_element()
        } else {
            self.render_user_menu(user_line.clone(), trigger_subline, menu_identity, theme, cx)
        };
        let user_menu = bottom_bar;

        // Keep the project selector at the top and move the local workspace row
        // down to the footer, as in the requested sidebar layout.
        let device_row = self.render_workspace_switcher(theme, cx);
        let filter_row = self.render_spaces_filter(theme, cx);

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            .child(filter_row)
            // The (filtered) Sessions list scrolls inside an EdgeFade scope —
            // a true per-glyph gradient at active overflow edges. Glass-safe
            // (no painted overlay can fade content over see-through blur) and
            // equivalent on opaque themes: alpha→0 reveals the surface tone
            // underneath, same as the gradient overlays it replaced. Overflow
            // is read at PAINT time via the scroll handle — render-time gating
            // rode the previous frame's offset, so the last frame of a content
            // shrink (row archived while scrolled) left a phantom fade stuck
            // over an unscrollable list (user report).
            .child(
                crate::edge_fade::edge_faded(
                    SIDEBAR_GLASS_FADE_BAND,
                    true,
                    true,
                    div().relative().flex_1().min_h_0().child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            // No "Sessions" header (user request) — the list
                            // is the whole column; a little air stands in.
                            .pt(px(4.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .pb(px(Theme::SPACE_SM))
                                    .children(list_items),
                            )
                            .children(archived_section),
                    ),
                )
                .fade_overflow_y(&self.sidebar_scroll),
            )
            // Update strip (above the user menu; below the lists).
            .when_some(self.render_update_strip(theme, cx), |el, strip| {
                el.child(strip)
            })
            // Inline mutation-failure notice.
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(
                div()
                    .px(px(Theme::SPACE_SM))
                    .pb(px(Theme::SPACE_XS))
                    .flex_none()
                    .child(device_row),
            )
            .child(div().p(px(Theme::SPACE_SM)).flex_none().child(user_menu))
            .into_any_element()
    }
}
