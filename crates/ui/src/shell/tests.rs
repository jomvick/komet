use komet_engine::InstanceLock;

use super::*;

#[test]
fn connection_banner_names_the_attempt_and_the_countdown() {
    let now = Utc::now();
    let status = ConnectionStatus::Reconnecting {
        attempt: 2,
        retry_in_ms: 5_000,
        reason: "socket closed".into(),
    };
    // The frame's `retry_in_ms` says 5 s but the deadline is 1.2 s out: the
    // countdown follows the clock, never the snapshot it was scheduled with.
    let due = now + chrono::Duration::milliseconds(1_200);
    let (label, retry) = reconnect_banner(&status, now, Some(due)).expect("reconnecting");
    assert!(retry, "{label}");
    assert!(label.contains("Reconnecting"), "{label}");
    assert!(
        label.contains(&format!("2/{RECONNECT_MAX_ATTEMPTS}")),
        "{label}"
    );
    assert!(label.contains(&format_ms(1_200)), "{label}");
    assert!(!label.contains(&format_ms(5_000)), "{label}");
    assert!(label.contains("socket closed"), "{label}");
    // An overdue deadline reads as 0 — a negative wait is a lie about a
    // retry that is already running.
    let late = now - chrono::Duration::milliseconds(400);
    let (label, _) = reconnect_banner(&status, now, Some(late)).expect("reconnecting");
    assert!(label.contains(&format_ms(0)), "{label}");
    // Degraded: the reason as a warning, no countdown (nothing is queued).
    let (label, retry) = reconnect_banner(
        &ConnectionStatus::Degraded {
            reason: "no output".into(),
        },
        now,
        None,
    )
    .expect("degraded");
    assert_eq!(label, "no output");
    assert!(!retry);
    assert!(!label.contains("retrying in"), "{label}");
    // Ready draws nothing at all.
    assert_eq!(reconnect_banner(&ConnectionStatus::Ready, now, None), None);
}

fn update_status(version: &str, available: bool) -> komet_update::UpdateStatus {
    komet_update::UpdateStatus {
        current_version: "1.0.0".into(),
        latest_version: Some(version.into()),
        update_available: available,
        checked_at: None,
        error: None,
        available_assets: Vec::new(),
    }
}

#[test]
fn update_detection_is_once_per_version_and_resets_for_newer_release() {
    let first = update_status("1.1.0", true);
    assert_eq!(
        newly_detected_update(None, &first).as_deref(),
        Some("1.1.0")
    );
    assert_eq!(
        newly_detected_update(Some("1.1.0"), &first),
        None,
        "repeated frames must not notify again"
    );
    let newer = update_status("1.2.0", true);
    assert_eq!(
        newly_detected_update(Some("1.1.0"), &newer).as_deref(),
        Some("1.2.0")
    );
}

#[test]
fn persistent_plan_card_shows_unfinished_work() {
    // One open task is enough: the card stays above the composer however
    // long the transcript gets, because it is derived from the doc, not
    // from what happens to be on screen.
    assert_eq!(plan_card_layout(1, 0, true), Some((1, 0)));
    assert_eq!(
        plan_card_layout(1, 0, false),
        Some((1, 0)),
        "unfinished work keeps the card even after the run settles"
    );
    // Past the display cap the extra tasks are announced, never silently cut.
    assert_eq!(plan_card_layout(9, 2, true), Some((6, 3)));
    assert_eq!(
        plan_card_layout(PLAN_CARD_MAX_ROWS, 0, true),
        Some((6, 0)),
        "exactly at the cap there is nothing to announce"
    );
}

#[test]
fn persistent_plan_card_leaves_when_the_plan_is_done() {
    assert_eq!(
        plan_card_layout(3, 3, false),
        None,
        "everything done and the run settled: the chips carry the record"
    );
    assert_eq!(
        plan_card_layout(3, 3, true),
        Some((3, 0)),
        "a live run keeps the last plan on screen — no flicker at the tail"
    );
    assert_eq!(
        plan_card_layout(0, 0, true),
        None,
        "an empty plan never renders a card"
    );
}

#[test]
fn update_modal_dismissal_is_version_scoped() {
    let status = update_status("1.1.0", true);
    assert!(!update_modal_should_show("1.1.0", Some("1.1.0"), &status));
    assert!(update_modal_should_show(
        "1.2.0",
        Some("1.1.0"),
        &update_status("1.2.0", true)
    ));
}

