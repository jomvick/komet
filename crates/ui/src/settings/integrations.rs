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
    status_is_error: bool,
    task: Option<Task<()>>,
}

fn project_key_kind_error(api_key: &str) -> Option<&'static str> {
    if api_key.starts_with("ck_") {
        Some(
            "This looks like a Composio consumer key. Use a Platform project API key from the project's API Keys settings.",
        )
    } else if api_key.starts_with("uak_") {
        Some(
            "This looks like a Composio user API key. This screen requires a Platform project API key from the project's API Keys settings.",
        )
    } else {
        None
    }
}

fn project_key_validation_error(error: &str) -> String {
    if error.contains("401") {
        "Composio rejected this key (HTTP 401). Check that it is an active Platform project API key for an accessible project. Consumer and user API keys are not accepted here.".into()
    } else {
        error.to_owned()
    }
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
            status_is_error: false,
            task: None,
        };
        page.refresh(cx);
        page
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            return;
        };
        self.busy = true;
        self.status = None;
        self.status_is_error = false;
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
                    Err(error) => page.set_status(error, true),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let api_key = self.key_input.read(cx).text().trim().to_owned();
        if api_key.is_empty() {
            self.set_status("Enter your Composio Platform API key.", true);
            cx.notify();
            return;
        }
        if let Some(message) = project_key_kind_error(&api_key) {
            self.set_status(message, true);
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            cx.notify();
            return;
        };
        self.busy = true;
        self.set_status("Validating the Composio API key…", false);
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
                        page.set_status("Composio API key saved securely on this device.", false);
                    }
                    Err(error) => {
                        page.set_status(project_key_validation_error(&error.to_string()), true)
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove_key(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
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
                        page.set_status(
                            "The API key was removed from this device. Composio still retains connected accounts.",
                            false,
                        );
                    }
                    Err(error) => page.set_status(error.to_string(), true),
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
            self.set_status("Search is limited to 80 characters.", true);
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            cx.notify();
            return;
        };
        self.busy = true;
        self.status = None;
        self.status_is_error = false;
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
                        page.status_is_error = false;
                    }
                    Err(error) => page.set_status(error, true),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn connect(&mut self, toolkit_slug: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            cx.notify();
            return;
        };
        self.busy = true;
        self.set_status("Preparing secure authorization…", false);
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
                        page.set_status(
                            "Finish authorization in your browser, then refresh the integrations list.",
                            false,
                        );
                    }
                    Err(error) => page.set_status(error.to_string(), true),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn disconnect(&mut self, account_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            cx.notify();
            return;
        };
        self.busy = true;
        self.status = None;
        self.status_is_error = false;
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
                        page.set_status("Connected account removed.", false);
                    }
                    Err(error) => page.set_status(error.to_string(), true),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_status(&mut self, message: impl Into<SharedString>, is_error: bool) {
        self.status = Some(message.into());
        self.status_is_error = is_error;
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

fn integration_logo(name: &str) -> Option<(&'static str, gpui::Hsla)> {
    let (path, color) = match name.to_ascii_lowercase().as_str() {
        "gmail" => (crate::icons::GMAIL_MARK, 0xEA4335),
        "google calendar" => (crate::icons::GOOGLE_CALENDAR_MARK, 0x4285F4),
        "google drive" => (crate::icons::GOOGLE_DRIVE_MARK, 0x0F9D58),
        "slack" => (crate::icons::SLACK_MARK, 0xE01E5A),
        "notion" => (crate::icons::NOTION_MARK, 0xFFFFFF),
        "linear" => (crate::icons::LINEAR_MARK, 0x5E6AD2),
        "posthog" => (crate::icons::POSTHOG_MARK, 0xF54E00),
        "sentry" => (crate::icons::SENTRY_MARK, 0xA08BFA),
        "stripe" => (crate::icons::STRIPE_MARK, 0x635BFF),
        "supabase" => (crate::icons::SUPABASE_MARK, 0x3FCF8E),
        "hubspot" => (crate::icons::HUBSPOT_MARK, 0xFF7A59),
        "outlook" => (crate::icons::MICROSOFT_OUTLOOK_MARK, 0x0078D4),
        _ => return None,
    };
    Some((path, gpui::rgb(color).into()))
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
        let status_is_error = self.status_is_error;
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
                                                .child("Use a Platform project key (ak_…). Consumer (ck_) and user (uak_) keys are not accepted."),
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
                                    let logo_icon: gpui::AnyElement =
                                        if let Some((path, color)) = integration_logo(&app.name) {
                                            crate::icons::icon(path)
                                                .size(px(18.0))
                                                .text_color(color)
                                                .into_any_element()
                                        } else {
                                            div()
                                                .text_color(theme.text)
                                                .child(
                                                    app.name
                                                        .chars()
                                                        .next()
                                                        .unwrap_or('?')
                                                        .to_ascii_uppercase()
                                                        .to_string(),
                                                )
                                                .into_any_element()
                                        };
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
                                                        .child(logo_icon),
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
                                .text_color(if status_is_error {
                                    theme.danger
                                } else {
                                    theme.text_muted
                                })
                                .child(status),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn featured_integrations_use_brand_marks() {
        for name in FEATURED_APPS {
            assert!(integration_logo(name).is_some(), "missing logo for {name}");
        }
    }

    #[test]
    fn rejects_composio_keys_for_the_wrong_authentication_surface() {
        assert!(project_key_kind_error("ck_consumer").is_some());
        assert!(project_key_kind_error("uak_user").is_some());
        assert!(project_key_kind_error("ak_project").is_none());
    }

    #[test]
    fn explains_unauthorized_project_keys_without_exposing_them() {
        let error = project_key_validation_error(
            "Composio key validation failed: Composio API returned HTTP 401 Unauthorized",
        );
        assert!(error.contains("active Platform project API key"));
        assert!(!error.contains("secret"));
        assert_eq!(
            project_key_validation_error("network timeout"),
            "network timeout"
        );
    }
}
