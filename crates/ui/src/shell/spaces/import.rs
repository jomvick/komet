//! "Import project…": copy one project folder from a source workspace into
//! another (this machine ⇄ a paired one, or between two paired ones).
//!
//! v1 copies the project only — its chats stay with the workspace that hosts
//! them. The folder preview (`ProjectTransferPlan`) runs on the SOURCE device,
//! the archive is built there and streamed to the destination device in chunks
//! (`ProjectExport*` → `ProjectImport*`), and only a verified archive is
//! extracted. The transfer itself is local-engine orchestration: the UI drives
//! two RPC surfaces, no byte of the project passes through the interface
//! process.

use super::*;
use komet_rpc::methods;

/// One line of the archive plan the source device computed.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanView {
    path: String,
    name: String,
    files: u64,
    bytes: u64,
    ignored_files: u64,
    secrets: Vec<PlanSecret>,
    warnings: Vec<String>,
    git_ready: bool,
    within_limit: bool,
    git: Option<GitView>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanSecret {
    path: String,
    bytes: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitView {
    remote: Option<String>,
    branch: Option<String>,
    dirty: bool,
}

/// The dialog's live state.
pub(crate) struct ImportDialog {
    /// Workspace the project is copied FROM (an env id, `local` included).
    source_env: String,
    source_input: Entity<ComposerInput>,
    /// Base folder on the DESTINATION workspace.
    dest_input: Entity<ComposerInput>,
    include_secrets: bool,
    /// Git mode clones the remote on the destination instead of streaming the
    /// archive; only offered when the plan found a reachable remote.
    git_mode: bool,
    plan: Option<PlanView>,
    busy: bool,
    /// Bytes moved / total while the archive streams.
    progress: Option<(u64, u64)>,
    error: Option<String>,
    done: Option<String>,
}

impl Shell {
    /// "Import project…" from the spaces menu: seed source = the workspace in
    /// view, source folder = the selected project (when there is one).
    pub(crate) fn open_import_dialog(&mut self, cx: &mut Context<Self>) {
        self.close_spaces_menu(cx);
        let (source_env, seed_path, seed_dest) = {
            let state = self.state.read(cx);
            let source_env = state.active_workspace.clone();
            let seed_path = self
                .settings
                .space_filter
                .clone()
                .or_else(|| state.selected_space.clone())
                .and_then(|id| state.space_row(&id).map(|s| s.path.clone()))
                .unwrap_or_default();
            let seed_dest = state
                .space_row(&self.settings.space_filter.clone().unwrap_or_default())
                .and_then(|s| {
                    s.path
                        .rsplit_once('/')
                        .map(|(parent, _)| parent.to_string())
                })
                .unwrap_or_else(|| "~".to_string());
            (source_env, seed_path, seed_dest)
        };
        let source_input = cx.new(|cx| ComposerInput::new("Source folder", cx));
        source_input.update(cx, |input, cx| input.set_text(seed_path, cx));
        let dest_input = cx.new(|cx| ComposerInput::new("Destination folder", cx));
        dest_input.update(cx, |input, cx| input.set_text(seed_dest, cx));
        self.import_dialog = Some(ImportDialog {
            source_env,
            source_input,
            dest_input,
            include_secrets: false,
            git_mode: true,
            plan: None,
            busy: false,
            progress: None,
            error: None,
            done: None,
        });
        cx.notify();
    }

    fn close_import_dialog(&mut self, cx: &mut Context<Self>) {
        self.import_dialog = None;
        cx.notify();
    }

    /// The device that owns a workspace's filesystem: this machine for `local`,
    /// the paired workspace's home device otherwise.
    fn workspace_device(&self, env_id: &str, cx: &App) -> Option<String> {
        self.state.read(cx).workspace_device_for(env_id)
    }

    /// Ask the SOURCE device to inspect the folder; nothing is copied yet.
    pub(crate) fn request_import_plan(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_ref() else {
            return;
        };
        let path = dialog.source_input.read(cx).text().trim().to_string();
        let source_env = dialog.source_env.clone();
        if path.is_empty() {
            if let Some(dialog) = self.import_dialog.as_mut() {
                dialog.error = Some("Choose the folder to copy.".into());
            }
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(device) = self.workspace_device(&source_env, cx) else {
            if let Some(dialog) = self.import_dialog.as_mut() {
                dialog.error = Some("That workspace has no reachable device.".into());
            }
            cx.notify();
            return;
        };
        if let Some(dialog) = self.import_dialog.as_mut() {
            dialog.busy = true;
            dialog.error = None;
            dialog.plan = None;
            dialog.done = None;
        }
        cx.notify();
        let client = engine.client_arc();
        cx.spawn(async move |this, cx| {
            let result = client
                .call(
                    methods::PROJECT_TRANSFER_PLAN,
                    serde_json::json!({ "path": path, "targetDeviceId": device }),
                )
                .await;
            this.update(cx, |shell, cx| {
                if let Some(dialog) = shell.import_dialog.as_mut() {
                    dialog.busy = false;
                    match result {
                        Ok(value) => match serde_json::from_value::<PlanView>(value) {
                            Ok(plan) => {
                                // Git mode is the cheap path: no project bytes
                                // travel, so prefer it whenever a remote exists.
                                dialog.git_mode = plan.git_ready;
                                dialog.plan = Some(plan);
                            }
                            Err(err) => dialog.error = Some(format!("unexpected plan: {err}")),
                        },
                        Err(err) => dialog.error = Some(err.to_string()),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run the archive transfer: export on the source device, stream the chunks
    /// into the destination device, verify, extract, then register the space.
    pub(crate) fn start_import(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_ref() else {
            return;
        };
        let Some(plan) = dialog.plan.clone() else {
            return;
        };
        let source_env = dialog.source_env.clone();
        let include_secrets = dialog.include_secrets;
        let git_mode = dialog.git_mode;
        let dest_base = dialog.dest_input.read(cx).text().trim().to_string();
        let dest_env = self.state.read(cx).active_workspace.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (Some(source_dev), Some(dest_dev)) = (
            self.workspace_device(&source_env, cx),
            self.workspace_device(&dest_env, cx),
        ) else {
            if let Some(dialog) = self.import_dialog.as_mut() {
                dialog.error = Some("A workspace has no reachable device.".into());
            }
            cx.notify();
            return;
        };
        if dest_base.is_empty() {
            if let Some(dialog) = self.import_dialog.as_mut() {
                dialog.error = Some("Choose where the copy should land.".into());
            }
            cx.notify();
            return;
        }
        if !git_mode && !plan.within_limit {
            if let Some(dialog) = self.import_dialog.as_mut() {
                dialog.error = Some("This project is too large for an archive transfer.".into());
            }
            cx.notify();
            return;
        }
        if git_mode && !plan.git_ready {
            if let Some(dialog) = self.import_dialog.as_mut() {
                dialog.error = Some("No git remote for this project; use the archive mode.".into());
            }
            cx.notify();
            return;
        }
        if let Some(dialog) = self.import_dialog.as_mut() {
            dialog.busy = true;
            dialog.error = None;
            dialog.done = None;
            dialog.progress = Some((0, 1));
        }
        cx.notify();

        let client = engine.client_arc();
        let source_path = plan.path.clone();
        let name = plan.name.clone();
        let remote = plan.git.as_ref().and_then(|git| git.remote.clone());
        let branch = plan.git.as_ref().and_then(|git| git.branch.clone());
        cx.spawn(async move |this, cx| {
            let (created_path, progress, transfer_error) = if git_mode {
                let outcome = match remote {
                    Some(remote) => {
                        clone_project(
                            client.clone(),
                            &dest_dev,
                            &remote,
                            branch.as_deref(),
                            &dest_base,
                            &name,
                        )
                        .await
                    }
                    None => Err("no git remote for this project".to_string()),
                };
                match outcome {
                    Ok(path) => (Some(path), Some((1, 1)), None),
                    Err(err) => (None, None, Some(err)),
                }
            } else {
                match transfer_archive(
                    client.clone(),
                    &source_dev,
                    &source_path,
                    include_secrets,
                    &dest_dev,
                    &dest_base,
                    &name,
                )
                .await
                {
                    Ok(done) => (
                        Some(done.created_path),
                        Some((done.chunks, done.total_chunks)),
                        None,
                    ),
                    Err(err) => (None, None, Some(err)),
                }
            };
            // Register the copied folder on the destination device. A failure
            // leaves the files on disk (never silently deleted) and shows the
            // error; the copy is still usable by hand.
            let mut error = None;
            if let Some(path) = &created_path {
                let space_id = uuid::Uuid::new_v4().to_string();
                if let Err(err) = client
                    .call(
                        methods::MUTATE,
                        serde_json::json!({
                            "op": "createSpace",
                            "spaceId": space_id,
                            "deviceId": dest_dev,
                            "path": path,
                            "gitDetected": false,
                        }),
                    )
                    .await
                {
                    error = Some(format!(
                        "copied to {path}, but registering the project failed: {err}"
                    ));
                }
            }
            this.update(cx, |shell, cx| {
                if let Some(dialog) = shell.import_dialog.as_mut() {
                    dialog.busy = false;
                    if let Some(progress) = progress {
                        dialog.progress = Some(progress);
                    }
                    match (created_path, error, transfer_error) {
                        (Some(path), None, _) => {
                            dialog.done = Some(format!("Imported {name} to {path}."));
                        }
                        (Some(_), Some(err), _) => dialog.error = Some(err),
                        (None, _, Some(err)) => dialog.error = Some(err),
                        (None, _, None) => {
                            dialog.error =
                                Some("The transfer failed; nothing was extracted.".into())
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn render_import_overlay(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.import_dialog.as_ref()?;
        if std::mem::take(&mut self.import_focus_pending) {
            let focus = dialog.source_input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        let (source_env, busy, include_secrets, git_mode) = (
            dialog.source_env.clone(),
            dialog.busy,
            dialog.include_secrets,
            dialog.git_mode,
        );
        let workspaces = self.state.read(cx).workspace_switcher();
        let plan = dialog.plan.clone();
        let error = dialog.error.clone();
        let done = dialog.done.clone();
        let progress = dialog.progress;
        let source_input = dialog.source_input.clone();
        let dest_input = dialog.dest_input.clone();

        let mut card = popover::dialog_card(&theme)
            .w(px(460.0))
            .child(popover::dialog_title(&theme, "Import project"))
            .child(
                popover::dialog_body(
                    &theme,
                    "Copy a project folder from another workspace. Its chats stay where they \
                     are; the project is copied, not moved.",
                )
                .mt(px(6.0)),
            )
            .child(
                div()
                    .mt(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    // Source workspace: one chip per row, the active one marked.
                    .child(div().flex().flex_row().flex_wrap().gap(px(6.0)).children(
                        workspaces.into_iter().map(|row| {
                            let selected = row.id == source_env;
                            let id = row.id.clone();
                            let label = if row.host.is_empty() {
                                row.label.clone()
                            } else {
                                format!("{} · {}", row.label, row.host)
                            };
                            popover::btn_ghost(&theme, &label, "import-source-env", cx.entity_id())
                                .id(SharedString::from(format!("import-src-{id}")))
                                .when(selected, |el| el.border_color(theme.text))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(dialog) = this.import_dialog.as_mut() {
                                        dialog.source_env = id.clone();
                                        dialog.plan = None;
                                    }
                                    cx.notify();
                                }))
                        }),
                    ))
                    .child(popover::dialog_field(source_input.into_any_element()))
                    .child(popover::dialog_field(dest_input.into_any_element())),
            );

        if let Some(plan) = &plan {
            let git = plan
                .git
                .as_ref()
                .and_then(|git| git.remote.clone())
                .unwrap_or_else(|| "no git remote".into());
            if plan.git_ready {
                card = card.child(
                    div()
                        .mt(px(10.0))
                        .flex()
                        .flex_row()
                        .gap(px(6.0))
                        .child(
                            popover::btn_ghost(
                                &theme,
                                "Git clone (recommended)",
                                "import-mode-git",
                                cx.entity_id(),
                            )
                            .id("import-mode-git")
                            .when(git_mode, |el| el.border_color(theme.text))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(dialog) = this.import_dialog.as_mut() {
                                    dialog.git_mode = true;
                                }
                                cx.notify();
                            })),
                        )
                        .child(
                            popover::btn_ghost(
                                &theme,
                                "Archive (uncommitted work)",
                                "import-mode-archive",
                                cx.entity_id(),
                            )
                            .id("import-mode-archive")
                            .when(!git_mode, |el| el.border_color(theme.text))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(dialog) = this.import_dialog.as_mut() {
                                    dialog.git_mode = false;
                                }
                                cx.notify();
                            })),
                        ),
                );
            }
            card = card.child(
                div()
                    .mt(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!(
                        "{} files · {} · {} ignored",
                        plan.files,
                        human_bytes(plan.bytes),
                        plan.ignored_files
                    )))
                    .child(SharedString::from(format!(
                        "git: {git}{}{} — {}",
                        plan.git
                            .as_ref()
                            .and_then(|g| g.branch.clone())
                            .map(|b| format!(" @ {b}"))
                            .unwrap_or_default(),
                        if plan.git.as_ref().is_some_and(|g| g.dirty) {
                            " (uncommitted changes)"
                        } else {
                            ""
                        },
                        if plan.git_ready {
                            "git clone available"
                        } else {
                            "archive only"
                        }
                    )))
                    .when(!plan.secrets.is_empty() && !include_secrets, |el| {
                        el.child(
                            div()
                                .text_color(theme.warning_muted)
                                .child(SharedString::from(format!(
                                    "{} secret-looking file(s) will be skipped: {}",
                                    plan.secrets.len(),
                                    plan.secrets
                                        .iter()
                                        .map(|secret| format!("{} ({})", secret.path, human_bytes(secret.bytes)))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ))),
                        )
                    })
                    .children(plan.warnings.iter().filter_map(|warning| match warning.as_str() {
                        "largeProject" => Some(div().child("Large project — this may take a while.")),
                        "caseCollision" => Some(div().child(
                            "Two files differ only by case; the destination may overwrite one.",
                        )),
                        _ => None,
                    })),
            );
        }
        if let Some(error) = error {
            card = card.child(
                div()
                    .mt(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger_muted)
                    .child(SharedString::from(error)),
            );
        }
        if let Some(done) = done {
            card = card.child(
                div()
                    .mt(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.success)
                    .child(SharedString::from(done)),
            );
        }
        if let Some((moved, total)) = progress
            && busy
        {
            card = card.child(
                div()
                    .mt(px(8.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("{moved}/{total} chunks"))),
            );
        }
        if !git_mode {
            card = card.child(
                div()
                    .mt(px(6.0))
                    .id("import-secrets-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(dialog) = this.import_dialog.as_mut() {
                            dialog.include_secrets = !dialog.include_secrets;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(14.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if include_secrets {
                                theme.text
                            } else {
                                theme.border
                            })
                            .bg(if include_secrets {
                                theme.text
                            } else {
                                crate::theme::ink(0.03)
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(include_secrets, |el| {
                                el.child(
                                    icons::icon(icons::CHECK)
                                        .size(px(10.0))
                                        .text_color(theme.on_solid),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child("Include .env and key-like files (not recommended)"),
                    ),
            );
        }
        card = card.child(
            div()
                .mt(px(18.0))
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(
                    popover::btn_ghost(&theme, "Cancel", "import-cancel", cx.entity_id())
                        .id("import-cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.close_import_dialog(cx))),
                )
                .child(
                    popover::btn_ghost(&theme, "Preview", "import-preview", cx.entity_id())
                        .id("import-preview")
                        .on_click(cx.listener(|this, _, _, cx| this.request_import_plan(cx))),
                )
                .child(
                    popover::btn_primary(&theme, if busy { "Importing…" } else { "Import" })
                        .id("import-start")
                        .on_click(cx.listener(|this, _, _, cx| this.start_import(cx))),
                ),
        );
        Some(popover::modal(
            "import-project-dialog",
            viewport,
            card.into_any_element(),
        ))
    }
}

/// Git mode: the destination device clones the remote itself. Nothing but the
/// URL/branch metadata crosses the wire.
async fn clone_project(
    client: std::sync::Arc<komet_rpc::RpcClient>,
    dest_dev: &str,
    remote: &str,
    branch: Option<&str>,
    base_path: &str,
    name: &str,
) -> Result<String, String> {
    let value = client
        .call(
            methods::PROJECT_CLONE,
            serde_json::json!({
                "remote": remote,
                "branch": branch,
                "basePath": base_path,
                "name": name,
                "targetDeviceId": dest_dev,
            }),
        )
        .await
        .map_err(|err| format!("git clone failed: {err}"))?;
    Ok(value
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Stream one archive from `source_dev` to `dest_dev` and extract it into
/// `dest_base`. Returns the number of chunks moved (for the dialog's progress).
#[allow(clippy::too_many_arguments)]
struct TransferDone {
    created_path: String,
    chunks: u64,
    total_chunks: u64,
}

async fn transfer_archive(
    client: std::sync::Arc<komet_rpc::RpcClient>,
    source_dev: &str,
    source_path: &str,
    include_secrets: bool,
    dest_dev: &str,
    dest_base: &str,
    name: &str,
) -> Result<TransferDone, String> {
    let opened = client
        .call(
            methods::PROJECT_EXPORT_OPEN,
            serde_json::json!({
                "path": source_path,
                "includeSecrets": include_secrets,
                "targetDeviceId": source_dev,
            }),
        )
        .await
        .map_err(|err| format!("export failed: {err}"))?;
    let transfer_id = opened
        .get("transferId")
        .and_then(|v| v.as_str())
        .ok_or("export returned no transfer id")?
        .to_string();
    let bytes = opened.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
    let sha = opened
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let begun = client
        .call(
            methods::PROJECT_IMPORT_BEGIN,
            serde_json::json!({
                "name": name,
                "bytes": bytes,
                "sha256": sha,
                "targetDeviceId": dest_dev,
            }),
        )
        .await
        .map_err(|err| format!("import begin failed: {err}"))?;
    let import_id = begun
        .get("transferId")
        .and_then(|v| v.as_str())
        .ok_or("import begin returned no transfer id")?
        .to_string();

    let mut offset = 0u64;
    let mut chunks = 0u64;
    let total_chunks = bytes
        .div_ceil(komet_engine::project_transfer::CHUNK_BYTES)
        .max(1);
    loop {
        let chunk = match client
            .call(
                methods::PROJECT_EXPORT_CHUNK,
                serde_json::json!({
                    "transferId": transfer_id,
                    "offset": offset,
                    "targetDeviceId": source_dev,
                }),
            )
            .await
        {
            Ok(chunk) => chunk,
            Err(err) => {
                let _ = client
                    .call(
                        methods::PROJECT_TRANSFER_CANCEL,
                        serde_json::json!({ "transferId": import_id, "targetDeviceId": dest_dev }),
                    )
                    .await;
                return Err(format!("export chunk failed: {err}"));
            }
        };
        let data = chunk
            .get("data")
            .and_then(|v| v.as_str())
            .ok_or("export chunk returned no data")?;
        let next = chunk
            .get("nextOffset")
            .and_then(|v| v.as_u64())
            .unwrap_or(bytes);
        client
            .call(
                methods::PROJECT_IMPORT_CHUNK,
                serde_json::json!({
                    "transferId": import_id,
                    "offset": offset,
                    "data": data,
                    "targetDeviceId": dest_dev,
                }),
            )
            .await
            .map_err(|err| format!("import chunk failed: {err}"))?;
        chunks += 1;
        offset = next;
        if chunk.get("done").and_then(|v| v.as_bool()).unwrap_or(true) {
            break;
        }
    }
    let committed = client
        .call(
            methods::PROJECT_IMPORT_COMMIT,
            serde_json::json!({
                "transferId": import_id,
                "basePath": dest_base,
                "name": name,
                "targetDeviceId": dest_dev,
            }),
        )
        .await
        .map_err(|err| format!("import commit failed: {err}"))?;
    let created_path = committed
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("import commit returned no path")?
        .to_string();
    Ok(TransferDone {
        created_path,
        chunks,
        total_chunks,
    })
}
