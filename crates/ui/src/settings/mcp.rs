//! Settings → MCP Servers: manage external MCP servers for the agents.
//!
//! The registry lives on the engine (`mcp-servers.json` in its data dir), so
//! enablement/addition is per-device. Every operation here goes through the
//! relay-forwardable RPCs so the page can manage a remote device's servers the
//! same way it manages this one.
//!
//! Secret policy: values are write-only. `ListMcpServers`/`GetMcpServer`
//! return `PublicMcpServerConfig` (headers/env elided, `has_secrets` flag
//! only), so this page never receives a raw token — it can display the
//! "••••" masked placeholder without ever holding the real value.

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};

use komet_engine::mcp::{McpTransport, PublicMcpServerConfig};
use komet_rpc::methods;

use crate::composer::ComposerInput;
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// Short transport label for the row meta line.
fn transport_label(transport: McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
        McpTransport::Sse => "sse",
    }
}

/// Write-only secrets never leave the engine; the row only shows a mask.
fn secrets_badge(has_secrets: bool) -> Option<&'static str> {
    has_secrets.then_some("••••")
}

pub struct McpServersPage {
    state: Entity<AppState>,
    servers: Loadable<Vec<PublicMcpServerConfig>>,
    /// Last refused/failed mutation, shown in the error strip.
    error: Option<String>,
    load_task: Option<Task<()>>,
    /// Server id currently running a mutation (toggle/delete) or a probe.
    busy: Option<String>,
    /// Per-server `TestMcpServer` outcome: server id -> display line.
    test_result: std::collections::HashMap<String, String>,
    editor: Option<McpEditor>,
}

struct McpEditor {
    id: Entity<ComposerInput>,
    name: Entity<ComposerInput>,
    command: Entity<ComposerInput>,
    args: Entity<ComposerInput>,
    url: Entity<ComposerInput>,
    secrets: Entity<ComposerInput>,
    transport: McpTransport,
    enabled: bool,
    always_load: bool,
}

