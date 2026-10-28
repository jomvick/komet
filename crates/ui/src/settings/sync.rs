//! Settings → Sync: what this machine syncs to, the devices paired through a
//! `komet://pair/` link, and the pairing wizard that turns a pasted link into a
//! workspace. The link carries a token that grants shell on the far host, so it
//! is never rendered, logged, or quoted back in an error message — only
//! re-encoded into the clipboard when the user asks for it.

use futures::future::Either;
use gpui::{
    AnyElement, App, AsyncApp, ClipboardItem, Context, Entity, EntityId, EventEmitter,
    SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use std::sync::Arc;
use std::time::Duration;

use komet_proto::Environment;
use komet_rpc::{RpcClient, methods};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::loaders;
use crate::popover;
use crate::settings::devices::format_last_seen;
use crate::settings::widgets;
use crate::state::{AppState, WorkspaceStatus};
use crate::theme::Theme;

/// Each pairing round trip gets its own deadline: the engine's own probe gives up
/// at 5 s, so a call still open past this is a stuck transport, not a slow VM.
const PAIR_TIMEOUT: Duration = Duration::from_secs(20);

/// One RPC call under a deadline the UI owns: a hung transport must settle the
/// wizard, not leave its ring turning forever.
async fn deadline_call(
    client: Arc<RpcClient>,
    method: &'static str,
    params: serde_json::Value,
    cx: &AsyncApp,
) -> Result<serde_json::Value, String> {
    let call = client.call(method, params);
    let clock = cx.background_executor().timer(PAIR_TIMEOUT);
    match futures::future::select(Box::pin(call), Box::pin(clock)).await {
        Either::Left((Ok(value), _)) => Ok(value),
        Either::Left((Err(err), _)) => Err(err.to_string()),
        Either::Right((_, _)) => Err("unreachable: no answer in time".to_string()),
    }
}

/// The wizard's three visible steps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Decode,
    Verify,
    Pair,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Self::Decode => "Reading the link",
            Self::Verify => "Reaching the device",
            Self::Pair => "Opening the workspace",
        }
    }
}

/// Per-step outcome: ring while running, check when done, cross on failure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Waiting,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SyncEvent {
    /// Pairing succeeded and the user chose to work on that device now.
    OpenWorkspace(String),
}

/// True when a link's URL is plain HTTP to a host off this machine: the bearer
/// would cross a network in the clear, so pairing needs the user's consent.
pub fn needs_insecure_consent(url: &str) -> bool {
    let Some(rest) = url
        .split_once("://")
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("http"))
        .map(|(_, rest)| rest)
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host
        .strip_prefix('[')
        .and_then(|bracketed| bracketed.split_once(']'))
        .map(|(host, _)| host)
        .unwrap_or_else(|| host.split_once(':').map(|(host, _)| host).unwrap_or(host));
    let host = host.trim().to_lowercase();
    !(host.is_empty() || host == "localhost" || host == "127.0.0.1" || host == "::1")
}

/// One typed pairing failure (`env_hosts::EnvAddError::code`) turned into copy:
/// the code stays visible because it is what a support thread needs, and the hint
/// says what to do next. `invalid_link` detail is dropped — a decode error can
/// quote the payload, and the payload is the secret.
pub fn explain_failure(code: &str) -> (String, Option<&'static str>) {
    let (kind, detail) = match code.split_once(": ") {
        Some((kind, detail)) => (kind, Some(detail)),
        None => (code, None),
    };
    let hint = match kind {
        "invalid_link" => {
            Some("That is not a komet://pair/ link from this build — copy it again on the device.")
        }
        "unreachable" => Some(
            "No answer from that address. Is the device on, and is its port reachable from here?",
        ),
        "unauthorized" => {
            Some("The token was refused — the server's token changed since this link was printed.")
        }
        "version_mismatch" => Some("Update this app, or run a matching komet on the device."),
        "already_synced_here" => {
            Some("This machine already is that server's device; pair a different computer.")
        }
        "insecure_http" => {
            Some("Turn on the plain-HTTP acknowledgement below if you trust that network.")
        }
        "store" => Some("Check free disk space and that you can write in Komet's data directory."),
        "runtime" => Some(
            "That workspace's local copy is unreadable — remove the device, then pair it again.",
        ),
        _ => None,
    };
    let headline = match kind {
        "invalid_link" => "Not a valid pairing link",
        "unreachable" => "Device unreachable",
        "unauthorized" => "Device refused the token",
        "version_mismatch" => "Version mismatch",
        "already_synced_here" => "Already synced here",
        "insecure_http" => "Plain HTTP needs your acknowledgement",
        "store" => "Couldn't write the local copy",
        "runtime" => "Couldn't open that workspace",
        other => return (format!("Pairing failed ({other})"), hint),
    }
    .to_string();
    let detail = if kind == "invalid_link" { None } else { detail };
    let message = match detail {
        Some(detail) => format!("{headline} — {detail}"),
        None => headline,
    };
    (message, hint)
}