#[test]
fn update_notification_preferences_gate_background_pings() {
    assert!(update_notification_allowed(true, false, true));
    assert!(!update_notification_allowed(true, true, true));
    assert!(update_notification_allowed(true, true, false));
    assert!(!update_notification_allowed(false, false, false));
}

#[tokio::test]
async fn remote_shutdown_waits_for_ipc_release() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(listener);
    });

    wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_secs(2))
        .await
        .unwrap();
    release.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn remote_shutdown_waits_for_engine_lock_release() {
    let dir = tempfile::tempdir().unwrap();
    let lock = InstanceLock::acquire(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let lock_released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let released_by_task = lock_released.clone();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(lock);
        released_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(lock_released.load(std::sync::atomic::Ordering::SeqCst));
    release.await.unwrap();
}

#[tokio::test]
async fn remote_shutdown_times_out_while_ipc_remains_open() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let error = wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_millis(100))
        .await
        .unwrap_err();

    assert!(error.contains("did not finish stopping"));
    drop(listener);
}

#[test]
fn account_actions_follow_the_attached_workspace_scope() {
    assert_eq!(
        account_menu_action(Some(WorkspaceScope::Local), SyncFlow::Idle),
        None
    );
    assert_eq!(
        account_menu_action(Some(WorkspaceScope::Synced), SyncFlow::Idle),
        Some(AccountMenuAction::SignOut)
    );
    assert_eq!(
        account_menu_action(Some(WorkspaceScope::Development), SyncFlow::Idle),
        None
    );
}

#[test]
fn local_sign_in_offers_the_in_place_switch() {
    let signed_in = AuthState::SignedIn {
        user: komet_proto::UserProfile {
            id: "user-1".into(),
            email: "user@example.com".into(),
            name: None,
        },
        org_id: Some("org-1".into()),
    };

    assert_eq!(
        sync_flow_after_auth(
            SyncFlow::Enabling,
            Some(WorkspaceScope::Local),
            Some(&signed_in),
        ),
        SyncFlow::SwitchOffer { notice_open: true }
    );
    assert_eq!(
        sync_flow_after_auth(
            SyncFlow::Idle,
            Some(WorkspaceScope::Local),
            Some(&signed_in),
        ),
        SyncFlow::SwitchOffer { notice_open: true },
        "another viewport derives the pending switch from AuthStatus"
    );
    assert_eq!(
        sync_flow_after_auth(
            SyncFlow::SwitchOffer { notice_open: false },
            Some(WorkspaceScope::Local),
            Some(&signed_in),
        ),
        SyncFlow::SwitchOffer { notice_open: false },
        "shared auth updates do not reopen a postponed wizard"
    );
    assert_eq!(
        sync_flow_after_auth(
            SyncFlow::RestartPending { notice_open: false },
            Some(WorkspaceScope::Local),
            Some(&signed_in),
        ),
        SyncFlow::RestartPending { notice_open: false },
        "the quit fallback survives shared auth updates too"
    );
    assert_eq!(
        account_menu_action(
            Some(WorkspaceScope::Local),
            SyncFlow::SwitchOffer { notice_open: false },
        ),
        Some(AccountMenuAction::RestartPending)
    );
    for notice_open in [true, false] {
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::SwitchOffer { notice_open },
                Some(WorkspaceScope::Local),
                Some(&AuthState::SignedOut),
            ),
            SyncFlow::Idle,
            "revoked credentials cancel the pending switch"
        );
    }
}

#[test]
fn import_summary_errors_are_a_failure_not_a_success() {
    // Clean summary → done with counts.
    let clean = serde_json::json!({
        "kind": "summary", "importedChats": 2, "skippedChats": 1, "errors": []
    });
    assert_eq!(import_summary_outcome(&clean), Ok((2, 1)));

    // Any error means the wizard must NOT say "all set" — partial
    // migrations surface as an explicit failure with the first cause.
    let partial = serde_json::json!({
        "kind": "summary", "importedChats": 1, "skippedChats": 0,
        "errors": ["chat c2: journal copy failed"]
    });
    let message = import_summary_outcome(&partial).expect_err("errors must fail");
    assert!(message.contains("journal copy failed"), "{message}");
    assert!(message.contains("1 imported"), "{message}");

    let many = serde_json::json!({
        "kind": "summary", "importedChats": 0, "skippedChats": 0,
        "errors": ["a", "b", "c"]
    });
    let message = import_summary_outcome(&many).expect_err("errors must fail");
    assert!(message.contains("3 failures"), "{message}");

    // A summary missing the errors field entirely (older engine) is
    // treated as clean rather than failing every import.
    let legacy = serde_json::json!({ "kind": "summary", "importedChats": 4 });
    assert_eq!(import_summary_outcome(&legacy), Ok((4, 0)));
}

