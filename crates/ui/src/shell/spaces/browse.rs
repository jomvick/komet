//! Remote add-project flow: inside an environment workspace the OS folder
//! picker can't see the environment's filesystem, so "+" opens this browser
//! instead — LIST_FOLDERS on the env's engine, then createSpace on its device.

use super::*;

/// Parent of `path`, or `None` at a filesystem root / relative path.
fn folder_parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => Some("/".into()),
        Some((parent, _)) => Some(parent.to_string()),
        None => None,
    }
}

impl Shell {
    /// "+" inside an environment workspace: browse the environment's folders
    /// over RPC — the local OS picker would only see this machine.
    pub(crate) fn open_remote_folder_browser(&mut self, cx: &mut Context<Self>) {
        let (local_id, devices) = {
            let state = self.state.read(cx);
            (state.local_device_id.clone(), state.devices.clone())
        };
        let device = devices
            .iter()
            .find(|d| local_id.as_deref() == Some(d.id.as_str()))
            .or_else(|| devices.first())
            .cloned();
        let Some(device) = device else {
            return;
        };
        self.remote_folder_browser = Some(RemoteFolderBrowser {
            device,
            path: None,
            listing: None,
            loading: false,
            error: None,
            generation: 0,
            focus: cx.focus_handle(),
            focus_pending: true,
            list_scroll: gpui::ScrollHandle::new(),
        });
        self.remote_folder_browser_load(cx);
        cx.notify();
    }

    /// (Re)load the requested folder on the environment's engine. `path:
    /// None` asks for the device's home dir.
    fn remote_folder_browser_load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(browser) = self.remote_folder_browser.as_mut() {
                browser.loading = false;
                browser.error = Some("This workspace has no engine connection.".into());
            }
            cx.notify();
            return;
        };
        let (path, generation, target) = {
            let Some(browser) = self.remote_folder_browser.as_mut() else {
                return;
            };
            browser.loading = true;
            browser.error = None;
            browser.generation += 1;
            (
                browser.path.clone(),
                browser.generation,
                browser.device.id.clone(),
            )
        };
        let client = engine.client_arc();
        cx.spawn(async move |this, cx| {
            // `targetDeviceId` makes the env-scoped EngineRpc relay the call to
            // the environment's own device — without it ListFolders would be
            // answered by THIS machine's repos service (local folders).
            let mut params = serde_json::json!({ "targetDeviceId": target });
            if let (Some(path), Some(map)) = (path, params.as_object_mut()) {
                map.insert("path".into(), path.into());
            }
            let result = client
                .call_as::<FolderListing>(methods::LIST_FOLDERS, params)
                .await;
            this.update(cx, |shell, cx| {
                let Some(browser) = shell.remote_folder_browser.as_mut() else {
                    return;
                };
                // A slower earlier navigation must not overwrite a newer one.
                if browser.generation != generation {
                    return;
                }
                browser.loading = false;
                match result {
                    Ok(listing) => {
                        browser.path = Some(listing.path.clone());
                        browser.listing = Some(listing);
                    }
                    Err(err) => browser.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Descend into `path` (the ".." row passes the parent path).
    fn remote_folder_browser_open(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(browser) = self.remote_folder_browser.as_mut() {
            browser.path = Some(path);
        }
        self.remote_folder_browser_load(cx);
    }

    /// "Add this folder": create the space on the environment's device, then
    /// close the browser.
    fn remote_folder_browser_submit(&mut self, cx: &mut Context<Self>) {
        let Some(browser) = self.remote_folder_browser.take() else {
            return;
        };
        let Some(listing) = browser.listing else {
            cx.notify();
            return;
        };
        self.create_space_at(browser.device, listing.path, cx);
    }

    /// The browser modal — appended to the shell's overlay list by
    /// `render_space_overlays`.
    pub(crate) fn render_remote_folder_browser(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let (device, listing, loading, error, focus, scroll) = {
            let browser = self.remote_folder_browser.as_mut()?;
            if std::mem::take(&mut browser.focus_pending) {
                window.focus(&browser.focus, cx);
            }
            (
                browser.device.clone(),
                browser.listing.clone(),
                browser.loading,
                browser.error.clone(),
                browser.focus.clone(),
                browser.list_scroll.clone(),
            )
        };
        let device_label = self
            .state
            .read(cx)
            .device_name(&device.id)
            .map(str::to_string)
            .unwrap_or_else(|| device.name.clone());

        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(listing) = &listing {
            let base = listing.path.trim_end_matches('/').to_string();
            if let Some(parent) = folder_parent(&listing.path) {
                rows.push(
                    popover::menu_row(&theme, false, "remote-folder-parent", cx.entity_id())
                        .id("remote-folder-parent")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remote_folder_browser_open(parent.clone(), cx)
                        }))
                        .child(
                            icon(icons::FOLDER)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(".."))
                        .into_any_element(),
                );
            }
            for entry in listing.entries.iter().filter(|e| e.is_dir) {
                let name = entry.name.clone();
                let target = format!("{base}/{name}");
                let fade_key = format!("remote-folder-row-{name}");
                let row_id = SharedString::from(fade_key.clone());
                rows.push(
                    popover::menu_row(&theme, false, fade_key, cx.entity_id())
                        .id(row_id)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remote_folder_browser_open(target.clone(), cx)
                        }))
                        .child(
                            icon(icons::FOLDER)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(name))
                        .into_any_element(),
                );
            }
            if rows.is_empty() {
                rows.push(
                    div()
                        .px(px(8.0))
                        .py(px(6.0))
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("No subfolders here."))
                        .into_any_element(),
                );
            }
        }

        let path_label = listing
            .as_ref()
            .map(|l| l.path.clone())
            .unwrap_or_else(|| "…".to_string());
        let list_body: AnyElement = if listing.is_none() && loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("Loading folders…"))
                .into_any_element()
        } else if listing.is_none() {
            let copy = error
                .clone()
                .unwrap_or_else(|| "Could not list that folder.".into());
            div()
                .px(px(8.0))
                .py(px(6.0))
                .text_size(px(13.0))
                .text_color(theme.danger)
                .child(SharedString::from(copy))
                .into_any_element()
        } else {
            div()
                .id("remote-folder-browser-list")
                .flex()
                .flex_col()
                .max_h(px(320.0))
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .children(rows)
                .into_any_element()
        };

        let card = popover::dialog_card(&theme)
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.remote_folder_browser = None;
                    cx.notify();
                } else if ev.keystroke.key == "enter" {
                    this.remote_folder_browser_submit(cx);
                }
            }))
            .child(popover::dialog_title(
                &theme,
                &format!("Add project on {device_label}"),
            ))
            .child(
                div()
                    .mt(px(6.0))
                    .child(popover::dialog_body(&theme, path_label)),
            )
            .child(div().mt(px(10.0)).child(list_body))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(
                            &theme,
                            "Cancel",
                            "remote-folder-cancel",
                            cx.entity_id(),
                        )
                        .id("remote-folder-cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.remote_folder_browser = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Add this folder")
                            .id("remote-folder-add")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.remote_folder_browser_submit(cx)),
                            ),
                    ),
            )
            .into_any_element();
        Some(popover::modal("remote-folder-browser", viewport, card))
    }
}