impl McpServersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            servers: Loadable::Idle,
            error: None,
            load_task: None,
            busy: None,
            test_result: std::collections::HashMap::new(),
            editor: None,
        };
        page.load(cx);
        page
    }

    fn open_editor(&mut self, server: Option<PublicMcpServerConfig>, cx: &mut Context<Self>) {
        let id_value = server.as_ref().map(|s| s.id.clone()).unwrap_or_default();
        let name_value = server.as_ref().map(|s| s.name.clone()).unwrap_or_default();
        let command_value = server
            .as_ref()
            .and_then(|s| s.command.clone())
            .unwrap_or_default();
        let args_value = server
            .as_ref()
            .map(|s| s.args.join("\n"))
            .unwrap_or_default();
        let url_value = server
            .as_ref()
            .and_then(|s| s.url.clone())
            .unwrap_or_default();
        let secrets_placeholder = server
            .as_ref()
            .filter(|s| s.has_secrets)
            .map(|_| "Existing secrets are preserved; add KEY=VALUE to replace".to_string())
            .unwrap_or_default();
        let mut input =
            |placeholder: &'static str| cx.new(|cx| ComposerInput::new(placeholder, cx));
        let id = input("Identifier (e.g. github)");
        let name = input("Display name");
        let command = input("Command (stdio)");
        let args = input("Arguments, one per line (stdio)");
        let url = input("URL (HTTP / SSE)");
        let secrets = input("Secrets, one KEY=VALUE per line");
        id.update(cx, |input, cx| input.set_text(id_value, cx));
        name.update(cx, |input, cx| input.set_text(name_value, cx));
        command.update(cx, |input, cx| input.set_text(command_value, cx));
        args.update(cx, |input, cx| input.set_text(args_value, cx));
        url.update(cx, |input, cx| input.set_text(url_value, cx));
        secrets.update(cx, |input, cx| input.set_text(secrets_placeholder, cx));
        self.editor = Some(McpEditor {
            id,
            name,
            command,
            args,
            url,
            secrets,
            transport: server
                .as_ref()
                .map(|s| s.transport.clone())
                .unwrap_or(McpTransport::Stdio),
            enabled: server.as_ref().map(|s| s.enabled).unwrap_or(true),
            // A newly registered server should work immediately. Users can
            // turn this off in the editor when they want per-session wiring.
            always_load: server.as_ref().map(|s| s.always_load).unwrap_or(true),
        });
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        cx.notify();
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let id = editor.id.read(cx).text().trim().to_string();
        let name = editor.name.read(cx).text().trim().to_string();
        let command = editor.command.read(cx).text().trim().to_string();
        let url = editor.url.read(cx).text().trim().to_string();
        if id.is_empty() || name.is_empty() {
            self.error = Some("Identifier and display name are required.".into());
            cx.notify();
            return;
        }
        let args: Vec<String> = editor
            .args
            .read(cx)
            .text()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        let mut headers = serde_json::Map::new();
        let mut env = serde_json::Map::new();
        for line in editor.secrets.read(cx).text().lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() || value.is_empty() || value.starts_with("Existing secrets") {
                continue;
            }
            let target = if matches!(editor.transport, McpTransport::Stdio) {
                &mut env
            } else {
                &mut headers
            };
            target.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let transport = editor.transport.clone();
        let enabled = editor.enabled;
        let always_load = editor.always_load;
        self.busy = Some(id.clone());
        self.error = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({
                "id": id,
                "name": name,
                "enabled": enabled,
                "transport": transport,
                "args": args,
                "headers": headers,
                "env": env,
                "alwaysLoad": always_load,
            });
            if matches!(transport, McpTransport::Stdio) {
                params["command"] = serde_json::Value::String(command);
                params["url"] = serde_json::Value::Null;
            } else {
                params["command"] = serde_json::Value::Null;
                params["url"] = serde_json::Value::String(url);
            }
            let result = engine.client().call(methods::SAVE_MCP_SERVER, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                match result {
                    Ok(_) => {
                        page.editor = None;
                        page.load(cx);
                    }
                    Err(err) => page.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_editor(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.editor.as_ref()?;
        let transport = editor.transport.clone();
        let enabled = editor.enabled;
        let always_load = editor.always_load;
        let transport_button = |label: &'static str, value: McpTransport| {
            let selected = transport == value;
            div()
                .id(SharedString::from(format!("mcp-transport-{label}")))
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(7.0))
                .text_size(px(12.0))
                .cursor_pointer()
                .text_color(if selected {
                    theme.text
                } else {
                    theme.text_muted
                })
                .bg(if selected {
                    theme.accent.opacity(0.15)
                } else {
                    crate::theme::wash(0.03)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = this.editor.as_mut() {
                        editor.transport = value.clone();
                    }
                    cx.notify();
                }))
                .child(SharedString::from(label))
                .into_any_element()
        };
        let field = |label: &'static str, input: Entity<ComposerInput>| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(label)),
                )
                .child(popover::dialog_field(input.into_any_element()))
        };
        let is_stdio = matches!(transport, McpTransport::Stdio);
        let id = editor.id.clone();
        let name = editor.name.clone();
        let command = editor.command.clone();
        let args = editor.args.clone();
        let url = editor.url.clone();
        let secrets = editor.secrets.clone();
        Some(
            widgets::section_card(theme)
                .p(px(16.0))
                .child(widgets::row_title(
                    theme,
                    if is_stdio {
                        "Add stdio MCP server"
                    } else {
                        "Add remote MCP server"
                    },
                ))
                .child(div().mt(px(12.0)).flex().flex_row().gap(px(6.0)).children([
                    transport_button("stdio", McpTransport::Stdio),
                    transport_button("HTTP", McpTransport::Http),
                    transport_button("SSE", McpTransport::Sse),
                ]))
                .child(
                    div()
                        .mt(px(12.0))
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(field("Identifier", id))
                        .child(field("Name", name)),
                )
                .child(if is_stdio {
                    div()
                        .mt(px(10.0))
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(field("Command", command))
                        .child(field("Arguments", args))
                        .into_any_element()
                } else {
                    div()
                        .mt(px(10.0))
                        .child(field("URL", url))
                        .into_any_element()
                })
                .child(
                    div()
                        .mt(px(10.0))
                        .child(field("Secrets (one KEY=VALUE per line)", secrets)),
                )
                .child(
                    div().mt(px(10.0)).flex().flex_row().gap(px(8.0)).children([
                        div()
                            .id("mcp-editor-enabled")
                            .px(px(8.0))
                            .py(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(editor) = this.editor.as_mut() {
                                    editor.enabled = !editor.enabled;
                                }
                                cx.notify();
                            }))
                            .child(SharedString::from(if enabled {
                                "✓ Enabled"
                            } else {
                                "Disabled"
                            })),
                        div()
                            .id("mcp-editor-always-load")
                            .px(px(8.0))
                            .py(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(editor) = this.editor.as_mut() {
                                    editor.always_load = !editor.always_load;
                                }
                                cx.notify();
                            }))
                            .child(SharedString::from(if always_load {
                                "✓ Always load"
                            } else {
                                "Per-session only"
                            })),
                    ]),
                )
                .child(
                    div()
                        .mt(px(14.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            widgets::ghost_action(theme)
                                .id("mcp-editor-cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx)))
                                .child(SharedString::from("Cancel")),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("mcp-editor-save")
                                .on_click(cx.listener(|this, _, _, cx| this.save_editor(cx)))
                                .child(SharedString::from("Save")),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Deserialize the `{servers:[...]}` reply into the ready list.
    fn finish_list(&mut self, value: serde_json::Value, cx: &mut Context<Self>) {
        let Some(servers_v) = value.get("servers") else {
            self.servers = Loadable::Error("ListMcpServers reply missing `servers`".into());
            return;
        };
        self.servers = match serde_json::from_value::<Vec<PublicMcpServerConfig>>(servers_v.clone())
        {
            Ok(list) => Loadable::Ready(list),
            Err(err) => Loadable::Error(err.to_string()),
        };
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.servers = Loadable::Loading;
        self.error = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let params = serde_json::json!({});
            let result = engine
                .client()
                .call(methods::LIST_MCP_SERVERS, params)
                .await;
            this.update(cx, |page, cx| match result {
                Ok(value) => page.finish_list(value, cx),
                Err(err) => {
                    page.servers = Loadable::Error(err.to_string());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Flip the enabled flag on the engine; the reply carries the fresh public
    /// config, so the row repaints from the authoritative value in one round
    /// trip (a refused/raced toggle self-corrects).
    fn toggle(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let id = id.clone();
        self.busy = Some(id.clone());
        self.error = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let params = serde_json::json!({
                "id": id,
                "enabled": enabled,
            });
            let result = engine
                .client()
                .call(methods::SET_MCP_SERVER_ENABLED, params)
                .await;
            this.update(cx, |page, cx| {
                page.busy = None;
                match result {
                    Ok(value) => match serde_json::from_value::<PublicMcpServerConfig>(value) {
                        Ok(server) => {
                            let Loadable::Ready(list) = &page.servers else {
                                cx.notify();
                                return;
                            };
                            let mut list = list.clone();
                            for i in 0..list.len() {
                                if list[i].id == server.id {
                                    list[i] = server;
                                    break;
                                }
                            }
                            page.servers = Loadable::Ready(list);
                        }
                        Err(err) => page.error = Some(err.to_string()),
                    },
                    Err(err) => page.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Delete a server, then reload the list (a delete removes it entirely).
    fn remove(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let id = id.clone();
        self.busy = Some(id.clone());
        self.error = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "id": id });
            let result = engine
                .client()
                .call(methods::DELETE_MCP_SERVER, params)
                .await;
            this.update(cx, |page, cx| {
                page.busy = None;
                match result {
                    Ok(_) => page.load(cx),
                    Err(err) => {
                        page.error = Some(err.to_string());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// One-shot ephemeral probe (`{id}`): returns `{status, toolsCount, error?}`.
    fn test(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let id = id.clone();
        self.busy = Some(id.clone());
        self.error = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "id": id });
            let result = engine.client().call(methods::TEST_MCP_SERVER, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                match result {
                    Ok(value) => {
                        let status = value
                            .get("status")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("error");
                        let tools = value
                            .get("toolsCount")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        let error = value
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        page.test_result.insert(
                            id.clone(),
                            if status == "ready" {
                                format!("ready · {tools} tools")
                            } else if error.is_empty() {
                                "connection failed".to_string()
                            } else {
                                format!("error · {error}")
                            },
                        );
                    }
                    Err(err) => {
                        page.test_result
                            .insert(id.clone(), format!("error · {err}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }
    fn rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let theme = Theme::of(cx).clone();
        let Loadable::Ready(list) = &self.servers else {
            return Vec::new();
        };
        let test_result = &self.test_result;
        list.into_iter()
            .enumerate()
            .map(|(ix, server)| {
                let id = server.id.clone();
                let transport = transport_label(server.transport.clone());
                let result = test_result.get(id.as_str()).cloned().unwrap_or_default();
                let mut meta: Vec<gpui::AnyElement> = vec![
                    div()
                        .child(SharedString::from(transport.to_string()))
                        .into_any_element(),
                ];
                // Command/URL + always_load describe how the server connects.
                let protocol = match server.transport {
                    McpTransport::Stdio => server.command.clone().unwrap_or_default(),
                    McpTransport::Http | McpTransport::Sse => {
                        server.url.clone().unwrap_or_default()
                    }
                };
                if !protocol.is_empty() {
                    meta.push(
                        div()
                            .text_color(theme.text_muted.opacity(0.75))
                            .child(SharedString::from(protocol))
                            .into_any_element(),
                    );
                }
                if server.always_load {
                    meta.push(
                        div()
                            .text_color(theme.warning_muted.opacity(0.9))
                            .child(SharedString::from("always load"))
                            .into_any_element(),
                    );
                }
                if let Some(badge) = secrets_badge(server.has_secrets) {
                    meta.push(
                        div()
                            .text_color(theme.text_muted.opacity(0.75))
                            .child(SharedString::from(badge))
                            .into_any_element(),
                    );
                }
                // Distinct clones: each listener moves its own id (String isn't Copy),
                // and guards read `this.busy` rather than capturing a shared
                // Option<String> into several `move` closures.
                let test_id = server.id.clone();
                let toggle_id = server.id.clone();
                let delete_id = server.id.clone();
                let toggle_on = server.enabled;
                let mut row = widgets::card_row(&theme, ix == 0).id(("mcp-server-row", ix));
                row = row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, server.name.clone()))
                        .child(widgets::meta_line(&theme, meta)),
                );
                // Test connection — a throwaway probe that never launches the
                // provider; the outcome lands in the row's meta area.
                row = row.child(
                    widgets::ghost_action(&theme)
                        .id(("mcp-test", ix))
                        .hover(|s| widgets::ghost_hover(&theme, s))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.test(test_id.clone(), cx);
                        }))
                        .child(SharedString::from("Test")),
                );
                let edit_server = server.clone();
                row = row.child(
                    widgets::ghost_action(&theme)
                        .id(("mcp-edit", ix))
                        .hover(|s| widgets::ghost_hover(&theme, s))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_editor(Some(edit_server.clone()), cx);
                        }))
                        .child(SharedString::from("Edit")),
                );
                if !result.is_empty() {
                    row = row.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(result))
                            .into_any_element(),
                    );
                }
                row = row
                    .child(
                        widgets::toggle_switch(&theme, toggle_on)
                            .id(("mcp-toggle", ix))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.busy.is_none() {
                                    this.toggle(toggle_id.clone(), !toggle_on, cx);
                                }
                            })),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id(("mcp-delete", ix))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.busy.is_none() {
                                    this.remove(delete_id.clone(), cx);
                                }
                            }))
                            .child(SharedString::from("Delete")),
                    );
                row.into_any_element()
            })
            .collect()
    }
}
impl Render for McpServersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let editor = self.render_editor(&theme, cx);
        let body: gpui::AnyElement = match &self.servers {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "mcp-servers-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => {
                let message = message.clone();
                div()
                    .child(widgets::error_strip(&theme, message))
                    .child(
                        widgets::ghost_action(&theme)
                            .id("mcp-servers-retry")
                            .mt(px(8.0))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.load(cx);
                                cx.notify();
                            }))
                            .child(SharedString::from("Retry")),
                    )
                    .into_any_element()
            }
            Loadable::Ready(list) if list.is_empty() => div()
                .child(widgets::page_subtitle(
                    &theme,
                    "No MCP servers yet — register one to expose its tools to \
                         the agents.",
                ))
                .into_any_element(),
            Loadable::Ready(_) => {
                let rows = self.rows(cx);
                widgets::section_card(&theme)
                    .children(rows)
                    .into_any_element()
            }
        };
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message).into_any_element());

        div()
            .id("mcp-servers-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .justify_between()
                            .child(widgets::page_header(&theme, "MCP Servers", None))
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("mcp-add-server")
                                    .hover(|s| widgets::ghost_hover(&theme, s))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_editor(None, cx);
                                    }))
                                    .child(SharedString::from("Add server")),
                            ),
                    )
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Register external MCP servers (GitHub, filesystem, etc.) \
                             so Claude, OpenCode, Grok, Hermes, and Pi can use their \
                             tools. Secrets are write-only: the UI only ever shows a \
                             masked placeholder.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .children(error)
                    .children(editor)
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parsing the ListMcpServers reply must never surface header/env values,
    /// only the `has_secrets` flag (task-8 security contract).
    #[test]
    fn list_reply_masks_secrets() {
        let reply = serde_json::json!({
            "servers": [
                {
                    "id": "gh",
                    "name": "GitHub",
                    "enabled": true,
                    "transport": "stdio",
                    "command": "npx",
                    "args": ["mcp-github"],
                    "url": serde_json::Value::Null,
                    "alwaysLoad": false,
                    "hasSecrets": true,
                },
            ],
        });
        let servers_v = reply.get("servers").unwrap();
        let list = serde_json::from_value::<Vec<PublicMcpServerConfig>>(servers_v.clone()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].has_secrets);
        // Serializing the public view must not emit any secret-shaped value.
        let json = serde_json::to_string(&list[0]).unwrap();
        assert!(!json.contains("Authorization"));
        assert!(!json.contains("token"));
    }

    /// The transport label mapping used for row meta lines.
    #[test]
    fn transport_labels() {
        assert_eq!(transport_label(McpTransport::Stdio), "stdio");
        assert_eq!(transport_label(McpTransport::Http), "http");
        assert_eq!(transport_label(McpTransport::Sse), "sse");
    }

    #[test]
    fn secrets_badge_is_masked_placeholder() {
        assert_eq!(secrets_badge(true), Some("••••"));
        assert_eq!(secrets_badge(false), None);
    }
}
