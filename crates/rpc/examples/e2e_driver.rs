//! Two-device e2e smoke driver (`scripts/e2e-smoke.sh` runs it).
//!
//! Connects to two running headless engines over their localhost IPC ports and
//! proves the cross-device command plane end to end against a real edge:
//!
//! 1. `LocalDevice` on both — distinct device ids;
//! 2. `Mutate createChat` on A (A hosts the chat, mock harness config);
//! 3. waits until the chat row syncs A → edge → B (`WatchChats` on B);
//! 4. `QueueCommand` a Run on **B** — the doc path: B commits the command into its
//!    replica, nudges A's device room, A's host executor drains and runs it;
//! 5. waits on **B** until the assistant entry lands `complete` with the mock
//!    transcript (A → edge → B), and the session-status row round-trips.
//!
//! Scenarios (`argv[3]`):
//! - `commands` (default): the flow above — B → A command, A → B transcript,
//!   i.e. a two-way exchange.
//! - `restart`: server killed and restarted between two runs — no loss, no
//!   duplicate, the cursor resumes. With the engine-side
//!   `KOMET_TEST_WAKE_FILE` hook armed this doubles as the wake scenario: the
//!   script injects the wake event and greps the engines' logs for the
//!   wake-triggered redial.
//! - `cut`: engines dial through a TCP proxy; the script kills the proxy
//!   (client-side cut) and brings it back — the driver reports the seconds
//!   from "proxy restored" to the second transcript entry (`RECONNECT_S=`).
//!
//! Coordination with the driving script is by marker files: the driver writes
//! `KOMET_E2E_READY` when the script may act (kill/restart), then waits for
//! `KOMET_E2E_SIGNAL` to exist before resuming.
//!
//! Prints `PASS`/`FAIL` lines; exits nonzero on failure.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use komet_doc::MessageRole;
use komet_rpc::{RpcClient, connect_ws, methods};

const STEP_TIMEOUT: Duration = Duration::from_secs(90);
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(120);
const MOCK_TEXT: &str = "Mock harness reporting in.";

fn fail(message: &str) -> ! {
    eprintln!("FAIL: {message}");
    std::process::exit(1);
}

fn pass(message: &str) {
    println!("PASS: {message}");
}

async fn device_id(client: &RpcClient, label: &str) -> String {
    match client
        .call(methods::LOCAL_DEVICE, serde_json::json!({}))
        .await
    {
        Ok(value) => value
            .get("deviceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| fail(&format!("{label}: LocalDevice reply missing deviceId"))),
        Err(err) => fail(&format!("{label}: LocalDevice call failed: {err}")),
    }
}

/// Subscribe to a watch-stream and poll items until `predicate` returns `Some`.
///
/// Stream end is NOT a failure: the engine ends watch streams deliberately at
/// lifecycle boundaries (the chat2 cutover drops the s2 handle so watchers
/// converge onto the new room), and the client contract — the UI does exactly
/// this — is to resubscribe. The step deadline still bounds the whole wait.
async fn wait_stream<T>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    what: &str,
    mut predicate: impl FnMut(&serde_json::Value) -> Option<T>,
) -> T {
    let deadline = Instant::now() + STEP_TIMEOUT;
    'resubscribe: loop {
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            fail(&format!(
                "{what}: timed out after {}s",
                STEP_TIMEOUT.as_secs()
            ));
        }
        let mut rx = match client.subscribe(method, params.clone()).await {
            Ok(rx) => rx,
            Err(err) => fail(&format!("{what}: subscribe {method} failed: {err}")),
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                fail(&format!(
                    "{what}: timed out after {}s",
                    STEP_TIMEOUT.as_secs()
                ));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(item)) => {
                    if let Some(found) = predicate(&item) {
                        return found;
                    }
                }
                Ok(None) => {
                    // Lifecycle boundary (e.g. chat2 cutover): resubscribe.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue 'resubscribe;
                }
                Err(_) => fail(&format!(
                    "{what}: timed out after {}s",
                    STEP_TIMEOUT.as_secs()
                )),
            }
        }
    }
}

fn apply_transcript_item(
    current: &mut Vec<komet_doc::SessionMessageEntry>,
    item: &serde_json::Value,
) -> Result<(), String> {
    let frame: komet_doc::TranscriptFrame =
        serde_json::from_value(item.clone()).map_err(|err| err.to_string())?;
    komet_doc::apply_transcript_frame(current, frame).map_err(|err| err.to_string())
}

// ── scenario plumbing ───────────────────────────────────────────────────────

struct Setup {
    b: RpcClient,
    a_dev: String,
    chat_id: String,
}

