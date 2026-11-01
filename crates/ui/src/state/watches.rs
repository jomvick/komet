use std::sync::Arc;

use gpui::{Context, Task};
use komet_doc::TranscriptFrame;
use komet_proto::Chat;
use komet_rpc::{RpcClient, RpcError, methods};
use serde::de::DeserializeOwned;

use super::{AppState, ConnectionStatus, EngineHandle, WatchOwner, wait_for_deferred_engine};

/// Observe assembly after an early attach (cloud onboarding or another viewport
/// reaching the embedded engine over IPC). Data subscriptions wait on the same
/// result, but their individual errors are not authoritative: older engines may
/// legitimately omit a watch method. Only the assembly result may fail the
/// whole connection.
pub(crate) fn spawn_deferred_engine_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
) -> Option<Task<()>> {
    let mut deferred = handle.deferred_state()?;
    Some(cx.spawn(async move |this, cx| {
        let Err(failure) = wait_for_deferred_engine(&mut deferred).await else {
            return;
        };
        tracing::error!(error = %failure, "engine assembly failed after attachment");
        // Embedded handles release their IPC listener before exposing Retry;
        // remote handles stop their completed readiness probe.
        handle.shutdown().await;
        this.update(cx, |state, cx| {
            // The runtime died after attaching: say so and hand Retry to the
            // user — re-embedding a half-assembled engine automatically would
            // hide a real startup failure behind a spinner.
            state.connection = ConnectionStatus::Failed {
                message: failure,
                retryable: true,
            };
            cx.notify();
        })
        .ok();
    }))
}

pub(crate) fn is_connection_error(err: &RpcError) -> bool {
    matches!(err, RpcError::Closed | RpcError::Transport(_))
}

/// The sandbox level for the `selected` chat. The new-chat screen (no
/// selection) uses `default`. An existing chat uses its saved level
/// (`ChatConfig.sandbox`, synced across devices), or `WorkspaceWrite`, the
/// level such chats always used, when it has no saved configuration or its
/// row has not loaded yet. Changing the default therefore never raises the
/// level of an existing chat.
pub fn access_for_selection(
    chats: &[Chat],
    selected: Option<&str>,
    default: komet_proto::SandboxLevel,
) -> komet_proto::SandboxLevel {
    let Some(id) = selected else {
        return default;
    };
    chats
        .iter()
        .find(|c| c.id == id)
        .and_then(|chat| chat.config.as_ref())
        .map_or(komet_proto::SandboxLevel::WorkspaceWrite, |config| {
            config.sandbox
        })
}

/// Chats watch. Boot selection is the shell's job (it lands on the first
/// restored open tab, device-local state this entity can't see); this task
/// only pumps frames.
pub(crate) fn spawn_chats_watch(
    cx: &mut Context<AppState>,
    client: Arc<RpcClient>,
    owner: WatchOwner,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Resubscribe loop (same contract as the transcript watch): a daemon
        // restart or RPC drop ends the stream, and a bare return here froze
        // the sidebar until app restart — new chats, renames and archives
        // from every device silently stopped arriving.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match client
                .subscribe(methods::WATCH_CHATS, serde_json::json!({}))
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(error = %err, "chats watch unavailable; retrying");
                    if is_connection_error(&err) {
                        pump_lost(&this, &owner, cx);
                        return;
                    }
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: Vec<Chat> = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(error = %err, "dropping malformed chats frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    state.apply_chats(parsed);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!("chats stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

/// Report a dropped connection for the watch's owner and stop this pump: whoever
/// re-dials re-attaches it. A workspace pump must never reconnect through the
/// local engine — its rows would arrive under the wrong device.
pub(crate) fn pump_lost(
    this: &gpui::WeakEntity<AppState>,
    owner: &WatchOwner,
    cx: &mut gpui::AsyncApp,
) {
    match owner {
        WatchOwner::Engine => {
            let _ = this.update(cx, |state, cx| {
                tracing::warn!("engine connection lost; reconnecting");
                state.schedule_reconnect("engine connection lost".to_string(), cx);
            });
        }
        WatchOwner::Workspace(env_id) => {
            let env_id = env_id.clone();
            let _ = this.update(cx, |state, cx| {
                tracing::warn!(%env_id, "device workspace connection lost");
                state.handle_workspace_lost(&env_id, cx);
            });
        }
    }
}

pub(crate) fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    client: Arc<RpcClient>,
    owner: WatchOwner,
    method: &'static str,
    apply: fn(&mut AppState, T),
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Resubscribe loop: these are the standing Sessions/Devices/Spaces
        // watches — a daemon restart ended the stream and a bare return froze
        // them for the rest of the app's life (remote Working dots staled out
        // to nothing after 45s, and Idle/Completed transitions from other
        // devices never arrived again — "the session never completes").
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match client.subscribe(method, serde_json::json!({})).await {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(method, error = %err, "watch unavailable; retrying");
                    if is_connection_error(&err) {
                        pump_lost(&this, &owner, cx);
                        return;
                    }
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: T = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(method, error = %err, "dropping malformed watch frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    apply(state, parsed);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!(method, "watch stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

/// Best-effort `LocalDevice` probe: names the registry row that is "This
/// device". Engines that don't serve the method leave it `None`.
pub(crate) fn spawn_local_device_probe(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(value) = handle
            .client()
            .call("LocalDevice", serde_json::json!({}))
            .await
        else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        let id = value
            .get("id")
            .or_else(|| value.get("deviceId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(id) = id {
            this.update(cx, |state, cx| {
                state.note_home_device(id);
                cx.notify();
            })
            .ok();
        }
    })
}

pub(crate) fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    client: Arc<RpcClient>,
    owner: WatchOwner,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Outer loop: a delta desync (missed frame) resubscribes immediately
        // and the fresh stream's opening reset heals the copy; a subscribe
        // failure, malformed frame, or stream end retries on a delay. Every
        // path re-enters the loop — a return here freezes the transcript
        // with no banner and no heal short of an app restart (this watch and
        // its engine-side room are the ONLY transcript delivery path). The
        // task itself is dropped by select_chat/apply_chats when the chat is
        // deselected or deleted, so retrying can't outlive relevance.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({ "chatId": chat_id });
            let mut rx = match client.subscribe(methods::WATCH_DOC_MESSAGES, params).await {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "transcript watch failed; retrying");
                    if is_connection_error(&err) {
                        pump_lost(&this, &owner, cx);
                        return;
                    }
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // Schema skew (a newer peer's entry shape arriving
                        // through sync): a skipped frame is a silently stale
                        // copy, so resubscribe for a fresh reset — delayed,
                        // in case the reset itself is what can't parse.
                        tracing::warn!(error = %err, "malformed transcript frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    // Guard against a stale pump racing a newer selection.
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        if let Err(err) = state.apply_transcript_frame(frame) {
                            tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                            desync = true;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            // Stream ended: engine restart, RPC drop, or chat purge. Retry;
            // the purge case is cleaned up by apply_chats dropping this task.
            tracing::debug!(%chat_id, "transcript stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}