#[test]
fn spaces_only_local_work_still_gets_the_import_offer() {
    assert_eq!(local_work_phrase(0, 0), None, "nothing to bring");
    assert_eq!(local_work_phrase(2, 0).as_deref(), Some("the 2 sessions"));
    assert_eq!(
        local_work_phrase(0, 1).as_deref(),
        Some("the 1 project"),
        "a projects-only profile must be offered the import, not a bare switch"
    );
    assert_eq!(
        local_work_phrase(1, 2).as_deref(),
        Some("the 1 session and 2 projects")
    );
}

#[test]
fn dismissed_import_failure_stays_reachable_on_a_synced_runtime() {
    let signed_in = AuthState::SignedIn {
        user: komet_proto::UserProfile {
            id: "user-1".into(),
            email: "user@example.com".into(),
            name: None,
        },
        org_id: Some("org-1".into()),
    };

    // "Later" postpones the failure notice; it must not evaporate.
    let dismissed = SyncFlow::ImportFailed { notice_open: false };
    assert_eq!(
        sync_flow_after_auth(dismissed, Some(WorkspaceScope::Synced), Some(&signed_in)),
        dismissed,
        "a postponed import failure survives auth/scope updates"
    );

    // …and the account menu on the SYNCED runtime still exposes the
    // re-entry point. This is the whole point: after the switch there is
    // no local runtime left to re-derive an offer from, so this menu row
    // is the only path back to the retry dialog.
    assert_eq!(
        account_menu_action(Some(WorkspaceScope::Synced), dismissed),
        Some(AccountMenuAction::RestartPending),
        "retry must remain reachable after dismissal"
    );
    assert_eq!(
        account_menu_action(
            Some(WorkspaceScope::Synced),
            SyncFlow::ImportFailed { notice_open: true },
        ),
        Some(AccountMenuAction::RestartPending)
    );

    // Resolving the failure restores the normal synced menu.
    assert_eq!(
        account_menu_action(Some(WorkspaceScope::Synced), SyncFlow::Idle),
        Some(AccountMenuAction::SignOut)
    );
}

#[test]
fn switch_lifecycle_survives_the_runtime_replacement_window() {
    let signed_in = AuthState::SignedIn {
        user: komet_proto::UserProfile {
            id: "user-1".into(),
            email: "user@example.com".into(),
            name: None,
        },
        org_id: Some("org-1".into()),
    };
    for flow in [
        SyncFlow::Switching { import: true },
        SyncFlow::Importing { done: 1, total: 3 },
        SyncFlow::ImportDone {
            imported: 3,
            skipped: 0,
        },
        SyncFlow::ImportFailed { notice_open: true },
        SyncFlow::ImportFailed { notice_open: false },
    ] {
        // Local (before the stop), detached (mid-replacement), and synced
        // (replacement runtime up): the driver owns these states — auth
        // and scope edges must never reset them.
        assert_eq!(
            sync_flow_after_auth(flow, Some(WorkspaceScope::Local), Some(&signed_in)),
            flow
        );
        assert_eq!(sync_flow_after_auth(flow, None, None), flow);
        assert_eq!(
            sync_flow_after_auth(flow, Some(WorkspaceScope::Synced), Some(&signed_in)),
            flow
        );
    }
}