/// Connect to both engines and create a chat hosted by A with the mock
/// harness, waiting until the space + chat rows sync A → edge → B.
async fn setup(a_port: u16, b_port: u16) -> Setup {
    let a_token = komet_rpc::ipc_token::read_default(a_port)
        .unwrap_or_else(|err| fail(&format!("read engine token :{a_port}: {err}")));
    let a = connect_ws(&format!("ws://127.0.0.1:{a_port}"), &a_token)
        .await
        .unwrap_or_else(|err| fail(&format!("connect device A ipc :{a_port}: {err}")));
    let b_token = komet_rpc::ipc_token::read_default(b_port)
        .unwrap_or_else(|err| fail(&format!("read engine token :{b_port}: {err}")));
    let b = connect_ws(&format!("ws://127.0.0.1:{b_port}"), &b_token)
        .await
        .unwrap_or_else(|err| fail(&format!("connect device B ipc :{b_port}: {err}")));

    // 1. Distinct identities.
    let a_dev = device_id(&a, "A").await;
    let b_dev = device_id(&b, "B").await;
    if a_dev == b_dev {
        fail("A and B report the same deviceId — data dirs are shared");
    }
    pass(&format!("devices distinct (A={a_dev} B={b_dev})"));

    // 2. Create a space + chat on A, hosted by A, with the mock harness (the
    //    space fixes device + cwd — the chat row derives both from it).
    let space_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": a_dev,
            "path": "/tmp",
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createSpace on A: {err}")));
    let chat_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createChat",
            "chatId": chat_id,
            "spaceId": space_id,
            "config": {
                "harness": "mock",
                "model": null,
                "reasoning": null,
                "sandbox": "workspace-write",
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createChat on A: {err}")));
    pass(&format!("space + chat created on A ({chat_id})"));

    // 2b. Space row syncs A → edge → B (WatchSpaces).
    let space_device = wait_stream(
        &b,
        methods::WATCH_SPACES,
        serde_json::json!({}),
        "space row visible on B",
        |item| {
            item.as_array()?
                .iter()
                .find(|space| space.get("id").and_then(|v| v.as_str()) == Some(space_id.as_str()))
                .and_then(|space| space.get("deviceId")?.as_str().map(str::to_string))
        },
    )
    .await;
    if space_device != a_dev {
        fail(&format!(
            "space synced to B but owned by {space_device}, expected {a_dev}"
        ));
    }
    pass("space row synced A -> edge -> B (owner = A)");

    // 3. Workspace sync A → edge → B: the chat row appears in B's WatchChats.
    let hosted_by = wait_stream(
        &b,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        "chat row visible on B",
        |item| {
            item.as_array()?
                .iter()
                .find(|chat| chat.get("id").and_then(|v| v.as_str()) == Some(chat_id.as_str()))
                .and_then(|chat| chat.get("deviceId")?.as_str().map(str::to_string))
        },
    )
    .await;
    if hosted_by != a_dev {
        fail(&format!(
            "chat synced to B but hosted by {hosted_by}, expected {a_dev}"
        ));
    }
    pass("chat row synced A -> edge -> B (host = A)");

    Setup { b, a_dev, chat_id }
}

/// Queue a Run command on B through the doc (no targetDeviceId). Returns the
/// client-minted message id, which the host writes as the USER entry's id —
/// the handle that ties one completed response to one command.
async fn queue_run(b: &RpcClient, chat_id: &str, label: &str) -> String {
    let message_id = uuid::Uuid::new_v4().to_string();
    b.call(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": chat_id,
            "command": {
                "kind": "run",
                "messageId": message_id,
                "request": {
                    "prompt": format!("e2e smoke: report in ({label})"),
                    "model": null,
                    "reasoning": null,
                    "cwd": "/tmp",
                    "sandbox": "workspace-write",
                    "autoApprove": true,
                    "resume": null,
                },
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("QueueCommand ({label}) on B: {err}")));
    pass(&format!(
        "run command queued on B via the doc command queue ({label})"
    ));
    message_id
}

/// Materialize the doc-transcript stream on B until `message_id`'s own turn is
/// complete. A completed response only counts for THIS command when it was
/// created at/after the command's user entry — so a repeated earlier run (the
/// restart/cut scenarios queue the same prompt twice) can never satisfy the
/// later expectation. Also asserts the transcript holds exactly
/// `expected_complete` complete assistant entries, proving nothing was lost or
/// duplicated across the interruption.
async fn wait_command_response(
    b: &RpcClient,
    chat_id: &str,
    message_id: &str,
    expected_complete: usize,
    what: &str,
) -> Vec<komet_doc::SessionMessageEntry> {
    let mut transcript = Vec::new();
    wait_stream(
        b,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({ "chatId": chat_id }),
        what,
        |item| {
            apply_transcript_item(&mut transcript, item)
                .unwrap_or_else(|err| fail(&format!("{what}: {err}")));
            let user = transcript
                .iter()
                .find(|entry| entry.role == MessageRole::User && entry.id == message_id)
                .cloned()?;
            let served = transcript.iter().any(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.status == Some(komet_doc::MessageStatus::Complete)
                    && entry.created_at >= user.created_at
            });
            served.then(|| transcript.clone())
        },
    )
    .await;

    let complete: Vec<_> = transcript
        .iter()
        .filter(|entry| {
            entry.role == MessageRole::Assistant
                && entry.status == Some(komet_doc::MessageStatus::Complete)
        })
        .collect();
    if complete.len() != expected_complete {
        fail(&format!(
            "{what}: expected exactly {expected_complete} complete assistant \
             entries, got {} (a lost or duplicated run)",
            complete.len()
        ));
    }
    transcript
}

