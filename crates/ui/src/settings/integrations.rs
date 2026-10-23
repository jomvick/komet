use gpui::{Context, Entity, Render, SharedString, Task, Window, div, prelude::*, px};
use komet_rpc::methods;

use crate::composer::ComposerInput;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const COMPOSIO_ID: &str = "composio";
const COMPOSIO_URL: &str = "https://connect.composio.dev/mcp";

pub struct IntegrationsPage {
    state: Entity<AppState>,
    key_input: Entity<ComposerInput>,
    connected: Option<bool>,
    busy: bool,
    status: Option<SharedString>,
    task: Option<Task<()>>,
}

impl IntegrationsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            key_input: cx.new(|cx| {
                ComposerInput::with_context("Paste your Composio consumer API key", "Composer", cx)
            }),
            connected: None,
            busy: false,
            status: None,
            task: None,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_MCP_SERVERS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.connected = result.ok().and_then(|value| {
                    let servers = value
                        .get("servers")
                        .and_then(serde_json::Value::as_array)
                        .or_else(|| value.as_array())?;
                    Some(servers.iter().any(|server| {
                        server.get("id").and_then(serde_json::Value::as_str) == Some(COMPOSIO_ID)
                            && server
                                .get("hasSecrets")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false)
                    }))
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let key = self.key_input.read(cx).text().trim().to_owned();
        if key.is_empty() {
            self.status = Some("Enter a Composio consumer API key to connect.".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.status = Some("The local engine is unavailable.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.status = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_MCP_SERVER,
                    serde_json::json!({
                        "id": COMPOSIO_ID,
                        "name": "Composio",
                        "enabled": true,
                        "transport": "http",
                        "url": COMPOSIO_URL,
                        "headers": { "x-consumer-api-key": key },
                        "env": {},
                        "alwaysLoad": true
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(_) => {
                        page.connected = Some(true);
                        page.status = Some("Composio is connected on this device.".into());
                        page.key_input
                            .update(cx, |input, cx| input.set_text(String::new(), cx));
                    }
                    Err(_) => {
                        page.status = Some(
                            "Could not save the credential to this device’s secure credential store."
                                .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::DELETE_MCP_SERVER,
                    serde_json::json!({ "id": COMPOSIO_ID }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(_) => {
                        page.connected = Some(false);
                        page.status = Some("Composio was removed from this device.".into());
                    }
                    Err(_) => {
                        page.status = Some("Could not remove the Composio credential.".into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for IntegrationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let connected = self.connected == Some(true);
        let save_label = if self.busy {
            "Saving…"
        } else {
            "Connect & enable"
        };
        widgets::page_column()
            .child(widgets::page_header(&theme, "Integrations", None))
            .child(widgets::page_subtitle(
                &theme,
                "Connect Composio once and make its MCP tools available to compatible agents.",
            ))
            .child(
                widgets::section_card(&theme)
                    .child(
                        div()
                            .px(px(16.0))
                            .py(px(14.0))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .text_color(theme.text)
                                    .child("Composio Connect"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.text_muted)
                                    .child("Paste the consumer API key from Composio AI Clients. Connecting makes Composio available in compatible chats. Komet stores the key in this device’s OS credential store; it is not synchronized or returned by MCP settings RPCs."),
                            )
                            .child(
                                div()
                                    .h(px(76.0))
                                    .w_full()
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .child(self.key_input.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(
                                        div()
                                            .id("composio-save")
                                            .px(px(12.0))
                                            .py(px(8.0))
                                            .rounded(px(8.0))
                                            .bg(theme.accent)
                                            .text_color(theme.text)
                                            .cursor_pointer()
                                            .on_click(cx.listener(|this, _, _, cx| this.save(cx)))
                                            .child(save_label),
                                    )
                                    .when(connected, |el| {
                                        el.child(
                                        div()
                                            .id("composio-disconnect")
                                            .px(px(12.0))
                                                .py(px(8.0))
                                                .rounded(px(8.0))
                                                .border_1()
                                                .border_color(theme.border)
                                                .cursor_pointer()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.disconnect(cx)
                                                }))
                                                .child("Disconnect"),
                                        )
                                    }),
                            )
                            .when_some(self.status.clone(), |el, status| {
                                el.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.text_muted)
                                        .child(status),
                                )
                            }),
                    ),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Composio Connect uses https://connect.composio.dev/mcp. Composio may open a browser authorization for each app the first time; those connections persist in Composio. Available in v1 to OpenCode, Grok, Hermes, Pi, and Cline; Claude is excluded to keep credentials out of process arguments, and Codex, Cursor, and Antigravity are not supported. Komet does not mediate external MCP actions for per-action confirmation; review tool calls before using connected accounts.",
            ))
    }
}