/// What the wizard shows before touching the network: the decoded identity.
#[derive(Clone)]
struct Preview {
    label: String,
    url: String,
}

struct RemoveDialog {
    env: Environment,
    purge: bool,
}

pub struct SyncPage {
    state: Entity<AppState>,
    /// The wizard's card is up (it replaces the device list's "Add device").
    adding: bool,
    link: Entity<ComposerInput>,
    steps: [(Step, Mark); 3],
    preview: Option<Preview>,
    failure: Option<SharedString>,
    failure_hint: Option<&'static str>,
    /// Consent for a plain-HTTP link that is not loopback.
    insecure_ok: bool,
    connected: Option<Environment>,
    /// Bumped per attempt; a late reply from a cancelled attempt must not settle
    /// the wizard that the next attempt is now driving.
    attempt: u64,
    task: Option<Task<()>>,
    remove: Option<RemoveDialog>,
    remove_task: Option<Task<()>>,
    copied: Option<String>,
    /// Which device's copy just failed. The link comes back from the engine, so
    /// a dead engine is the one way this can fail — the button says so instead
    /// of looking like it worked.
    copy_failed: Option<String>,
    copy_task: Option<Task<()>>,
    error: Option<SharedString>,
    _link_events: Subscription,
    _observe: Subscription,
}

impl EventEmitter<SyncEvent> for SyncPage {}