#[test]
fn synced_sign_out_blocks_every_viewport_and_cannot_switch_accounts() {
    let signed_in_as_another_user = AuthState::SignedIn {
        user: komet_proto::UserProfile {
            id: "user-2".into(),
            email: "other@example.com".into(),
            name: None,
        },
        org_id: Some("org-2".into()),
    };

    assert_eq!(
        sync_flow_after_auth(
            SyncFlow::SigningOut,
            Some(WorkspaceScope::Synced),
            Some(&AuthState::SignedOut),
        ),
        SyncFlow::SignedOutRestartRequired,
        "the viewport that requested sign-out is blocked by AuthStatus"
    );
    assert_eq!(
        sync_flow_after_auth(
            SyncFlow::Idle,
            Some(WorkspaceScope::Synced),
            Some(&AuthState::SignedOut),
        ),
        SyncFlow::SignedOutRestartRequired,
        "another viewport observing the same runtime is also blocked"
    );
    assert_eq!(
        sync_flow_after_auth(
            SyncFlow::SignedOutRestartRequired,
            Some(WorkspaceScope::Synced),
            Some(&signed_in_as_another_user),
        ),
        SyncFlow::SignedOutRestartRequired,
        "new credentials cannot reopen the previous account's store"
    );
}

#[test]
fn titlebar_cluster_matches_komet_window_controls() {
    // komet window-controls.tsx: `left: fullscreen ? 12 : 88` — the
    // cluster clears the {14,15} traffic lights, and reclaims the inset
    // when fullscreen hides them.
    assert_eq!(titlebar_cluster_start(false), 88.0);
    assert_eq!(titlebar_cluster_start(true), 12.0);
}

#[test]
fn titlebar_spacer_selects_per_platform_and_fullscreen() {
    // macOS, lights visible: spacer fills up to the 88px cluster start.
    assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
    assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
    assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
    // macOS fullscreen: the inset animates away (clamped at zero when the
    // strip's own padding already exceeds the 12px cluster start).
    assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
    assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
    // Linux / Windows: never any inset.
    assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
    assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
}

#[test]
fn windows_caption_controls_reserve_titlebar_space() {
    assert_eq!(titlebar_right_padding(true, 0.0, 16.0), 124.0);
    assert_eq!(titlebar_right_padding(false, 0.0, 16.0), 16.0);
    assert_eq!(titlebar_right_padding(false, 78.0, 16.0), 94.0);
    assert_eq!(titlebar_right_padding(true, 78.0, 16.0), 124.0);
}

#[test]
fn linux_control_width_respects_layout_and_platform_capabilities() {
    let buttons = [
        Some(WindowButton::Minimize),
        Some(WindowButton::Maximize),
        Some(WindowButton::Close),
    ];
    assert_eq!(linux_control_side_width(buttons, true, true), 114.0);
    assert_eq!(linux_control_side_width(buttons, false, true), 78.0);
    assert_eq!(linux_control_side_width([None; 3], true, true), 0.0);
}

#[test]
fn linux_empty_desktop_button_layout_uses_visible_fallback_controls() {
    let controls = gpui::WindowControls::default();
    let layout = effective_linux_button_layout(
        WindowButtonLayout {
            left: [None; 3],
            right: [None; 3],
        },
        controls,
    );
    assert_eq!(
        layout.right,
        [
            Some(WindowButton::Minimize),
            Some(WindowButton::Maximize),
            Some(WindowButton::Close),
        ]
    );

    let only_close = effective_linux_button_layout(
        WindowButtonLayout {
            left: [None; 3],
            right: [None; 3],
        },
        gpui::WindowControls {
            minimize: false,
            maximize: false,
            ..controls
        },
    );
    assert_eq!(
        linux_control_side_width(only_close.right, false, false),
        42.0
    );
}

#[test]
fn client_resize_edges_skip_tiled_edges() {
    let size = gpui::size(px(100.0), px(80.0));
    assert_eq!(
        client_resize_edge(gpui::point(px(1.0), px(1.0)), size, 8.0, Tiling::default()),
        Some(ResizeEdge::TopLeft)
    );
    assert_eq!(
        client_resize_edge(gpui::point(px(50.0), px(1.0)), size, 8.0, Tiling::default()),
        Some(ResizeEdge::Top)
    );
    assert_eq!(
        client_resize_edge(
            gpui::point(px(50.0), px(40.0)),
            size,
            8.0,
            Tiling::default()
        ),
        None
    );
    assert_eq!(
        client_resize_edge(
            gpui::point(px(50.0), px(1.0)),
            size,
            8.0,
            Tiling {
                top: true,
                ..Tiling::default()
            },
        ),
        None
    );
}

