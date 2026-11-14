//! Add-project flow: the OS folder picker — a whole folder becomes a project
//! on this device, like a file manager's "Open folder".

use super::*;

impl Shell {
    /// The "+" affordances: the native folder picker. Picking folders adds
    /// them as projects and stays on the grouped project view — no redirect
    /// into the new space (the way file managers behave).
    pub(crate) fn open_project_folder_picker(&mut self, cx: &mut Context<Self>) {
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
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Choose project folder".into()),
        });
        self.project_picker_task = Some(cx.spawn(async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) => {
                if let Err(err) = this.update(cx, |shell, cx| {
                    for path in paths {
                        shell.create_space_at(
                            device.clone(),
                            path.to_string_lossy().into_owned(),
                            cx,
                        );
                    }
                }) {
                    tracing::debug!(error = %err, "shell closed while adding picked folders");
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "project folder picker failed");
            }
            Err(err) => {
                tracing::warn!(error = %err, "project folder picker interrupted");
            }
        }));
    }

    /// Create the space for `path` on `device`. Same (device, folder) already
    /// having a space is a no-op — the user stays where they are either way.
    pub(crate) fn create_space_at(&mut self, device: Device, path: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        // Same (device, folder) already has a space → nothing to create; the
        // engine dedupes this case too (a createSpace for a duplicate pair
        // no-ops), so creating would leave the minted id dangling.
        if self
            .state
            .read(cx)
            .spaces
            .iter()
            .any(|s| s.device_id == device.id && s.path == path)
        {
            cx.notify();
            return;
        }
        let space_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the watch frame carrying the real row replaces it
        // by id (apply_spaces re-sorts; same-id upsert is idempotent).
        let space = Space {
            id: space_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            if !s.spaces.iter().any(|existing| existing.id == space.id) {
                s.spaces.push(space);
            }
            cx.notify();
        });
        let params = serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": false,
        });
        cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                if let Err(err) = result {
                    // Roll the optimistic row back; there is no palette to
                    // surface an inline error on the native-picker path.
                    shell.state.update(cx, |s, cx| {
                        s.spaces.retain(|space| space.id != space_id);
                        cx.notify();
                    });
                    tracing::warn!(error = %err, "createSpace failed");
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}