/// Every completed assistant entry came from the host and carries the mock
/// transcript — checked per entry so a stale entry cannot mask a broken one.
fn assert_complete_responses(
    transcript: &[komet_doc::SessionMessageEntry],
    host: &str,
    expected_complete: usize,
    what: &str,
) {
    let complete: Vec<_> = transcript
        .iter()
        .filter(|entry| {
            entry.role == MessageRole::Assistant
                && entry.status == Some(komet_doc::MessageStatus::Complete)
        })
        .collect();
    if complete.len() != expected_complete {
        fail(&format!(
            "{what}: expected {expected_complete} complete entries, got {}",
            complete.len()
        ));
    }
    for entry in complete {
        if entry.device_id != host {
            fail(&format!(
                "{what}: assistant entry written by {}, expected host {host}",
                entry.device_id
            ));
        }
        let text = serde_json::to_string(entry)
            .unwrap_or_else(|err| fail(&format!("{what}: serialize transcript: {err}")));
        if !text.contains(MOCK_TEXT) {
            fail(&format!(
                "{what}: assistant entry lacks the mock transcript: {text}"
            ));
        }
    }
}

fn marker_from_env(name: &str) -> PathBuf {
    std::env::var(name)
        .map(PathBuf::from)
        .unwrap_or_else(|_| fail(&format!("{name} not set — the script must provide it")))
}