#[test]
fn cluster_clearance_clears_the_overlay_buttons() {
    // Linux: buttons at 10..86; a 16px-padded header needs 78 more px to
    // put content at 86 + 8 breathing room.
    assert_eq!(cluster_clearance(false, false, 16.0), 78.0);
    assert_eq!(cluster_clearance(false, false, 10.0), 84.0);
    // macOS: buttons start at the 88px traffic-light cluster start.
    assert_eq!(
        cluster_clearance(true, false, 16.0),
        88.0 + 76.0 + 8.0 - 16.0
    );
    // macOS fullscreen: cluster reclaims the inset (starts at 12).
    assert_eq!(
        cluster_clearance(true, true, 16.0),
        12.0 + 76.0 + 8.0 - 16.0
    );
}

// ---- per-session panel flags (§1.10/1.11 parity: komet sessionPanels) ----

#[test]
fn session_panels_default_closed_per_chat() {
    let panels = SessionPanels::default();
    assert_eq!(panels.get("a"), ChatPanels::default());
    // Everything closed until explicitly opened (user request — the
    // brief default-open popped the pane on every visited session).
    assert!(!panels.get("a").terminal_open);
    assert!(!panels.get("a").changes_open);
    assert_eq!(panels.get("a").right_active, RightSurface::Picker);
    // The new-chat canvas ("" key) is its own session, also closed.
    assert!(!panels.get("").terminal_open);
}

#[test]
fn session_panels_flags_are_chat_scoped() {
    let mut panels = SessionPanels::default();
    // Opening the terminal in chat A opens it ONLY in chat A.
    assert!(panels.toggle_terminal("a"));
    assert!(panels.get("a").terminal_open);
    assert!(!panels.get("b").terminal_open);
    assert!(!panels.get("").terminal_open);
    // Changes pane in B is independent of A's terminal.
    assert!(panels.toggle_changes("b"));
    assert!(panels.get("b").changes_open);
    assert!(!panels.get("b").terminal_open);
    assert!(!panels.get("a").changes_open);
    // Switching back to A restores A's state untouched.
    assert!(panels.get("a").terminal_open);
    // Toggling off round-trips.
    assert!(!panels.toggle_terminal("a"));
    assert!(!panels.get("a").terminal_open);
}

#[test]
fn session_panels_both_flags_coexist_per_chat() {
    let mut panels = SessionPanels::default();
    panels.toggle_terminal("a");
    panels.toggle_changes("a");
    assert_eq!(
        panels.get("a"),
        ChatPanels {
            terminal_open: true,
            changes_open: true,
            ..Default::default()
        }
    );
    assert_eq!(panels.get("b"), ChatPanels::default());
    // The right pane round-trips back closed.
    assert!(!panels.toggle_changes("a"));
    assert!(!panels.get("a").changes_open);
}

#[test]
fn session_panels_update_tracks_right_surfaces() {
    let mut panels = SessionPanels::default();
    panels.update("a", |p| p.right_active = RightSurface::Diff(3));
    assert_eq!(panels.get("a").right_active, RightSurface::Diff(3));
    // Other chats keep the picker default.
    assert_eq!(panels.get("b").right_active, RightSurface::Picker);
    panels.update("a", |p| p.right_active = RightSurface::Terminal(7));
    assert_eq!(panels.get("a").right_active, RightSurface::Terminal(7));
    panels.update("a", |p| p.right_active = RightSurface::File(42));
    assert_eq!(panels.get("a").right_active, RightSurface::File(42));
}

// ---- sidebar resort FLIP diff (§1.6) ----

fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
    list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
}

#[test]
fn resort_offsets_empty_when_order_unchanged() {
    let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
    assert!(resort_offsets(&order, &order, 2.0).is_empty());
}

#[test]
fn resort_offsets_activity_moves_row_to_top() {
    // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
    // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
    // painted at +62 decaying to 0 (a glide UP into place). a and b shift
    // down by c's height + gap (31).
    let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
    let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
    let offsets = resort_offsets(&old, &new, 2.0);
    assert_eq!(offsets.get("c"), Some(&62.0));
    assert_eq!(offsets.get("a"), Some(&-31.0));
    assert_eq!(offsets.get("b"), Some(&-31.0));
}

