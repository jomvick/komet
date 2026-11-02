//! Terminal panel: one-time panel creation, persisted prefs, tab events,
//! show/hide and the resize drag.

use super::*;

impl Shell {
    pub(super) fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let prefs = self.terminal_prefs();
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        terminal.update(cx, |panel, cx| panel.set_prefs(prefs, cx));
        let sub = cx.subscribe(&terminal, |this, panel, event: &TerminalPanelEvent, cx| {
            this.on_terminal_panel_event(panel, event, cx);
        });
        self.terminal_event_sub = Some(sub);
        self.terminal = Some(terminal.clone());
        terminal
    }

    /// The persisted terminal chrome settings, as the panels consume them.
    pub(super) fn terminal_prefs(&self) -> TerminalPrefs {
        TerminalPrefs {
            font_size: self.settings.terminal_font_size,
            scrollback_lines: self.settings.terminal_scrollback_lines,
        }
    }

    /// Mirror prefs onto the panel that did NOT report the change (updating
    /// the emitter back into itself while it is mid-update would re-borrow
    /// the entity).
    pub(super) fn push_terminal_prefs(
        &self,
        emitter: &Entity<TerminalPanel>,
        cx: &mut Context<Self>,
    ) {
        let prefs = self.terminal_prefs();
        for panel in [&self.terminal, &self.right_terminal].into_iter().flatten() {
            if panel.entity_id() == emitter.entity_id() {
                continue;
            }
            let panel = panel.clone();
            panel.update(cx, |panel, cx| panel.set_prefs(prefs, cx));
        }
    }

    pub(super) fn on_terminal_panel_event(
        &mut self,
        panel: Entity<TerminalPanel>,
        event: &TerminalPanelEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalPanelEvent::FontSizeChanged(size) => {
                self.settings.terminal_font_size = *size;
                self.schedule_save(cx);
                self.push_terminal_prefs(&panel, cx);
            }
            TerminalPanelEvent::TabActivated(key) => {
                // Embedded cycling: the surface strip highlights from shell
                // state, so follow the panel's new active tab — but only if
                // that tab has a surface (drawer tabs have none).
                let key_panel = self.panel_key(cx);
                let has = self
                    .right_tabs
                    .get(&key_panel)
                    .is_some_and(|tabs| tabs.contains(&RightSurface::Terminal(*key)));
                if has {
                    self.panels.update(&key_panel, |p| {
                        p.right_active = RightSurface::Terminal(*key)
                    });
                    cx.notify();
                }
            }
            TerminalPanelEvent::TabClosed(key) => {
                // Drop the surface for a closed embedded tab (the ✕ path also
                // does this, but keyboard/menu closes land here; retaining an
                // absent surface is a no-op). State-only: never update the
                // emitting panel here, it is mid-update.
                let key_panel = self.panel_key(cx);
                let surface = RightSurface::Terminal(*key);
                let had = if let Some(tabs) = self.right_tabs.get_mut(&key_panel) {
                    let before = tabs.len();
                    tabs.retain(|s| *s != surface);
                    tabs.len() != before
                } else {
                    false
                };
                if had {
                    let next = self
                        .right_tabs
                        .get(&key_panel)
                        .and_then(|tabs| {
                            tabs.iter().find_map(|s| match s {
                                RightSurface::Terminal(k) => Some(RightSurface::Terminal(*k)),
                                _ => None,
                            })
                        })
                        .unwrap_or(RightSurface::Picker);
                    self.panels.update(&key_panel, |p| {
                        if p.right_active == surface {
                            p.right_active = next;
                        }
                    });
                    cx.notify();
                }
            }
        }
    }

    pub(super) fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (komet `sessionPanels`).
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (komet terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in komet's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }
}