/// Tell the script it may act (kill/restart), then wait for its signal.
async fn script_handoff(label: &str) -> std::time::SystemTime {
    let ready = marker_from_env("KOMET_E2E_READY");
    let signal = marker_from_env("KOMET_E2E_SIGNAL");
    std::fs::File::create(&ready)
        .and_then(|mut f| f.write_all(label.as_bytes()))
        .unwrap_or_else(|err| fail(&format!("write ready marker {}: {err}", ready.display())));
    pass(&format!("handed off to the script ({label})"));
    let deadline = Instant::now() + SIGNAL_TIMEOUT;
    while !signal.exists() {
        if Instant::now() >= deadline {
            fail(&format!(
                "signal marker {} never appeared",
                signal.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    std::fs::metadata(&signal)
        .and_then(|m| m.modified())
        .unwrap_or_else(|err| fail(&format!("signal marker mtime: {err}")))
}

// ── scenarios ───────────────────────────────────────────────────────────────

/// The original two-device flow: B → A command, A → B transcript + session
/// status — the two-way exchange the smoke proves.
async fn commands(a_port: u16, b_port: u16) {
    let s = setup(a_port, b_port).await;

    // 4. Queue the run from B THROUGH THE DOC (no targetDeviceId): B commits
    //    the command into its replica; the nudge + room sync hand it to A's
    //    executor.
    let message_id = queue_run(&s.b, &s.chat_id, "B -> A").await;

    // 5a. Assistant entry executed by A arrives back on B, complete, with the
    // mock text. WatchDocMessages speaks the framed protocol: a full `reset`
    // list on attach, then `upsert`/`append` deltas. Materialize every frame
    // through the shared parser so candidates persist across incremental frames.
    // The command's own message id pins which response belongs to it.
    let transcript = wait_command_response(
        &s.b,
        &s.chat_id,
        &message_id,
        1,
        "assistant transcript on B",
    )
    .await;
    assert_complete_responses(&transcript, &s.a_dev, 1, "assistant transcript on B");
    pass("assistant entry (host-executed, status=complete) synced back to B");

    // 5b. Session-status row round-trips: B sees A's session for this chat go idle.
    wait_stream(
        &s.b,
        methods::WATCH_SESSIONS,
        serde_json::json!({}),
        "session status on B",
        |item| {
            item.as_array()?.iter().find(|session| {
                session.get("chatId").and_then(|v| v.as_str()) == Some(s.chat_id.as_str())
                    && session.get("deviceId").and_then(|v| v.as_str()) == Some(s.a_dev.as_str())
                    && session.get("status").and_then(|v| v.as_str()) == Some("idle")
            })?;
            Some(())
        },
    )
    .await;
    pass("session status (A working -> idle) round-tripped to B");

    println!("PASS: two-device e2e smoke complete");
}

/// Server killed and restarted between two runs (script kills at the ready
/// marker, restarts, then writes the signal): neither run is lost, nothing is
/// duplicated, and the second run syncs over the resumed cursors. Also the
/// wake scenario's driver half — with `KOMET_TEST_WAKE_FILE` armed the script
/// injects the wake event and greps the engines' logs for the redial.
async fn restart(a_port: u16, b_port: u16, label: &str) {
    let s = setup(a_port, b_port).await;

    let before = queue_run(&s.b, &s.chat_id, "before the restart").await;
    wait_command_response(&s.b, &s.chat_id, &before, 1, "first transcript on B").await;
    pass("first run synced B -> A -> B before the restart");

    let restarted_at = script_handoff(label).await;

    let after = queue_run(&s.b, &s.chat_id, "after the restart").await;
    if before == after {
        fail("the two commands share a message id — identity is not per command");
    }
    let transcript =
        wait_command_response(&s.b, &s.chat_id, &after, 2, "second transcript on B").await;
    // Exactly 2 complete entries, both from the host, both carrying the mock
    // text: the restart lost nothing and duplicated nothing. The `after` id
    // guarantees the second entry is the second command's own response, not a
    // replay of the first.
    assert_complete_responses(&transcript, &s.a_dev, 2, "second transcript on B");
    pass("second run synced after the restart; no loss, no duplicate (2/2)");

    // The sync must have recovered promptly: the second transcript entry
    // arriving is the cursor-resumed proof; report the lag for the script.
    if let Ok(elapsed) = std::time::SystemTime::now().duration_since(restarted_at) {
        println!("RESUME_S={:.1}", elapsed.as_secs_f64());
    }
    println!("PASS: {label} scenario complete");
}

/// Engines dial through a TCP proxy the script kills and restores (the
/// client-side cut): report the seconds from "proxy restored" to the second
/// transcript entry — the script bounds it.
async fn cut(a_port: u16, b_port: u16) {
    let s = setup(a_port, b_port).await;

    let before = queue_run(&s.b, &s.chat_id, "before the cut").await;
    wait_command_response(&s.b, &s.chat_id, &before, 1, "first transcript on B").await;
    pass("first run synced before the cut");

    let restored_at = script_handoff("proxy cut + restore").await;

    let after = queue_run(&s.b, &s.chat_id, "after the cut").await;
    if before == after {
        fail("the two commands share a message id — identity is not per command");
    }
    let transcript =
        wait_command_response(&s.b, &s.chat_id, &after, 2, "second transcript on B").await;
    assert_complete_responses(&transcript, &s.a_dev, 2, "second transcript on B");
    let elapsed = std::time::SystemTime::now()
        .duration_since(restored_at)
        .unwrap_or_else(|err| fail(&format!("clock: {err}")));
    println!("RECONNECT_S={:.1}", elapsed.as_secs_f64());
    println!("PASS: cut scenario complete");
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let a_port: u16 = args
        .next()
        .unwrap_or_else(|| "27801".into())
        .parse()
        .expect("A port");
    let b_port: u16 = args
        .next()
        .unwrap_or_else(|| "27802".into())
        .parse()
        .expect("B port");
    let scenario = args.next().unwrap_or_else(|| "commands".into());

    match scenario.as_str() {
        "commands" => commands(a_port, b_port).await,
        "restart" | "wake" => restart(a_port, b_port, &scenario).await,
        "cut" => cut(a_port, b_port).await,
        other => fail(&format!(
            "unknown scenario {other:?} (commands|restart|wake|cut)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_reset_frame_is_materialized() {
        let mut transcript = Vec::new();
        apply_transcript_item(
            &mut transcript,
            &serde_json::json!({
                "reset": [{
                    "id": "assistant-1",
                    "role": "assistant",
                    "parts": [{ "kind": "text", "id": "t0", "text": MOCK_TEXT }],
                    "createdAt": 1,
                    "deviceId": "device-a",
                    "status": "complete"
                }]
            }),
        )
        .expect("reset frame");

        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].device_id, "device-a");
        assert_eq!(
            transcript[0].status,
            Some(komet_doc::MessageStatus::Complete)
        );
    }
}