#[test]
fn resort_offsets_respect_heights_and_gap() {
    // Tall row (45px) swaps with a short one (29px).
    let old = keys(&[("tall", 45.0), ("short", 29.0)]);
    let new = keys(&[("short", 29.0), ("tall", 45.0)]);
    let offsets = resort_offsets(&old, &new, 2.0);
    // short: old y 47 → new y 0; tall: old y 0 → new y 31.
    assert_eq!(offsets.get("short"), Some(&47.0));
    assert_eq!(offsets.get("tall"), Some(&-31.0));
}

#[test]
fn resort_offsets_ignore_added_and_removed_keys() {
    let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
    let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
    let offsets = resort_offsets(&old, &new, 2.0);
    // "new" has no old position (fades in instead); "gone" just goes.
    assert!(!offsets.contains_key("new"));
    assert!(!offsets.contains_key("gone"));
    // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
    // slot replaced by "new" of equal height — no move, no entry).
    assert_eq!(offsets.get("a"), Some(&-31.0));
    assert_eq!(offsets.get("b"), None);
}

#[test]
fn resort_glide_spec_matches_original() {
    // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
    assert_eq!(RESORT.duration_ms, 260);
    assert_eq!(RESORT.curve, motion::EASE_RESORT);
}

// ---- navigation history (titlebar back/forward) ----

fn chat(id: &str) -> NavEntry {
    NavEntry::Chat(id.to_string())
}

#[test]
fn nav_history_starts_with_nothing_to_walk() {
    let nav = NavHistory::new(chat(""));
    assert!(!nav.can_back());
    assert!(!nav.can_forward());
    assert_eq!(*nav.current(), chat(""));
}

#[test]
fn nav_push_then_back_and_forward() {
    let mut nav = NavHistory::new(chat("a"));
    nav.push(chat("b"));
    nav.push(NavEntry::Settings(SettingsSection::Devices));
    assert!(nav.can_back());
    assert!(!nav.can_forward());

    // Back walks toward the oldest entry without dropping anything.
    assert_eq!(
        nav.back(),
        Some(chat("b")),
        "back lands on the previous route"
    );
    assert_eq!(nav.back(), Some(chat("a")));
    assert!(!nav.can_back());
    assert!(nav.can_forward());
    assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

    // Forward retraces the same path.
    assert_eq!(nav.forward(), Some(chat("b")));
    assert_eq!(
        nav.forward(),
        Some(NavEntry::Settings(SettingsSection::Devices))
    );
    assert!(!nav.can_forward());
    assert_eq!(nav.forward(), None);
}

#[test]
fn nav_push_dedups_the_current_route() {
    let mut nav = NavHistory::new(chat("a"));
    nav.push(chat("a"));
    nav.push(chat("a"));
    assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
    nav.push(NavEntry::Settings(SettingsSection::Providers));
    nav.push(NavEntry::Settings(SettingsSection::Providers));
    assert_eq!(nav.len(), 2);
}

#[test]
fn nav_push_truncates_the_forward_branch() {
    // a → b → c, back to a, then push d: the b/c branch is gone (browser
    // semantics — komet's memory history PUSH truncates entries ahead).
    let mut nav = NavHistory::new(chat("a"));
    nav.push(chat("b"));
    nav.push(chat("c"));
    nav.back();
    nav.back();
    assert_eq!(*nav.current(), chat("a"));
    assert!(nav.can_forward());
    nav.push(chat("d"));
    assert!(!nav.can_forward(), "the old branch is unreachable");
    assert_eq!(nav.len(), 2);
    assert_eq!(nav.back(), Some(chat("a")));
    assert_eq!(nav.forward(), Some(chat("d")));
}

#[test]
fn nav_replace_swaps_in_place() {
    // The boot auto-select replaces the untouched canvas entry, so Back
    // stays disabled after landing in the last-used chat.
    let mut nav = NavHistory::new(chat(""));
    nav.replace(chat("boot"));
    assert_eq!(nav.len(), 1);
    assert_eq!(*nav.current(), chat("boot"));
    assert!(!nav.can_back());
}

