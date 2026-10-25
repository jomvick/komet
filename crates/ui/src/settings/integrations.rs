use gpui::{Context, Entity, Render, SharedString, Task, Window, div, prelude::*, px};
use komet_rpc::methods;
use serde::Deserialize;

use crate::composer::ComposerInput;
use crate::icons::{self, icon};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const COMPOSIO_HOME_URL: &str = "https://composio.dev";
const COMPOSIO_DASHBOARD_URL: &str = "https://dashboard.composio.dev/";

const FEATURED_APPS: [&str; 12] = [
    "Gmail",
    "Google Calendar",
    "Google Drive",
    "Slack",
    "Notion",
    "Linear",
    "PostHog",
    "Sentry",
    "Stripe",
    "Supabase",
    "HubSpot",
    "Outlook",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntegrationApp {
    slug: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    no_auth: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectedAccount {
    id: String,
    toolkit_slug: String,
    status: String,
    label: Option<String>,
}

pub struct IntegrationsPage {
    state: Entity<AppState>,
    key_input: Entity<ComposerInput>,
    search_input: Entity<ComposerInput>,
    configured: Option<bool>,
    apps: Vec<IntegrationApp>,
    accounts: Vec<ConnectedAccount>,
    busy: bool,
    status: Option<SharedString>,
    task: Option<Task<()>>,
}

impl IntegrationsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            key_input: cx
                .new(|cx| ComposerInput::with_context("Composio Platform API key", "Composer", cx)),
            search_input: cx
                .new(|cx| ComposerInput::with_context("Search integrations", "Composer", cx)),
            configured: None,
            apps: Vec::new(),
            accounts: Vec::new(),
            busy: false,
            status: None,
            task: None,
        };
        page.refresh(cx);
        page
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.status = Some("The local engine is unavailable.".into());
            return;
        };
        self.busy = true;
        self.status = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result: Result<_, String> = async {
                let configured = engine
                    .client()
                    .call(methods::GET_COMPOSIO_STATUS, serde_json::json!({}))
                    .await
                    .map_err(|error| error.to_string())?;
                let configured = configured
                    .get("configured")
                    .and_then(serde_json::Value::as_bool)
                    .ok_or_else(|| "Invalid Composio status response".to_string())?;
                if !configured {
                    return Ok((false, Vec::new(), Vec::new()));
                }
                let apps = engine
                    .client()
                    .call(
                        methods::SEARCH_COMPOSIO_INTEGRATIONS,
                        serde_json::json!({ "query": "" }),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let accounts = engine
                    .client()
                    .call(methods::LIST_COMPOSIO_ACCOUNTS, serde_json::json!({}))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok((
                    true,
                    parse_apps(apps)?,
                    serde_json::from_value::<Vec<ConnectedAccount>>(
                        accounts
                            .get("accounts")
                            .cloned()
                            .ok_or_else(|| "Missing Composio accounts in response".to_string())?,
                    )
                    .map_err(|error| error.to_string())?,
                ))
            }
            .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok((configured, apps, accounts)) => {
                        page.configured = Some(configured);
                        if configured {
                            page.apps = featured_apps(apps);
                            page.accounts = accounts;
                        } else {
                            page.apps.clear();
                            page.accounts.clear();
                        }
                    }
                    Err(error) => page.status = Some(error.into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let api_key = self.key_input.read(cx).text().trim().to_owned();
        if api_key.is_empty() {
            self.status = Some("Enter your Composio Platform API key.".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.status = Some("The local engine is unavailable.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.status = Some("Validating the Composio API key…".into());
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_COMPOSIO_API_KEY,
                    serde_json::json!({ "apiKey": api_key }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(_) => {
                        page.key_input
                            .update(cx, |input, cx| input.set_text(String::new(), cx));
                        page.refresh(cx);
                        page.status =
                            Some("Composio API key saved securely on this device.".into());
                    }
                    Err(error) => page.status = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove_key(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.status = Some("The local engine is unavailable.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::DELETE_COMPOSIO_API_KEY, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(_) => {
                        page.configured = Some(false);
                        page.apps.clear();
                        page.accounts.clear();
                        page.status = Some(
                            "The API key was removed from this device. Composio still retains connected accounts."
                                .into(),
                        );
                    }
                    Err(error) => page.status = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).text().trim().to_owned();
        let show_featured = query.is_empty();
        if query.chars().count() > 80 {
            self.status = Some("Search is limited to 80 characters.".into());
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
                    methods::SEARCH_COMPOSIO_INTEGRATIONS,
                    serde_json::json!({ "query": query }),
                )
                .await
                .map_err(|error| error.to_string())
                .and_then(parse_apps);
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(apps) => {
                        page.apps = if show_featured {
                            featured_apps(apps)
                        } else {
                            apps
                        };
                        page.status = if page.apps.is_empty() {
                            Some("No integrations matched your search.".into())
                        } else {
                            None
                        };
                    }
                    Err(error) => page.status = Some(error.into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn connect(&mut self, toolkit_slug: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.status = Some("The local engine is unavailable.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.status = Some("Preparing secure authorization…".into());
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CONNECT_COMPOSIO_INTEGRATION,
                    serde_json::json!({ "toolkitSlug": toolkit_slug }),
                )
                .await
                .and_then(|value| {
                    value
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            komet_rpc::RpcError::Failed(
                                "Composio response did not include an authorization URL".into(),
                            )
                        })
                });
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(url) => {
                        cx.open_url(&url);
                        page.status = Some(
                            "Finish authorization in your browser, then refresh the integrations list."
                                .into(),
                        );
                    }
                    Err(error) => page.status = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn disconnect(&mut self, account_id: String, cx: &mut Context<Self>) {
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
                    methods::DISCONNECT_COMPOSIO_ACCOUNT,
                    serde_json::json!({ "accountId": account_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(_) => {
                        page.refresh(cx);
                        page.status = Some("Connected account removed.".into());
                    }
                    Err(error) => page.status = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

fn parse_apps(value: serde_json::Value) -> Result<Vec<IntegrationApp>, String> {
    serde_json::from_value(
        value
            .get("integrations")
            .cloned()
            .ok_or_else(|| "Missing Composio integrations in response".to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn featured_apps(apps: Vec<IntegrationApp>) -> Vec<IntegrationApp> {
    FEATURED_APPS
        .iter()
        .filter_map(|name| {
            apps.iter()
                .find(|app| app.name.eq_ignore_ascii_case(name))
                .cloned()
        })
        .collect()
}

impl Render for IntegrationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let configured = self.configured == Some(true);
        let busy = self.busy;
        let apps = self.apps.clone();
        let accounts = self.accounts.clone();
        let about_composio_url = COMPOSIO_HOME_URL.to_string();
        let dashboard_url = COMPOSIO_DASHBOARD_URL.to_string();
        div()
            .id("integrations-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.0))
                            .child(
                                div()
                                    .size(px(48.0))
                                    .rounded(px(14.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.surface_card)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        icon(icons::COMPOSIO_MARK)
                                            .size(px(30.0))
                                            .text_color(theme.text),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .child(
                                        div()
                                            .text_size(px(23.0))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child("Composio integrations"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .text_color(theme.text_muted)
                                            .child("Bring the apps you use into your Komet workspace."),
                                    ),
                            )
                    )
                    .when(!configured, |page| {
                        page.child(
                            widgets::section_card(&theme)
                                .child(
                                    div()
                                        .px(px(16.0))
                                        .py(px(18.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(13.0))
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(theme.accent)
                                                .child("GET STARTED  /  01"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(18.0))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(theme.text)
                                                .child("Connect your Composio project"),
                                        )
                                        .child(
                                            div()
                                                .max_w(px(560.0))
                                                .text_size(px(12.0))
                                                .text_color(theme.text_muted)
                                                .line_height(px(18.0))
                                                .child("Composio lets your agent work with services like Gmail, Slack and Notion. Add a project API key to browse apps and connect your accounts."),
                                        )
                                        .child(
                                            div()
                                                .mt(px(4.0))
                                                .flex()
                                                .items_center()
                                                .gap(px(7.0))
                                                .child(widgets::field_label(
                                                    &theme,
                                                    "Composio Platform API key",
                                                ))
                                                .child(
                                                    div()
                                                        .id("composio-get-key")
                                                        .text_size(px(11.0))
                                                        .text_color(theme.accent)
                                                        .cursor_pointer()
                                                        .hover(|style| {
                                                            style.text_color(theme.text)
                                                        })
                                                        .on_click(cx.listener(
                                                            move |_, _, _, cx| {
                                                                cx.open_url(&dashboard_url);
                                                            },
                                                        ))
                                                        .child("Get a key ↗"),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .h(px(56.0))
                                                .w_full()
                                                .rounded(px(9.0))
                                                .border_1()
                                                .border_color(theme.border)
                                                .bg(theme.bg)
                                                .child(self.key_input.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(7.0))
                                                .text_size(px(11.0))
                                                .text_color(theme.text_muted)
                                                .child(
                                                    icon(icons::SHIELD)
                                                        .size(px(14.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child("Validated and stored in this device’s OS credential store. Never synced."),
                                        )
                                        .child(
                                            div()
                                                .mt(px(2.0))
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .gap(px(12.0))
                                                .child(
                                                    div()
                                                        .id("composio-about")
                                                        .text_size(px(12.0))
                                                        .text_color(theme.text_muted)
                                                        .cursor_pointer()
                                                        .hover(|style| {
                                                            style.text_color(theme.text)
                                                        })
                                                        .on_click(cx.listener(
                                                            move |_, _, _, cx| {
                                                                cx.open_url(
                                                                    &about_composio_url,
                                                                );
                                                            },
                                                        ))
                                                        .child("New to Composio? Learn how it works ↗"),
                                                )
                                                .child(
                                                    div()
                                                        .id("composio-save-key")
                                                        .px(px(16.0))
                                                        .py(px(9.0))
                                                        .rounded(px(8.0))
                                                        .bg(theme.accent)
                                                        .text_color(theme.on_accent)
                                                        .cursor_pointer()
                                                        .when(busy, |button| {
                                                            button.opacity(0.5)
                                                        })
                                                        .on_click(cx.listener(
                                                            |this, _, _, cx| {
                                                                if !this.busy {
                                                                    this.save_key(cx)
                                                                }
                                                            },
                                                        ))
                                                        .child(if busy {
                                                            "Validating key…"
                                                        } else {
                                                            "Continue"
                                                        }),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .mt(px(7.0))
                                                .border_t_1()
                                                .border_color(theme.border)
                                                .pt(px(14.0))
                                                .flex()
                                                .flex_wrap()
                                                .gap(px(16.0))
                                                .children(
                                                    [
                                                        ("01", "Add your project key"),
                                                        ("02", "Choose apps to connect"),
                                                        ("03", "Use them with your agent"),
                                                    ]
                                                    .into_iter()
                                                    .map(|(number, label)| {
                                                        div()
                                                            .flex()
                                                            .items_center()
                                                            .gap(px(7.0))
                                                            .child(
                                                                div()
                                                                    .text_size(px(10.0))
                                                                    .font_weight(
                                                                        gpui::FontWeight::SEMIBOLD,
                                                                    )
                                                                    .text_color(theme.accent)
                                                                    .child(number),
                                                            )
                                                            .child(
                                                                div()
                                                                    .text_size(px(11.0))
                                                                    .text_color(theme.text_muted)
                                                                    .child(label),
                                                            )
                                                    }),
                                                ),
                                        ),
                                ),
                        )
                    })
                    .when(configured, |page| {
                        page.child(
                            div()
                                .mt(px(26.0))
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .h(px(52.0))
                                        .rounded(px(9.0))
                                        .bg(theme.surface_card)
                                        .px(px(12.0))
                                        .child(self.search_input.clone()),
                                )
                                .child(
                                    div()
                                        .id("integrations-search")
                                        .px(px(14.0))
                                        .py(px(10.0))
                                        .rounded(px(8.0))
                                        .bg(theme.surface_raised)
                                        .text_color(theme.text)
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if !this.busy {
                                                this.search(cx)
                                            }
                                        }))
                                        .child("Search"),
                                )
                                .child(
                                    div()
                                        .id("integrations-refresh")
                                        .px(px(12.0))
                                        .py(px(10.0))
                                        .rounded(px(8.0))
                                        .bg(theme.surface_raised)
                                        .text_color(theme.text)
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if !this.busy {
                                                this.refresh(cx)
                                            }
                                        }))
                                        .child("Refresh"),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(14.0))
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(12.0))
                                .children(apps.into_iter().map(|app| {
                                    let app_accounts: Vec<_> = accounts
                                        .iter()
                                        .filter(|account| account.toolkit_slug == app.slug)
                                        .cloned()
                                        .collect();
                                    let slug = app.slug.clone();
                                    div()
                                        .w(px(224.0))
                                        .rounded(px(10.0))
                                        .bg(theme.surface_card)
                                        .border_1()
                                        .border_color(theme.border)
                                        .p(px(10.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(10.0))
                                                .child(
                                                    div()
                                                        .size(px(32.0))
                                                        .rounded(px(7.0))
                                                        .bg(theme.element_active)
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .text_color(theme.text)
                                                        .child(
                                                            app.name
                                                                .chars()
                                                                .next()
                                                                .unwrap_or('?')
                                                                .to_ascii_uppercase()
                                                                .to_string(),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_size(px(13.0))
                                                        .text_color(theme.text)
                                                        .child(app.name.clone()),
                                                )
                                                .when(app.no_auth, |row| {
                                                    row.child(
                                                        div()
                                                            .text_size(px(11.0))
                                                            .text_color(theme.text_muted)
                                                            .child("No auth"),
                                                    )
                                                })
                                                .when(!app.no_auth, |row| {
                                                    row.child(
                                                        div()
                                                            .id(SharedString::from(format!(
                                                                "connect-{}",
                                                                app.slug
                                                            )))
                                                            .px(px(4.0))
                                                            .text_size(px(20.0))
                                                            .text_color(theme.text_muted)
                                                            .cursor_pointer()
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    if !this.busy {
                                                                        this.connect(
                                                                            slug.clone(),
                                                                            cx,
                                                                        )
                                                                    }
                                                                },
                                                            ))
                                                            .child("+"),
                                                    )
                                                }),
                                        )
                                        .when(!app.description.is_empty(), |card| {
                                            card.child(
                                                div()
                                                    .text_size(px(11.0))
                                                    .text_color(theme.text_muted)
                                                    .line_height(px(15.0))
                                                    .child(app.description.clone()),
                                            )
                                        })
                                        .children(app_accounts.into_iter().map(|account| {
                                            let account_id = account.id.clone();
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(8.0))
                                                .border_t_1()
                                                .border_color(theme.border)
                                                .pt(px(7.0))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_size(px(11.0))
                                                        .text_color(theme.text_muted)
                                                        .child(
                                                            account.label.unwrap_or_else(|| {
                                                                account.status.clone()
                                                            }),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .id(SharedString::from(format!(
                                                            "disconnect-{}",
                                                            account.id
                                                        )))
                                                        .text_size(px(11.0))
                                                        .text_color(theme.text_muted)
                                                        .cursor_pointer()
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                if !this.busy {
                                                                    this.disconnect(
                                                                        account_id.clone(),
                                                                        cx,
                                                                    )
                                                                }
                                                            },
                                                        ))
                                                        .child("Disconnect"),
                                                )
                                        }))
                                })),
                        )
                        .child(
                            div()
                                .mt(px(16.0))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(theme.text_muted)
                                        .child("Connections are stored in your Composio project and scoped to this Komet device."),
                                )
                                .child(
                                    div()
                                        .id("composio-remove-key")
                                        .text_size(px(12.0))
                                        .text_color(theme.text_muted)
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if !this.busy {
                                                this.remove_key(cx)
                                            }
                                        }))
                                        .child("Remove API key"),
                                ),
                        )
                    })
                    .when_some(self.status.clone(), |page, status| {
                        page.child(
                            div()
                                .mt(px(12.0))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child(status),
                        )
                    }),
            )
    }
}