impl SyncPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let link = cx.new(|cx| ComposerInput::new("komet://pair/…", cx));
        let _link_events = cx.subscribe(&link, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) && !this.busy() {
                this.start_pair(cx);
            }
        });
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            adding: false,
            link,
            steps: [
                (Step::Decode, Mark::Waiting),
                (Step::Verify, Mark::Waiting),
                (Step::Pair, Mark::Waiting),
            ],
            preview: None,
            failure: None,
            failure_hint: None,
            insecure_ok: false,
            connected: None,
            attempt: 0,
            task: None,
            remove: None,
            remove_task: None,
            copied: None,
            copy_failed: None,
            copy_task: None,
            error: None,
            _link_events,
            _observe: observe,
        }
    }

    fn busy(&self) -> bool {
        self.steps.iter().any(|(_, mark)| *mark == Mark::Running)
    }

    fn set_mark(&mut self, step: Step, mark: Mark) {
        for (candidate, current) in self.steps.iter_mut() {
            if *candidate == step {
                *current = mark;
            }
        }
    }

    fn rest() -> [(Step, Mark); 3] {
        [
            (Step::Decode, Mark::Waiting),
            (Step::Verify, Mark::Waiting),
            (Step::Pair, Mark::Waiting),
        ]
    }

    fn open_wizard(&mut self, cx: &mut Context<Self>) {
        self.adding = true;
        self.steps = Self::rest();
        self.failure = None;
        self.failure_hint = None;
        self.preview = None;
        self.connected = None;
        cx.notify();
    }

    /// Close the card, drop any in-flight attempt (its replies are stale by `gen`).
    fn cancel_wizard(&mut self, cx: &mut Context<Self>) {
        self.task = None;
        self.attempt += 1;
        self.adding = false;
        self.steps = Self::rest();
        self.preview = None;
        self.failure = None;
        self.failure_hint = None;
        self.connected = None;
        self.link
            .update(cx, |link, cx| link.set_text(String::new(), cx));
        cx.notify();
    }

    /// Decode locally → `PreviewEnvironment` → `AddEnvironment`, one mark per step.
    fn start_pair(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let raw = self.link.read(cx).text().trim().to_string();
        self.connected = None;
        self.failure = None;
        self.failure_hint = None;
        self.preview = None;

        // Decoding happens here rather than over RPC: a bad paste answers
        // instantly, and the host is known before consent is even asked for.
        let Ok(payload) = komet_proto::decode_pair_link(&raw) else {
            self.steps = Self::rest();
            self.set_mark(Step::Decode, Mark::Failed);
            self.fail(Step::Decode, "invalid_link".to_string(), cx);
            return;
        };
        if needs_insecure_consent(&payload.url) && !self.insecure_ok {
            self.steps = Self::rest();
            self.fail(Step::Decode, "insecure_http".to_string(), cx);
            return;
        }
        self.steps = [
            (Step::Decode, Mark::Done),
            (Step::Verify, Mark::Running),
            (Step::Pair, Mark::Waiting),
        ];
        self.preview = Some(Preview {
            label: payload.label.clone(),
            url: payload.url.clone(),
        });
        cx.notify();

        let Some(engine) = self.state.read(cx).local_engine().cloned() else {
            self.fail(Step::Verify, "runtime: komet is still starting".into(), cx);
            return;
        };
        let client = engine.client_arc();
        self.attempt += 1;
        let attempt = self.attempt;
        self.task = Some(cx.spawn(async move |this, cx| {
            let probe = deadline_call(
                client.clone(),
                methods::PREVIEW_ENVIRONMENT,
                serde_json::json!({ "link": &raw }),
                cx,
            )
            .await;
            let ok = this
                .update(cx, |page, cx| {
                    if page.attempt != attempt {
                        return false;
                    }
                    match probe {
                        Err(message) => {
                            page.fail(Step::Verify, message, cx);
                            false
                        }
                        Ok(_) => {
                            page.set_mark(Step::Verify, Mark::Done);
                            page.set_mark(Step::Pair, Mark::Running);
                            cx.notify();
                            true
                        }
                    }
                })
                .unwrap_or(false);
            if !ok {
                return;
            }

            let added = deadline_call(
                client,
                methods::ADD_ENVIRONMENT,
                serde_json::json!({ "link": &raw }),
                cx,
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<Environment>(value)
                    .map_err(|err| format!("runtime: {err}"))
            });
            this.update(cx, |page, cx| {
                if page.attempt != attempt {
                    return;
                }
                match added {
                    Ok(env) => {
                        page.set_mark(Step::Pair, Mark::Done);
                        page.connected = Some(env);
                        cx.notify();
                    }
                    Err(message) => page.fail(Step::Pair, message, cx),
                }
            })
            .ok();
        }));
    }

    /// Mark the step failed and stop the rings; the wizard card stays open so the
    /// user can fix the paste and retry without starting over.
    fn fail(&mut self, step: Step, code: String, cx: &mut Context<Self>) {
        self.set_mark(step, Mark::Failed);
        for (_, mark) in self.steps.iter_mut() {
            if *mark == Mark::Running {
                *mark = Mark::Waiting;
            }
        }
        let (message, hint) = explain_failure(&code);
        self.failure = Some(message.into());
        self.failure_hint = hint;
        self.task = None;
        cx.notify();
    }

    /// Copy one device's pairing link. The bearer is not in the rows this page
    /// lists (every environment reply comes back redacted), so the link is
    /// re-encoded by the engine and fetched: a link built from a row here would
    /// carry an empty token and fail at paste time.
    fn copy_link(&mut self, env: &Environment, cx: &mut Context<Self>) {
        self.copy_failed = None;
        let Some(engine) = self.state.read(cx).local_engine().cloned() else {
            self.copy_failed = Some(env.id.clone());
            cx.notify();
            return;
        };
        let client = engine.client_arc();
        let id = env.id.clone();
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            let answer = deadline_call(
                client,
                methods::ENVIRONMENT_LINK,
                serde_json::json!({ "id": &id }),
                cx,
            )
            .await;
            let link = answer.ok().and_then(|value| {
                value
                    .get("link")
                    .and_then(|link| link.as_str())
                    .map(str::to_string)
            });
            let Some(link) = link else {
                this.update(cx, |page, cx| {
                    page.copy_failed = Some(id);
                    cx.notify();
                })
                .ok();
                return;
            };
            this.update(cx, |page, cx| {
                page.copied = Some(id);
                cx.write_to_clipboard(ClipboardItem::new_string(link));
                cx.notify();
            })
            .ok();
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_remove(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.remove.take() else {
            return;
        };
        let Some(engine) = self.state.read(cx).local_engine().cloned() else {
            self.error = Some("Engine unavailable".into());
            cx.notify();
            return;
        };
        let client = engine.client_arc();
        let params = serde_json::json!({ "id": dialog.env.id, "purge": dialog.purge });
        self.error = None;
        self.remove_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = client.call(methods::REMOVE_ENVIRONMENT, params).await {
                this.update(cx, |page, cx| {
                    page.error = Some(format!("Unpair failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
        cx.notify();
    }

    fn render_remove_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.remove.as_ref()?;
        let purge = dialog.purge;
        let name = dialog.env.label.clone();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Unpair this device"))
            .child(
                div()
                    .mt(px(8.0))
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!(
                        "{name} keeps its own data — unpairing only removes it from this app."
                    ))),
            )
            .child(
                div()
                    .mt(px(14.0))
                    .id("purge-local-copy")
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(10.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(dialog) = this.remove.as_mut() {
                            dialog.purge = !dialog.purge;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .size(px(16.0))
                            .mt(px(1.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if purge { theme.text } else { theme.border })
                            .bg(if purge {
                                theme.text
                            } else {
                                crate::theme::ink(0.03)
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(purge, |el| {
                                el.child(
                                    icons::icon(icons::CHECK)
                                        .size(px(11.0))
                                        .text_color(theme.on_solid),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme.text)
                                    .child("Also delete this device's local copy"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(if purge {
                                        theme.warning_muted.opacity(0.9)
                                    } else {
                                        theme.text_muted.opacity(0.7)
                                    })
                                    .child(
                                        "Chats, spaces and files mirrored here get erased. If that \
                                         server goes away later, this was the only copy.",
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "unpair-cancel", cx.entity_id())
                            .id("unpair-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.remove = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Unpair")
                            .id("unpair-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.submit_remove(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("unpair-device-dialog", viewport, card))
    }

    fn step_row(
        step: Step,
        mark: Mark,
        theme: &Theme,
        view_id: EntityId,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let glyph = match mark {
            Mark::Waiting => div()
                .size(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(theme.border)
                .into_any_element(),
            Mark::Running => loaders::spinning_ring(16.0, theme.busy, view_id, window, cx),
            Mark::Done => div()
                .size(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme.success.opacity(0.15))
                .child(
                    icons::icon(icons::CHECK)
                        .size(px(11.0))
                        .text_color(theme.success),
                )
                .into_any_element(),
            Mark::Failed => div()
                .size(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme.danger.opacity(0.15))
                .child(
                    icons::icon(icons::CLOSE)
                        .size(px(10.0))
                        .text_color(theme.danger),
                )
                .into_any_element(),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .text_size(px(12.5))
            .text_color(match mark {
                Mark::Waiting => theme.text_muted.opacity(0.55),
                Mark::Running => theme.text,
                Mark::Done => theme.text.opacity(0.85),
                Mark::Failed => theme.danger_muted.opacity(0.9),
            })
            .child(glyph)
            .child(SharedString::from(step.label()))
            .into_any_element()
    }

    fn render_wizard(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view_id = cx.entity_id();
        let busy = self.busy();
        let steps = self.steps;
        let consent_needed = self
            .preview
            .as_ref()
            .is_some_and(|preview| needs_insecure_consent(&preview.url));
        let insecure_ok = self.insecure_ok;
        let card = widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .flex_col()
                .items_start()
                .gap(px(10.0))
                .child(widgets::field_label(theme, "Pair a device"))
                .child(
                    div()
                        .w_full()
                        .child(popover::dialog_field(self.link.clone().into_any_element())),
                )
                .when_some(self.preview.clone(), |el, preview| {
                    el.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(icons::icon(icons::CLOUD).size(px(13.0)))
                            .child(SharedString::from(format!(
                                "{} · {}",
                                preview.label, preview.url
                            ))),
                    )
                })
                .when(consent_needed, |el| {
                    el.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("pair-insecure-consent")
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.insecure_ok = !this.insecure_ok;
                                        cx.notify();
                                    }))
                                    .child(widgets::toggle_switch(theme, insecure_ok)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(11.5))
                                    .text_color(theme.warning_muted.opacity(0.9))
                                    .child(
                                        "Plain HTTP: the token crosses that network unread. Check \
                                         this only on a network you control.",
                                    ),
                            ),
                    )
                })
                .child(div().w_full().flex().flex_col().gap(px(6.0)).children(
                    steps.iter().map(|(step, mark)| {
                        Self::step_row(*step, *mark, theme, view_id, window, cx)
                    }),
                ))
                .when_some(self.failure.clone(), |el, message| {
                    el.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(widgets::error_strip(theme, message))
                            .when_some(self.failure_hint, |el, hint| {
                                el.child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(theme.text_muted.opacity(0.75))
                                        .child(SharedString::from(hint)),
                                )
                            }),
                    )
                })
                .when_some(self.connected.clone(), |el, env| {
                    el.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::badge_active(theme, "Connected"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .text_color(theme.text)
                                    .child(SharedString::from(env.label.clone())),
                            )
                            .child(
                                popover::btn_primary(theme, "Open workspace")
                                    .id("open-paired-workspace")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.emit(SyncEvent::OpenWorkspace(env.id.clone()));
                                        this.cancel_wizard(cx);
                                    })),
                            ),
                    )
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            popover::btn_primary(theme, "Pair")
                                .id("pair-device-submit")
                                .opacity(if busy { 0.5 } else { 1.0 })
                                .when(!busy, |el| {
                                    el.on_click(cx.listener(|this, _, _, cx| this.start_pair(cx)))
                                }),
                        )
                        .child(
                            popover::btn_ghost(theme, "Cancel", "pair-cancel", cx.entity_id())
                                .id("pair-cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_wizard(cx))),
                        ),
                ),
        );
        card.into_any_element()
    }
}

impl Render for SyncPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &Theme::of(cx).clone();
        let (envs, rows_ready, engine_up, statuses) = {
            let state = self.state.read(cx);
            (
                state.paired_envs.clone(),
                state.env_list_synced,
                state.local_engine().is_some(),
                state.workspaces.clone(),
            )
        };
        let view_id = cx.entity_id();
        let dialog = self.render_remove_dialog(window.viewport_size(), cx);
        let adding = self.adding;
        let copied = self.copied.clone();
        let failed_copy = self.copy_failed.clone();

        let mut device_rows: Vec<AnyElement> = Vec::new();
        for (ix, env) in envs.iter().enumerate() {
            let paired_row = statuses.iter().find(|row| row.id == env.id);
            let status = paired_row
                .map(|row| row.status)
                .unwrap_or(WorkspaceStatus::Offline);
            let device_count = paired_row.map(|row| row.device_count).unwrap_or(0);
            let version = paired_row.and_then(|row| row.version.clone());
            let dot = match status {
                WorkspaceStatus::Online => theme.success,
                WorkspaceStatus::Connecting => theme.warning,
                WorkspaceStatus::Offline => theme.text_faint,
            };
            let tile = widgets::row_tile(theme, icons::CLOUD).relative().child(
                div()
                    .absolute()
                    .bottom(px(-3.0))
                    .right(px(-3.0))
                    .size(px(9.0))
                    .rounded_full()
                    .border_2()
                    .border_color(theme.surface)
                    .bg(dot),
            );
            let added = if env.added_at_ms > 0 {
                format_last_seen(
                    chrono::DateTime::from_timestamp_millis(env.added_at_ms),
                    chrono::Utc::now(),
                )
            } else {
                "unknown".to_string()
            };
            let is_copied = copied.as_deref() == Some(env.id.as_str());
            let copy_failed = failed_copy.as_deref() == Some(env.id.as_str());
            let row_env = env.clone();
            let mut meta = vec![
                div()
                    .child(SharedString::from(env.url.clone()))
                    .into_any_element(),
            ];
            if let Some(version) = version.as_deref() {
                meta.push(
                    div()
                        .child(SharedString::from(format!("komet {version}")))
                        .into_any_element(),
                );
            }
            meta.extend([
                div()
                    .child(SharedString::from(format!(
                        "{device_count} device{}",
                        if device_count == 1 { "" } else { "s" }
                    )))
                    .into_any_element(),
                div()
                    .child(SharedString::from(format!("added {added}")))
                    .into_any_element(),
                div()
                    .child(SharedString::from(match status {
                        WorkspaceStatus::Online => "online",
                        WorkspaceStatus::Connecting => "connecting",
                        WorkspaceStatus::Offline => "offline",
                    }))
                    .into_any_element(),
            ]);
            device_rows.push(
                widgets::card_row(theme, ix == 0)
                    .child(tile)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, env.label.clone()))
                            .child(widgets::meta_line(theme, meta)),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("copy-pair-link", ix))
                            .opacity(if is_copied { 1.0 } else { 0.85 })
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener({
                                let env = row_env.clone();
                                move |this, _, _, cx| this.copy_link(&env, cx)
                            }))
                            .child(SharedString::from(if is_copied {
                                "Link copied"
                            } else if copy_failed {
                                "Couldn't copy"
                            } else {
                                "Copy link"
                            })),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("unpair-device", ix))
                            .opacity(0.85)
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove = Some(RemoveDialog {
                                    env: row_env.clone(),
                                    purge: false,
                                });
                                cx.notify();
                            }))
                            .child("Remove"),
                    )
                    .into_any_element(),
            );
        }

        let devices_card = if device_rows.is_empty() {
            widgets::section_card(theme)
                .child(
                    widgets::card_row(theme, true).child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(if !rows_ready {
                                "Loading paired devices…"
                            } else {
                                "No devices paired yet — pair one to work on that machine from here."
                            })),
                    ),
                )
                .into_any_element()
        } else {
            widgets::section_card(theme)
                .children(device_rows)
                .into_any_element()
        };

        let edge_url = std::env::var("KOMET_EDGE_URL").unwrap_or_else(|_| "local".into());
        let has_token = std::env::var("KOMET_SYNC_TOKEN")
            .map(|token| !token.is_empty())
            .unwrap_or(false);
        let insecure_self = std::env::var("KOMET_EDGE_URL")
            .ok()
            .is_some_and(|url| needs_insecure_consent(&url));
        let local_card = widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, icons::MONITOR))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(
                                theme,
                                if has_token {
                                    "Synced to your own server"
                                } else {
                                    "Local only"
                                },
                            ))
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    div()
                                        .child(SharedString::from(format!("server: {edge_url}")))
                                        .into_any_element(),
                                    div()
                                        .child(SharedString::from(if has_token {
                                            "token: set"
                                        } else {
                                            "token: not set"
                                        }))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .into_any_element(),
            )
            .child(
                widgets::card_row(theme, false)
                    .flex_col()
                    .items_start()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(SharedString::from(
                                "To let another copy of Komet reach this machine, run `komet serve` \
                                 and paste the link it prints there (docs/self-hosted-sync.md).",
                            )),
                    )
                    .when(insecure_self, |el| {
                        el.child(widgets::warning_strip(
                            theme,
                            "Your own sync server is reached over plain HTTP — only safe on a \
                             network you control (KOMET_SYNC_ALLOW_INSECURE_HTTP=1).",
                        ))
                    })
                    .into_any_element(),
            )
            .into_any_element();

        div()
            .id("sync-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(
                        theme,
                        "Sync & devices",
                        (!envs.is_empty()).then_some(envs.len()),
                    ))
                    .child(widgets::page_subtitle(
                        theme,
                        "This machine's sync status, plus the devices paired with a komet:// link.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(theme, message)
                                .id("sync-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(local_card)
                    .child(
                        div()
                            .mt(px(28.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .flex_1()
                                    .child(widgets::field_label(theme, "Paired devices")),
                            )
                            .when(!adding, |el| {
                                el.child(
                                    popover::btn_primary(theme, "Add device")
                                        .id("add-device")
                                        .opacity(if engine_up { 1.0 } else { 0.5 })
                                        .when(engine_up, |el| {
                                            el.on_click(cx.listener(|this, _, _, cx| {
                                                this.open_wizard(cx)
                                            }))
                                        }),
                                )
                            }),
                    )
                    .when(adding, |el| el.child(self.render_wizard(theme, window, cx)))
                    .when(!adding, |el| el.child(devices_card))
                    .when(!engine_up, |el| {
                        el.child(
                            div()
                                .mt(px(10.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .text_size(px(11.5))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(loaders::spinning_ring(14.0, theme.busy, view_id, window, cx))
                                .child(SharedString::from(
                                    "Komet is still starting — pairing unlocks in a moment.",
                                )),
                        )
                    })
                    .child(
                        div()
                            .mt(px(20.0))
                            .text_size(px(11.5))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "A pairing link is a secret: it carries the token that grants shell \
                                 access on that machine. Keep it out of chat and screenshots.",
                            )),
                    ),
            )
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_non_loopback_http_needs_consent() {
        assert!(!needs_insecure_consent("https://sync.example.com"));
        assert!(!needs_insecure_consent("http://localhost:8787"));
        assert!(!needs_insecure_consent("http://127.0.0.1:8787"));
        assert!(!needs_insecure_consent("http://[::1]:8787/x"));
        assert!(needs_insecure_consent("http://192.168.0.9:8787"));
        assert!(needs_insecure_consent("http://vm1.lan:8787/push"));
        assert!(needs_insecure_consent("http://user@203.0.113.4"));
        // Never a false sense of safety on a bare hostname over http.
        assert!(needs_insecure_consent("http://vm1"));
    }

    #[test]
    fn typed_pairing_failures_get_copy_and_a_remedy() {
        let (message, hint) = explain_failure("unreachable: connection refused");
        assert_eq!(message, "Device unreachable — connection refused");
        assert!(hint.is_some());
        assert_eq!(
            explain_failure("unauthorized").0,
            "Device refused the token"
        );
        assert_eq!(
            explain_failure("already_synced_here").0,
            "Already synced here"
        );
        assert_eq!(
            explain_failure("invalid_link").0,
            "Not a valid pairing link"
        );
        // Local-store failures carry the engine's reason: these codes come from
        // opening files under the data dir, never from the pairing transport, so
        // the detail is the useful half of the message.
        let (message, hint) =
            explain_failure("store: /me/.komet/environments.json: Permission denied");
        assert_eq!(
            message,
            "Couldn't write the local copy — /me/.komet/environments.json: Permission denied"
        );
        assert!(hint.is_some());
        assert_eq!(
            explain_failure("runtime: environment has no namespace").0,
            "Couldn't open that workspace — environment has no namespace"
        );
        // Unknown codes stay visible rather than being swallowed.
        assert_eq!(
            explain_failure("weird: something").0,
            "Pairing failed (weird)"
        );
    }

    #[test]
    fn a_decode_failure_never_quotes_the_link() {
        let secret = "komet://pair/c2VjcmV0LXRva2Vu";
        let (message, _) = explain_failure(&format!("invalid_link: {secret}"));
        assert!(!message.contains("c2VjcmV0"), "{message}");
        assert_eq!(message, "Not a valid pairing link");
    }

    #[test]
    fn step_labels_read_as_the_wizard_order() {
        assert_eq!(Step::Decode.label(), "Reading the link");
        assert_eq!(Step::Verify.label(), "Reaching the device");
        assert_eq!(Step::Pair.label(), "Opening the workspace");
    }
}