#[test]
fn nav_settings_sections_are_distinct_entries() {
    let mut nav = NavHistory::new(chat("a"));
    nav.push(NavEntry::Settings(SettingsSection::Devices));
    nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
    assert_eq!(nav.len(), 3, "section changes are navigations");
    assert_eq!(
        nav.back(),
        Some(NavEntry::Settings(SettingsSection::Devices))
    );
    assert_eq!(nav.back(), Some(chat("a")));
}

#[test]
fn settings_sections_include_security() {
    assert_eq!(SettingsSection::ALL.len(), 10);
    assert!(SettingsSection::ALL.contains(&SettingsSection::Providers));
    assert_eq!(SettingsSection::Providers.label(), "Providers");
    assert!(SettingsSection::ALL.contains(&SettingsSection::Security));
    assert_eq!(SettingsSection::Security.label(), "Security");
    assert!(SettingsSection::ALL.contains(&SettingsSection::Integrations));
    assert_eq!(SettingsSection::Integrations.label(), "Integrations");
}

#[test]
fn settings_navigation_is_a_flat_list_with_unique_icons() {
    let unique_icons: std::collections::HashSet<_> = SettingsSection::ALL
        .into_iter()
        .map(SettingsSection::nav_icon)
        .collect();
    assert_eq!(unique_icons.len(), SettingsSection::ALL.len());
}

fn cli_status(
    harness: HarnessId,
    installed: &str,
    latest: &str,
    update_available: bool,
) -> komet_engine::harness_cli::HarnessCliStatus {
    komet_engine::harness_cli::HarnessCliStatus {
        harness,
        installed_version: Some(installed.into()),
        latest_version: Some(latest.into()),
        managed: true,
        update_available,
        check_error: None,
    }
}

#[test]
fn home_card_lists_only_clis_with_a_newer_release() {
    let updates = harness_updates(
        vec![
            // `claude --version` reports "2.1.309 (Claude Code)": the row keeps
            // the comparison token, not the product suffix.
            cli_status(
                HarnessId::ClaudeCode,
                "2.1.309 (Claude Code)",
                "2.1.310",
                true,
            ),
            cli_status(HarnessId::Codex, "0.9.0", "0.9.0", false),
            HarnessCliStatus {
                managed: false,
                ..cli_status(HarnessId::Cursor, "1.0.0", "1.1.0", true)
            },
        ],
        None,
    );
    assert_eq!(
        updates.len(),
        1,
        "up-to-date and unmanaged CLIs must not get a row"
    );
    assert_eq!(updates[0].harness, HarnessId::ClaudeCode);
    assert_eq!(cli_update_versions(&updates[0]), "2.1.309 → 2.1.310");
}

#[test]
fn home_card_skips_harnesses_disabled_on_this_device() {
    let enabled = HashSet::from([HarnessId::ClaudeCode]);
    let updates = harness_updates(
        vec![
            cli_status(HarnessId::ClaudeCode, "2.1.309", "2.1.310", true),
            cli_status(HarnessId::Codex, "0.9.0", "0.9.1", true),
        ],
        Some(&enabled),
    );
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].harness, HarnessId::ClaudeCode);
}

#[test]
fn failed_cli_checks_are_polled_again_until_they_succeed() {
    assert!(!harness_check_needs_retry(&[cli_status(
        HarnessId::Codex,
        "0.9.0",
        "0.9.0",
        false
    )]));
    let mut offline = cli_status(HarnessId::ClaudeCode, "2.1.309", "2.1.309", false);
    offline.latest_version = None;
    offline.check_error = Some("registry offline".into());
    assert!(harness_check_needs_retry(&[offline]));
}

#[test]
fn only_idle_or_failed_rows_are_updated_again() {
    let rows = HashMap::from([
        (HarnessId::ClaudeCode, CliUpdateRow::Idle),
        (HarnessId::Codex, CliUpdateRow::Updating),
        (HarnessId::Opencode, CliUpdateRow::Updated(None)),
        (HarnessId::Pi, CliUpdateRow::Failed("boom".into())),
    ]);
    assert!(cli_row_pending(&rows, HarnessId::ClaudeCode));
    assert!(!cli_row_pending(&rows, HarnessId::Codex));
    assert!(!cli_row_pending(&rows, HarnessId::Opencode));
    assert!(cli_row_pending(&rows, HarnessId::Pi));
    // A harness with no row yet is idle.
    assert!(cli_row_pending(&rows, HarnessId::Grok));
}
