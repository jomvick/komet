//! Render tests against ratatui's in-memory backend.
//!
//! The unit tests in `app`/`transcript` cover the logic; these cover the thing
//! logic tests can't see — that the frame actually contains what it should, at
//! the sizes a terminal really comes in, and that no pane writes outside its
//! area or panics on a degenerate one. A TUI's worst failures (an overflowing
//! row, a cursor placed off-screen, a panic at 20 columns) are all geometry
//! bugs, and this is where geometry is checked.

use chrono::Utc;
use komet_doc::{MessagePart, MessageRole, SessionMessageEntry};
use komet_proto::view::ConnectionStatus;
use komet_proto::{AuthState, Chat, Session, SessionStatus, Space, ToolCall, UserProfile};
use komet_tui::app::App;
use komet_tui::keys::{Action, Focus};
use komet_tui::link::Update;
use komet_tui::render;
use komet_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn space(id: &str, path: &str) -> Space {
    Space {
        id: id.into(),
        device_id: "dev".into(),
        path: path.into(),
        name: None,
        git_detected: true,
        git_checked_at: None,
        checkout_id: None,
        created_at: Utc::now(),
    }
}

fn chat(id: &str, title: &str) -> Chat {
    Chat {
        id: id.into(),
        device_id: "dev".into(),
        title: Some(title.into()),
        archived: false,
        cwd: Some("/dev/komet".into()),
        branch: Some("main".into()),
        checkout_id: None,
        config: None,
        last_message_preview: None,
        last_message_at: Some(Utc::now()),
        created_at: Utc::now(),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: Some("s1".into()),
        last_seen_at: Some(Utc::now()),
    }
}

fn entry(id: &str, role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts,
        created_at: Utc::now().timestamp_millis(),
        device_id: "dev".into(),
        status: None,
        continuation_of: None,
    }
}

fn text(id: &str, body: &str) -> MessagePart {
    MessagePart::Text {
        id: id.into(),
        text: body.into(),
    }
}

/// A signed-in app with one space, two sessions, and a transcript.
fn populated() -> App {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Auth(Box::new(AuthState::SignedIn {
        user: UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        },
        org_id: Some("org".into()),
    })));
    app.apply(Update::Spaces(vec![space("s1", "/dev/komet")]));
    app.apply(Update::Chats(vec![
        chat("c1", "Rework the diff sidebar"),
        chat("c2", "Chase the flaky room test"),
    ]));
    app.select_chat(Some("c1".into()));
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![
            entry(
                "m1",
                MessageRole::User,
                vec![text("t0", "why is the room test flaky?")],
            ),
            entry(
                "m2",
                MessageRole::Assistant,
                vec![
                    text("t0", "Let me look at the retry path."),
                    MessagePart::Tool {
                        id: "p1".into(),
                        call: ToolCall::Exec {
                            command: "cargo test -p komet-rpc device_room".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    text(
                        "t1",
                        "The join races the successor election. Here's the fix:\n\n```\nlet successor = peers.iter().filter(|p| !p.closing);\n```\n\n- drop the closing host\n- retry once",
                    ),
                ],
            ),
        ],
    });
    app
}

/// Draw once and return the frame as the terminal would actually show it.
///
/// A wide glyph occupies one cell and leaves a filler in the next, which
/// ratatui's diff skips rather than emitting. Reconstructing a row therefore has
/// to skip those cells too — reading every cell would report a column width
/// larger than the pane for any row containing CJK or emoji, and make the
/// overflow assertions below meaningless.
fn snapshot(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render::draw(frame, app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0u16;
            while x < width {
                let symbol = buffer[(x, y)].symbol();
                row.push_str(symbol);
                let advance = unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
                x += advance;
            }
            row.trim_end().to_string()
        })
        .collect()
}

fn joined(rows: &[String]) -> String {
    rows.join("\n")
}

#[test]
fn a_populated_frame_shows_the_chrome_sidebar_and_transcript() {
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 24);
    let screen = joined(&rows);

    // The tab strip is the header: the selected space's sessions, plus `+`,
    // with the engine label right-aligned.
    assert!(rows[0].contains("Rework the diff"), "{}", rows[0]);
    assert!(
        rows[0].contains("Chase the flaky"),
        "both tabs: {}",
        rows[0]
    );
    assert!(rows[0].trim_end().ends_with("engine ready"), "{}", rows[0]);

    // Sidebar: two sections.
    assert!(screen.contains("Spaces"), "{screen}");
    assert!(screen.contains("Sessions"), "{screen}");
    // Session rows carry a "space@device" sub-line under the title — where the
    // work happens, not which branch it happens on.
    assert!(screen.contains("komet@"), "sub-line missing:\n{screen}");

    // Transcript: the user turn, the assistant text, the tool row, the fenced
    // code, and the bullet.
    assert!(
        screen.contains("why is the room test flaky?"),
        "user text missing:\n{screen}"
    );
    // Tool calls collapse into a group summary; the newest group shows chips.
    assert!(
        screen.contains("Ran 1 command"),
        "tool group summary missing:\n{screen}"
    );
    assert!(screen.contains("Run"), "tool chip label missing:\n{screen}");
    assert!(
        screen.contains("cargo test -p komet-rpc"),
        "tool detail missing:\n{screen}"
    );
    assert!(
        screen.contains("let successor"),
        "fenced code missing:\n{screen}"
    );
    assert!(screen.contains("retry once"), "bullet missing:\n{screen}");

    // The user's own prompt is a right-aligned bubble, not a gutter marker.
    let prompt_row = rows
        .iter()
        .position(|row| row.contains("why is the room test flaky?"))
        .expect("the prompt");
    let indent = rows[prompt_row].find("why is").expect("prompt column");
    assert!(
        indent > 40,
        "the prompt should be right-aligned, found at column {indent}:\n{screen}"
    );

    // Hint bar: focus-aware — the fixture starts in the composer, where `q` is a
    // literal and Ctrl-C is the way out.
    let status = rows.last().unwrap();
    assert!(status.contains("Ctrl-C detach"), "{status}");
    app.focus = Focus::Transcript;
    let rows = snapshot(&mut app, 100, 24);
    assert!(
        rows.last().unwrap().contains("q detach"),
        "{:?}",
        rows.last()
    );
}

#[test]
fn no_row_ever_exceeds_the_terminal_width() {
    // The failure this guards against is a wide glyph or an un-truncated label
    // spilling into the next row, which corrupts the whole frame below it.
    let mut app = populated();
    for (width, height) in [(100u16, 24u16), (80, 30), (60, 12), (40, 10), (24, 8)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.area.width, width,
            "buffer resized under us at {width}x{height}"
        );
        // TestBackend panics on out-of-area writes, so reaching here already
        // proves containment; assert the cursor claim too.
        for y in 0..height {
            for x in 0..width {
                let _ = buffer[(x, y)];
            }
        }
    }
}

#[test]
fn degenerate_sizes_render_something_instead_of_panicking() {
    let mut app = populated();
    // Absurd but reachable: a user drags a split to nothing.
    for (width, height) in [(1u16, 1u16), (4, 2), (20, 6), (19, 5), (200, 3)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap_or_else(|err| panic!("{width}x{height} failed: {err}"));
    }
}

#[test]
fn wide_and_combining_glyphs_do_not_shift_the_layout() {
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![text(
                "t0",
                "日本語のテキストと絵文字 👩‍👩‍👧 と結合文字 e\u{301} が混ざった行",
            )],
        )],
    });
    // Narrow enough that the wide text must wrap mid-run. Every row has to fit
    // in the pane *by column*, which is the measure a terminal uses — a wrapper
    // that counted characters would overflow here.
    for width in [40u16, 33, 28] {
        for row in snapshot(&mut app, width, 16) {
            assert!(
                unicode_width::UnicodeWidthStr::width(row.as_str()) <= width as usize,
                "row wider than the {width}-column pane: {row:?}"
            );
        }
    }
}

#[test]
fn the_gate_replaces_the_body_while_the_engine_is_unreachable() {
    let mut app = App::with_theme(Theme::dark());
    // Boot: the first frame must already explain what is happening, because the
    // probe and a possible daemon spawn take a moment.
    let rows = snapshot(&mut app, 80, 20);
    let screen = joined(&rows);
    assert!(screen.contains("Starting the engine"), "{screen}");
    assert!(
        screen.contains("keep running"),
        "the detach promise belongs on the boot screen:\n{screen}"
    );

    app.apply(Update::Connection(ConnectionStatus::Failed(
        "no engine listening on 127.0.0.1:27654".into(),
    )));
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("Can't reach the engine"), "{screen}");
    assert!(
        screen.contains("27654"),
        "the reason must be shown:\n{screen}"
    );
    assert!(screen.contains("retry now"), "{screen}");

    // Signed out is a different gate with a different instruction.
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Auth(Box::new(AuthState::SignedOut)));
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("komet login"), "{screen}");
}

#[test]
fn the_help_overlay_covers_the_body_and_lists_the_real_bindings() {
    let mut app = populated();
    app.act(Action::ToggleHelp);
    let screen = joined(&snapshot(&mut app, 96, 30));
    assert!(screen.contains("Keys"), "{screen}");
    assert!(screen.contains("detach"), "{screen}");
    // A modal, not a floating panel: no transcript survives beside it, which is
    // what made it look like a rendering fault.
    assert!(
        !screen.contains("why is the room test flaky?"),
        "the overlay must cover the body:\n{screen}"
    );
    assert!(
        !screen.contains("cargo test -p komet-rpc"),
        "including the right-hand ends of long lines:\n{screen}"
    );
    // The tab strip still names the sessions; only the body is covered.
    assert!(screen.contains("Rework the diff"), "{screen}");
    // Every entry comes from the keymap's own table, so the overlay cannot drift
    // from the bindings.
    for (key, _) in komet_tui::keys::HELP.iter().take(3) {
        assert!(screen.contains(key.trim()), "missing {key:?} in:\n{screen}");
    }
}

#[test]
fn an_empty_workspace_says_what_to_do() {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("No session open"), "{screen}");
    assert!(screen.contains("No spaces yet"), "{screen}");
    assert!(screen.contains("No sessions yet"), "{screen}");
}

#[test]
fn hiding_the_sidebar_gives_its_columns_to_the_transcript() {
    let mut app = populated();
    // Tall enough for the whole fixture transcript: bottom-anchored content
    // clips at the top otherwise, which is correct but not what this checks.
    let with_sidebar = snapshot(&mut app, 80, 28);
    app.act(Action::ToggleSidebar);
    let without = snapshot(&mut app, 80, 28);
    assert_ne!(
        with_sidebar, without,
        "hiding the sidebar must change the frame"
    );
    let screen = joined(&without);
    // Sidebar-only chrome is gone…
    assert!(
        !screen.contains("Spaces") && !screen.contains("Sessions"),
        "the sidebar is hidden, so its sections must be gone:\n{screen}"
    );
    // …but the tab strip is the main panel's, so it stays.
    assert!(screen.contains("Chase the flaky"), "{screen}");
    // The transcript is still there, now wider.
    assert!(screen.contains("why is the room test flaky?"), "{screen}");
}

#[test]
fn the_composer_grows_with_its_content_and_places_the_caret() {
    let mut app = populated();
    app.focus = Focus::Composer;
    let one_line = snapshot(&mut app, 60, 20);

    for _ in 0..3 {
        app.act(Action::Edit(komet_tui::keys::Edit::Insert('x')));
        app.act(Action::Edit(komet_tui::keys::Edit::Newline));
    }
    let three_lines = snapshot(&mut app, 60, 20);
    assert_ne!(one_line, three_lines);
    let screen = joined(&three_lines);
    // The composer is a rule plus a prompt marker — no box, so nothing collides
    // with the sidebar divider.
    assert!(screen.contains('›'), "prompt marker missing:\n{screen}");
    assert!(
        !screen.contains('╭'),
        "the composer must not be boxed:\n{screen}"
    );

    // The caret is inside the frame — placing it outside is a real bug that
    // shows up as a cursor parked in a corner.
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let cursor = terminal.get_cursor_position().expect("cursor position");
    assert!(cursor.x < 60 && cursor.y < 20, "caret at {cursor:?}");
}

#[test]
fn a_notice_takes_over_the_status_line_then_gives_it_back() {
    let mut app = populated();
    app.notify("Couldn't send: engine unreachable".into());
    let rows = snapshot(&mut app, 80, 20);
    let status = rows.last().unwrap();
    assert!(status.contains("engine unreachable"), "{status}");

    app.notice.as_mut().unwrap().until = std::time::Instant::now();
    assert!(app.expire_notice());
    let rows = snapshot(&mut app, 80, 20);
    let status = rows.last().unwrap();
    assert!(status.contains("detach"), "hints must return: {status}");
}

#[test]
fn scrolling_up_is_announced_so_a_stalled_view_is_never_a_mystery() {
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: (0..60)
            .map(|i| {
                entry(
                    &format!("m{i}"),
                    MessageRole::Assistant,
                    vec![text("t0", "line")],
                )
            })
            .collect(),
    });
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(!screen.contains("Scrolled"), "following: no banner");

    app.act(Action::PageUp);
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("Scrolled back"), "{screen}");

    app.act(Action::ScrollBottom);
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(!screen.contains("Scrolled"), "{screen}");
}

#[test]
fn a_working_session_animates_and_an_idle_one_does_not() {
    let mut app = populated();
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::Working,
        started_at: None,
        updated_at: Utc::now(),
    }]));
    assert!(app.animating());
    // The loaders read a clock, so two draws a beat apart must differ —
    // otherwise the timer the event loop arms for them is wasted work.
    let first = snapshot(&mut app, 80, 20);
    std::thread::sleep(std::time::Duration::from_millis(140));
    let second = snapshot(&mut app, 80, 20);
    assert_ne!(first, second, "the loader must actually animate");
}

#[test]
fn drawing_twice_with_no_changes_touches_nothing() {
    // This is the property the whole design rests on: an idle app is free. If a
    // second identical draw produced a different buffer, ratatui's diff would
    // emit bytes every frame.
    let mut app = populated();
    assert!(
        !app.animating(),
        "this property only holds when nothing is animating"
    );
    let first = snapshot(&mut app, 90, 24);
    let second = snapshot(&mut app, 90, 24);
    assert_eq!(first, second);
}

#[test]
fn the_sidebar_follows_the_desktop_order() {
    // The reference sidebar reads: device, New session, rule, "Sessions", then a
    // faint space heading with its two-line session rows, and the user pinned to
    // the bottom (docs/reference/original-komet.png).
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 24);
    let sidebar: Vec<&str> = rows
        .iter()
        .map(|row| row.split('│').next().unwrap_or("").trim_end())
        .collect();

    let index = |needle: &str| {
        sidebar
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing from sidebar:\n{sidebar:#?}"))
    };
    // Two sections, in komet-native's order: Spaces, then a flat global
    // Sessions list. Not one list grouped by project — that was the original
    // Electron app.
    let spaces = index("Spaces");
    let space_row = index("komet ");
    let sessions = index("Sessions");
    let session = index("Rework the diff");
    assert!(
        spaces < space_row && space_row < sessions && sessions < session,
        "sidebar order wrong:\n{sidebar:#?}"
    );
    // The Spaces header carries its add affordance, right-aligned.
    assert!(sidebar[spaces].trim_end().ends_with('+'), "{sidebar:#?}");
    // Two-line session rows: the sub-line is directly under its title.
    assert!(
        sidebar[session + 1].contains("komet@"),
        "sub-line must follow the title:\n{sidebar:#?}"
    );
    // The user row is pinned to the bottom of the sidebar, not inline.
    let user = index("w@example.com");
    assert!(user > session, "user row must be last:\n{sidebar:#?}");
    assert!(
        user >= sidebar.len() - 3,
        "user row must be pinned to the bottom:\n{sidebar:#?}"
    );
}

#[test]
fn the_cursor_steps_over_decoration() {
    // Rules, the section header and the user row cannot hold the cursor; walking
    // the list must never leave it parked on nothing.
    let mut app = populated();
    app.focus = Focus::Sidebar;
    app.act(Action::ListTop);
    for _ in 0..40 {
        assert!(
            app.rows[app.cursor].selectable(),
            "cursor landed on {:?}",
            app.rows[app.cursor]
        );
        app.act(Action::ListDown);
    }
    for _ in 0..40 {
        assert!(app.rows[app.cursor].selectable());
        app.act(Action::ListUp);
    }
}

#[test]
fn tool_runs_collapse_into_a_group() {
    use komet_proto::ToolCall;
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![
            entry(
                "m1",
                MessageRole::Assistant,
                vec![
                    text("t0", "Checking a few things."),
                    MessagePart::Tool {
                        id: "p1".into(),
                        call: ToolCall::Exec {
                            command: "cargo test".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    MessagePart::Tool {
                        id: "p2".into(),
                        call: ToolCall::ReadFile {
                            path: "src/lib.rs".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    MessagePart::Tool {
                        id: "p3".into(),
                        call: ToolCall::EditFile {
                            path: "src/lib.rs".into(),
                            old_string: None,
                            new_string: None,
                        },
                        is_error: false,
                        resolved: true,
                    },
                ],
            ),
            entry("m2", MessageRole::Assistant, vec![text("t0", "Done.")]),
        ],
    });
    let screen = joined(&snapshot(&mut app, 100, 26));
    // One summary line for the whole run, using the shared wording.
    assert!(
        screen.contains("Ran 1 command · edited 1 file · read 1 file"),
        "group summary missing:\n{screen}"
    );
    // Not one row per tool: the individual paths stay folded away, because this
    // is no longer the newest group.
    assert!(
        !screen.contains("src/lib.rs"),
        "an older group must stay collapsed:\n{screen}"
    );
}

#[test]
fn a_running_tool_group_shows_its_chips() {
    use komet_proto::ToolCall;
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![MessagePart::Tool {
                id: "p1".into(),
                call: ToolCall::Exec {
                    command: "cargo build --workspace".into(),
                },
                is_error: false,
                // Still running: what it is doing now must be visible.
                resolved: false,
            }],
        )],
    });
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("⌄"), "expanded chevron missing:\n{screen}");
    assert!(
        screen.contains("cargo build --workspace"),
        "a running command must be shown:\n{screen}"
    );
}

#[test]
fn the_working_strip_reports_elapsed_time() {
    let mut app = populated();
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::Working,
        started_at: Some(Utc::now() - chrono::Duration::seconds(11)),
        updated_at: Utc::now(),
    }]));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("Working"), "{screen}");
    assert!(screen.contains("11s"), "elapsed time missing:\n{screen}");
    assert!(screen.contains("Ctrl-X"), "{screen}");
}

#[test]
fn the_space_row_names_its_host_device() {
    let mut app = populated();
    // The fixture's chats are hosted on "dev"; this engine is someone else.
    app.apply(Update::LocalDevice("laptop".into()));
    // The space row names the host device, which is where "whose machine is
    // this running on" lives in komet-native.
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("dev"), "host device missing:\n{screen}");

    // Hosted here: the row says so.
    app.apply(Update::LocalDevice("dev".into()));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("this device"), "{screen}");
}

#[test]
fn a_short_transcript_sits_above_the_composer() {
    // The desktop transcript is bottom-anchored (`ListAlignment::Bottom`): a new
    // conversation starts just above the composer, not floating at the top of an
    // empty pane with a gap beneath it.
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![text("t0", "one short reply")],
        )],
    });
    let rows = snapshot(&mut app, 100, 26);
    let at = rows
        .iter()
        .position(|row| row.contains("one short reply"))
        .expect("the reply");
    let composer = rows
        .iter()
        .position(|row| row.contains('›'))
        .expect("the composer");
    // Bottom-anchored: the reply sits low in the pane, a couple of rows above
    // the composer (its own trailing blank, then the status strip and rule).
    assert!(
        at > rows.len() / 2,
        "content floated to the top:\n{}",
        joined(&rows)
    );
    assert!(
        composer - at <= 6,
        "content should hug the composer: reply at {at}, composer at {composer}\n{}",
        joined(&rows)
    );
}

#[test]
fn the_user_bubble_stays_inside_the_pane() {
    // The wash is a background: one column of overflow would paint into the
    // pane edge (and, before the fix, past it).
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::User,
            vec![text(
                "t0",
                "a prompt long enough that it has to wrap inside the bubble at these widths",
            )],
        )],
    });
    for width in [100u16, 80, 60, 44] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // The last column of every row must never carry the bubble's wash.
        let washed = app.theme.raised;
        for y in 0..24u16 {
            let cell = &buffer[(width - 1, y)];
            assert_ne!(
                cell.bg, washed,
                "bubble reached the pane edge at {width}x24, row {y}"
            );
        }
    }
}

#[test]
fn a_selected_row_keeps_its_status_colour() {
    // The selection wash is a background; applying it in the wrong order made it
    // overwrite the dot's hue, so the selected session lost the one signal the
    // row exists to carry.
    let mut app = populated();
    app.focus = Focus::Sidebar;
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::AwaitingInput,
        started_at: None,
        updated_at: Utc::now(),
    }]));
    // Park the cursor on that session.
    app.cursor = app
        .rows
        .iter()
        .position(|row| row.id() == Some("c1"))
        .expect("the session row");

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    // The dot on the washed row — not just any dot in the sidebar.
    let dot = (0..24u16)
        .flat_map(|y| (0..30u16).map(move |x| (x, y)))
        .map(|(x, y)| &buffer[(x, y)])
        .find(|cell| cell.symbol() == komet_tui::theme::DOT && cell.bg == app.theme.selection)
        .expect("a status dot on the selected row");
    assert_eq!(
        dot.fg, app.theme.dot_awaiting,
        "the dot must keep its hue under the selection wash"
    );
    assert_eq!(dot.bg, app.theme.selection, "…and still sit on the wash");
}

#[test]
fn the_sessions_list_is_flat_and_global_not_grouped_by_space() {
    // komet-native's Sessions list answers "what needs me right now" across
    // every space. Grouping it by project — which the original Electron app did
    // — buries exactly that.
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        Space {
            id: "s2".into(),
            path: "/dev/beta".into(),
            ..space("s2", "/dev/beta")
        },
    ]));
    let mut in_beta = chat("c2", "Beta session");
    in_beta.space_id = Some("s2".into());
    in_beta.last_message_at = Some(Utc::now());
    let mut in_alpha = chat("c1", "Alpha session");
    in_alpha.space_id = Some("s1".into());
    in_alpha.last_message_at = Some(Utc::now() - chrono::Duration::hours(2));
    app.apply(Update::Chats(vec![in_alpha, in_beta]));

    let sidebar: Vec<String> = snapshot(&mut app, 100, 24)
        .iter()
        .map(|row| row.split('│').next().unwrap_or("").trim_end().to_string())
        .collect();
    let sessions = sidebar
        .iter()
        .position(|row| row.contains("Sessions"))
        .expect("Sessions header");
    let beta = sidebar.iter().position(|r| r.contains("Beta session"));
    let alpha = sidebar.iter().position(|r| r.contains("Alpha session"));
    let (beta, alpha) = (beta.expect("beta"), alpha.expect("alpha"));

    // Both sessions sit under the single Sessions header…
    assert!(beta > sessions && alpha > sessions, "{sidebar:#?}");
    // …in recency order, and adjacent: title + sub-line each, with no space
    // heading spliced between them.
    assert!(beta < alpha, "recency order:\n{sidebar:#?}");
    assert_eq!(
        alpha,
        beta + 2,
        "the rows must be consecutive (title, sub-line, title, sub-line):\n{sidebar:#?}"
    );
}

#[test]
fn the_tab_strip_shows_only_the_selected_spaces_sessions() {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        space("s2", "/dev/beta"),
    ]));
    let mut alpha = chat("c1", "Alpha session");
    alpha.space_id = Some("s1".into());
    let mut beta = chat("c2", "Beta session");
    beta.space_id = Some("s2".into());
    app.apply(Update::Chats(vec![alpha, beta]));

    app.selected_space = Some("s1".into());
    let rows = snapshot(&mut app, 100, 24);
    assert!(rows[0].contains("Alpha session"), "{}", rows[0]);
    assert!(
        !rows[0].contains("Beta session"),
        "another space's session must not be a tab: {}",
        rows[0]
    );

    // Switching space swaps the strip.
    app.selected_space = Some("s2".into());
    let rows = snapshot(&mut app, 100, 24);
    assert!(rows[0].contains("Beta session"), "{}", rows[0]);
    assert!(!rows[0].contains("Alpha session"), "{}", rows[0]);
}

#[test]
fn tabs_cycle_and_select_by_number() {
    let mut app = populated();
    app.selected_space = Some("s1".into());
    let first = app.selected_chat.clone().expect("a selection");

    app.act(Action::CycleTab(1));
    let second = app.selected_chat.clone().expect("a selection");
    assert_ne!(first, second, "next tab must move");

    // Wrapping: two tabs, so two steps returns.
    app.act(Action::CycleTab(1));
    assert_eq!(app.selected_chat, Some(first.clone()));
    app.act(Action::CycleTab(-1));
    assert_eq!(app.selected_chat, Some(second));

    // Direct selection is 1-based; out of range is a no-op, not a panic.
    app.act(Action::SelectTab(1));
    assert_eq!(app.selected_chat, Some(first));
    let before = app.selected_chat.clone();
    app.act(Action::SelectTab(9));
    assert_eq!(app.selected_chat, before);

    // The active tab carries the wash.
    let tabs = app.tabs();
    assert_eq!(tabs.iter().filter(|tab| tab.active).count(), 1);
}

#[test]
fn a_space_shows_its_host_and_flags_a_lapsed_one() {
    let mut app = populated();
    // Unknown device: no claim either way — silence beats a wrong "offline".
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(!screen.contains("offline"), "{screen}");

    // A device we DO have a record of, whose heartbeat lapsed, reads offline.
    app.apply(Update::Devices(vec![komet_proto::Device {
        id: "dev".into(),
        name: "devbox".into(),
        platform: "linux".into(),
        last_seen_at: Some(Utc::now() - chrono::Duration::minutes(5)),
        created_at: None,
    }]));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(
        screen.contains("offline"),
        "a lapsed host must show:\n{screen}"
    );

    // Fresh heartbeat: named, not flagged.
    app.apply(Update::Devices(vec![komet_proto::Device {
        id: "dev".into(),
        name: "devbox".into(),
        platform: "linux".into(),
        last_seen_at: Some(Utc::now()),
        created_at: None,
    }]));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("devbox"), "{screen}");
    assert!(!screen.contains("offline"), "{screen}");
}

/// The terminal column a byte offset falls on. `str::find` returns bytes, and
/// these rows are full of multi-byte glyphs (`●`, `│`, `…`), so the two are not
/// the same number — clicking the byte offset lands in the wrong place.
fn column_of(row: &str, byte_index: usize) -> u16 {
    unicode_width::UnicodeWidthStr::width(&row[..byte_index]) as u16
}

/// Draw, then find the cell of the first row containing `needle`.
fn cell_of(app: &mut App, width: u16, height: u16, needle: &str) -> (u16, u16) {
    let rows = snapshot(app, width, height);
    let y = rows
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", joined(&rows)));
    let x = column_of(&rows[y], rows[y].find(needle).unwrap_or(0));
    (x, y as u16)
}

#[test]
fn clicking_a_session_row_opens_it() {
    let mut app = populated();
    app.select_chat(Some("c1".into()));
    let (x, y) = cell_of(&mut app, 100, 24, "Chase the flaky");

    let effects = app.click(x, y);
    assert_eq!(
        app.selected_chat.as_deref(),
        Some("c2"),
        "clicking a row must open that session"
    );
    assert_eq!(app.focus, Focus::Composer, "opening focuses the prompt");
    assert!(
        effects.iter().any(
            |c| matches!(c, komet_tui::link::Command::WatchTranscript(Some(id)) if id == "c2")
        ),
        "and resubscribes its transcript"
    );
}

#[test]
fn clicking_a_space_activates_it_and_swaps_the_tabs() {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        space("s2", "/dev/beta"),
    ]));
    let mut alpha = chat("c1", "Alpha session");
    alpha.space_id = Some("s1".into());
    let mut beta = chat("c2", "Beta session");
    beta.space_id = Some("s2".into());
    app.apply(Update::Chats(vec![alpha, beta]));
    app.selected_space = Some("s1".into());

    let (x, y) = cell_of(&mut app, 100, 24, "beta");
    app.click(x, y);
    assert_eq!(app.selected_space.as_deref(), Some("s2"));
    let rows = snapshot(&mut app, 100, 24);
    assert!(rows[0].contains("Beta session"), "{}", rows[0]);
}

#[test]
fn clicking_a_tab_switches_to_it() {
    let mut app = populated();
    app.selected_space = Some("s1".into());
    app.select_chat(Some("c1".into()));
    // Tabs live on row 0; find the inactive one and click it.
    let (x, y) = cell_of(&mut app, 100, 24, "Chase the flaky");
    assert_eq!(y, 0, "tabs are the header row");
    app.click(x, y);
    assert_eq!(app.selected_chat.as_deref(), Some("c2"));
}

#[test]
fn clicking_plus_starts_a_session_and_the_panes_take_focus() {
    let mut app = populated();
    app.selected_space = Some("s1".into());
    let before = app.selected_chat.clone();
    // The tab strip's `+`, not the Spaces header's — both are on screen.
    let rows = snapshot(&mut app, 100, 24);
    let x = column_of(&rows[0], rows[0].rfind('+').expect("the tab strip's +"));
    app.click(x, 0);
    // `+` opens a draft, as the desktop canvas does — nothing is created until
    // the first send.
    assert!(app.draft.is_some(), "a draft was opened");
    assert_eq!(app.selected_chat, None, "the draft owns the pane");
    let _ = before;

    // Panes take focus when clicked.
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 24);
    let composer_y = rows.iter().position(|r| r.contains('›')).unwrap() as u16;
    app.focus = Focus::Sidebar;
    app.click(60, composer_y);
    assert_eq!(app.focus, Focus::Composer);

    app.click(60, 6); // in the transcript
    assert_eq!(app.focus, Focus::Transcript);

    // Sidebar decoration (a section header) takes focus without selecting —
    // only rows that can hold the cursor are targets.
    let rows = snapshot(&mut app, 100, 24);
    let header = rows
        .iter()
        .position(|row| row.starts_with(" Sessions"))
        .expect("the Sessions header") as u16;
    let before = app.selected_chat.clone();
    app.click(4, header);
    assert_eq!(app.focus, Focus::Sidebar);
    assert_eq!(app.selected_chat, before, "a header selects nothing");
}

#[test]
fn clicking_anywhere_dismisses_the_help_overlay() {
    let mut app = populated();
    app.act(Action::ToggleHelp);
    snapshot(&mut app, 100, 26);
    app.click(50, 12);
    assert!(!app.help, "a click must close the overlay");
    // And it must not have fallen through to whatever was underneath.
    assert_eq!(
        app.focus,
        Focus::Composer,
        "focus unchanged by the dismissal"
    );
}

#[test]
fn clicks_outside_any_target_do_nothing() {
    let mut app = populated();
    snapshot(&mut app, 100, 24);
    let before = (app.selected_chat.clone(), app.focus, app.cursor);
    // The hint bar is the last row and registers no target.
    app.click(10, 23);
    assert_eq!((app.selected_chat.clone(), app.focus, app.cursor), before);
    // Far outside the frame entirely.
    app.click(500, 500);
    assert_eq!((app.selected_chat.clone(), app.focus, app.cursor), before);
}

#[test]
fn the_spaces_plus_explains_itself_rather_than_doing_nothing() {
    // There is no folder picker in the TUI yet; the affordance is still drawn
    // (it is part of the desktop sidebar), so clicking it must say why.
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 24);
    let y = rows
        .iter()
        .position(|row| row.contains("Spaces"))
        .expect("the Spaces header");
    // The Spaces header shares row 0 with the tab strip, so take the FIRST `+`
    // on that row — the sidebar's — not the last, which belongs to the tabs.
    let x = column_of(&rows[y], rows[y].find('+').expect("the add affordance"));
    let y = y as u16;

    let before = app.selected_chat.clone();
    app.click(x, y);
    assert_eq!(app.selected_chat, before, "it must not open a session");
    let notice = app.notice.as_ref().expect("a notice");
    assert!(notice.text.contains("desktop app"), "{}", notice.text);
}

#[test]
fn right_click_opens_a_context_menu_with_the_desktop_verbs() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);

    let screen = joined(&snapshot(&mut app, 100, 26));
    for verb in ["Rename", "Archive", "Delete"] {
        assert!(screen.contains(verb), "{verb} missing:\n{screen}");
    }
    // Right-clicking targets the row under the pointer, not wherever the cursor
    // happened to be.
    assert_eq!(app.rows[app.cursor].id(), Some("c2"));

    // Esc dismisses without acting.
    let before = app.selected_chat.clone();
    app.act(Action::CloseOverlay);
    assert!(app.overlay.is_none());
    assert_eq!(app.selected_chat, before);
}

#[test]
fn a_space_menu_offers_space_verbs_only() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "komet ");
    app.right_click(x, y);
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Rename space"), "{screen}");
    assert!(screen.contains("Remove space"), "{screen}");
    assert!(!screen.contains("Archive"), "not a session verb:\n{screen}");
}

#[test]
fn archiving_from_the_menu_issues_the_mutation() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);
    // Walk to "Archive" and confirm.
    app.act(Action::OverlayStep(1));
    let effects = app.act(Action::OverlayConfirm);
    assert!(app.overlay.is_none(), "confirming closes the menu");
    match effects.first() {
        Some(komet_tui::link::Command::Call { params, .. }) => {
            assert_eq!(params["op"], "setChatArchived");
            assert_eq!(params["chatId"], "c2");
            assert_eq!(params["archived"], true);
        }
        other => panic!("expected setChatArchived, got {other:?}"),
    }
}

#[test]
fn renaming_opens_a_prompt_seeded_with_the_current_title() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);
    let effects = app.act(Action::OverlayConfirm); // "Rename…" is first
    assert!(effects.is_empty(), "opening a prompt makes no RPC");

    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Rename session"), "{screen}");
    assert!(
        screen.contains("Chase the flaky"),
        "prompt should start from the current title:\n{screen}"
    );

    // Type and confirm.
    for ch in " v2".chars() {
        app.act(Action::OverlayEdit(komet_tui::keys::Edit::Insert(ch)));
    }
    let effects = app.act(Action::OverlayConfirm);
    match effects.first() {
        Some(komet_tui::link::Command::Call { params, .. }) => {
            assert_eq!(params["op"], "renameChat");
            assert_eq!(params["title"], "Chase the flaky room test v2");
        }
        other => panic!("expected renameChat, got {other:?}"),
    }
    assert!(app.overlay.is_none());
}

#[test]
fn an_empty_rename_is_refused_rather_than_wiping_the_title() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);
    app.act(Action::OverlayConfirm); // open the prompt
    app.act(Action::OverlayEdit(
        komet_tui::keys::Edit::DeleteToLineStart,
    ));
    app.act(Action::OverlayEdit(komet_tui::keys::Edit::End));
    let effects = app.act(Action::OverlayConfirm);
    assert!(effects.is_empty(), "an empty name must not be submitted");
    assert!(app.overlay.is_some(), "and the prompt stays open");
}

#[test]
fn the_model_picker_lists_and_switches() {
    use komet_proto::{Model, ReasoningLevel};
    let mut app = populated();
    let effects = app.act(Action::PickModel);
    assert!(
        effects
            .iter()
            .any(|c| matches!(c, komet_tui::link::Command::ListModels { .. })),
        "asks the engine for the catalogue"
    );
    // Until it answers, the picker says so rather than showing an empty list.
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Loading"), "{screen}");

    app.apply(Update::Models(vec![
        Model {
            id: "fable-5".into(),
            label: "Fable 5".into(),
            description: Some("balanced".into()),
            reasoning_levels: vec![ReasoningLevel::High],
            options: vec![],
        },
        Model {
            id: "opus-5".into(),
            label: "Opus 5".into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        },
    ]));
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Fable 5"), "{screen}");
    assert!(screen.contains("balanced"), "descriptions show:\n{screen}");

    app.act(Action::OverlayStep(1));
    let effects = app.act(Action::OverlayConfirm);
    match effects.first() {
        Some(komet_tui::link::Command::Call { params, .. }) => {
            assert_eq!(params["op"], "setChatConfig");
            assert_eq!(params["config"]["model"], "opus-5");
        }
        other => panic!("expected setChatConfig, got {other:?}"),
    }
    // The chip updates immediately rather than waiting for the round trip.
    assert!(
        app.composer_chips()
            .iter()
            .any(|(_, label)| label == "opus-5"),
        "{:?}",
        app.composer_chips()
    );
}

#[test]
fn a_failed_model_fetch_closes_the_picker() {
    let mut app = populated();
    app.act(Action::PickModel);
    assert!(app.overlay.is_some());
    app.apply(Update::Notice("Couldn't list models: boom".into()));
    assert!(
        app.overlay.is_none(),
        "a waiting picker must not hang on failure"
    );
    assert!(app.notice.is_some());
}

#[test]
fn rules_join_the_sidebar_divider_instead_of_leaving_a_notch() {
    // A rule that starts one column right of the divider reads as two loose
    // strokes; the tee is what makes it a frame.
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 26);
    let tees = rows.iter().filter(|row| row.contains('├')).count();
    assert!(tees >= 2, "expected joined rules:\n{}", joined(&rows));
    // And no double vertical: the composer is not boxed.
    assert!(
        !joined(&rows).contains("│╭"),
        "a box abutting the divider:\n{}",
        joined(&rows)
    );
}

#[test]
fn a_draft_shows_its_pending_tab_and_where_it_will_run() {
    let mut app = populated();
    app.act(Action::NewSession);
    let rows = snapshot(&mut app, 100, 28);
    let screen = joined(&rows);

    // A pending tab, so the pane is not simply blank.
    assert!(rows[0].contains("New session"), "{}", rows[0]);
    // The chips say where it will run — the choice only exists before send.
    assert!(
        screen.contains("Current checkout"),
        "checkout chip missing:\n{screen}"
    );
}

#[test]
fn the_checkout_picker_offers_two_modes_and_names_the_third_outcome() {
    use komet_proto::RepoRef;
    let mut app = populated();
    app.act(Action::NewSession);
    app.apply(Update::Refs(vec![RepoRef {
        name: "main".into(),
        current: true,
        worktree_path: None,
    }]));

    app.act(Action::PickCheckout);
    let screen = joined(&snapshot(&mut app, 100, 28));
    assert!(screen.contains("Run in"), "{screen}");
    assert!(screen.contains("Current checkout"), "{screen}");
    assert!(screen.contains("New worktree"), "{screen}");

    // With a materialized ref picked, the same Local row reads "Current
    // worktree" — two modes, three outcomes.
    app.act(Action::CloseOverlay);
    app.apply(Update::Refs(vec![RepoRef {
        name: "feat".into(),
        current: true,
        worktree_path: Some("/wt/feat".into()),
    }]));
    app.act(Action::PickCheckout);
    let screen = joined(&snapshot(&mut app, 100, 28));
    assert!(screen.contains("Current worktree"), "{screen}");
    assert!(!screen.contains("Current checkout"), "{screen}");
}

#[test]
fn the_branch_picker_tags_current_and_materialized_refs() {
    use komet_proto::RepoRef;
    let mut app = populated();
    app.act(Action::NewSession);
    // While loading it says so rather than showing an empty list.
    app.act(Action::PickRef);
    assert!(joined(&snapshot(&mut app, 100, 28)).contains("Loading"));

    app.apply(Update::Refs(vec![
        RepoRef {
            name: "main".into(),
            current: true,
            worktree_path: None,
        },
        RepoRef {
            name: "feat".into(),
            current: false,
            worktree_path: Some("/wt/feat".into()),
        },
    ]));
    let screen = joined(&snapshot(&mut app, 100, 28));
    assert!(screen.contains("Branch"), "{screen}");
    assert!(screen.contains("current"), "current tag missing:\n{screen}");
    assert!(
        screen.contains("worktree"),
        "worktree tag missing:\n{screen}"
    );

    // A space that is not a git checkout says so.
    app.act(Action::CloseOverlay);
    app.apply(Update::Refs(vec![]));
    app.act(Action::PickRef);
    assert!(joined(&snapshot(&mut app, 100, 28)).contains("Not a git checkout"));
}

#[test]
fn the_prompt_has_air_around_it() {
    // One row wedged between a rule and the hint bar reads as an afterthought.
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 28);
    let prompt = rows
        .iter()
        .position(|row| row.contains('›'))
        .expect("the prompt");
    let rule = rows
        .iter()
        .rposition(|row| row.contains('├'))
        .expect("the composer rule");
    assert!(
        prompt > rule + 1,
        "no air above the prompt:\n{}",
        joined(&rows)
    );
    // And a blank row below it, before the hint bar. Only the main pane matters
    // — the row still carries the sidebar divider.
    let below = rows[prompt + 1].split('│').nth(1).unwrap_or("").trim();
    assert!(
        below.is_empty(),
        "no air below the prompt: {below:?}\n{}",
        joined(&rows)
    );
}

#[test]
fn every_loader_on_screen_is_the_same_one() {
    // A running session is drawn in three places — the sidebar row, its tab and
    // the working strip. They must not be three different animations.
    let mut app = populated();
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::Working,
        started_at: Some(Utc::now()),
        updated_at: Utc::now(),
    }]));
    let (expected, _) = komet_tui::loaders::mini_spinner(app.elapsed());
    let screen = joined(&snapshot(&mut app, 100, 28));
    let count = screen.matches(expected.as_str()).count();
    assert!(
        count >= 3,
        "expected the same glyph in row, tab and strip; saw {count} of {expected:?}\n{screen}"
    );
}

#[test]
fn each_chip_is_its_own_button() {
    use komet_tui::app::ChipKind;
    let mut app = populated();
    app.act(Action::NewSession);
    snapshot(&mut app, 110, 28);

    // Four distinct targets, not one lumped region.
    let kinds: Vec<ChipKind> = app
        .composer_chips()
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert!(kinds.contains(&ChipKind::Branch), "{kinds:?}");
    assert!(kinds.contains(&ChipKind::Checkout), "{kinds:?}");
    assert!(kinds.contains(&ChipKind::Model), "{kinds:?}");

    // Clicking each opens *its own* picker, not always the model one.
    let rows = snapshot(&mut app, 110, 28);
    let y = rows
        .iter()
        .position(|row| row.contains("Current checkout"))
        .expect("the chip row") as u16;
    let x = column_of(
        &rows[y as usize],
        rows[y as usize].find("Current checkout").unwrap(),
    );
    app.click(x, y);
    assert!(
        joined(&snapshot(&mut app, 110, 28)).contains("Run in"),
        "the checkout chip must open the checkout picker"
    );
    app.act(Action::CloseOverlay);

    let rows = snapshot(&mut app, 110, 28);
    let y = rows
        .iter()
        .position(|row| row.contains("Select ref"))
        .expect("the branch chip") as u16;
    let x = column_of(
        &rows[y as usize],
        rows[y as usize].find("Select ref").unwrap(),
    );
    app.click(x, y);
    assert!(
        joined(&snapshot(&mut app, 110, 28)).contains("Branch"),
        "the branch chip must open the branch picker"
    );
}

#[test]
fn effort_is_pickable_and_follows_the_model() {
    use komet_proto::{Model, ReasoningLevel};
    let mut app = populated();
    app.act(Action::NewSession);

    // Default levels when no catalogue entry is known yet.
    app.act(Action::PickReasoning);
    let screen = joined(&snapshot(&mut app, 110, 28));
    assert!(screen.contains("Effort"), "{screen}");
    assert!(screen.contains("High"), "{screen}");
    app.act(Action::OverlayStep(3));
    app.act(Action::OverlayConfirm);
    assert_eq!(
        app.draft.as_ref().unwrap().reasoning,
        Some(ReasoningLevel::High)
    );
    // …and it shows as a chip.
    assert!(
        app.composer_chips()
            .iter()
            .any(|(_, label)| label == "High")
    );

    // Choosing a model whose levels exclude the current one moves it to a level
    // that model actually offers, rather than sending an unsupported value.
    app.act(Action::PickModel);
    app.apply(Update::Models(vec![Model {
        id: "small".into(),
        label: "Small".into(),
        description: None,
        reasoning_levels: vec![ReasoningLevel::Low],
        options: vec![],
    }]));
    app.act(Action::OverlayConfirm);
    assert_eq!(
        app.draft.as_ref().unwrap().reasoning,
        Some(ReasoningLevel::Low),
        "effort must stay within the model's levels"
    );
}

#[test]
fn a_drafts_model_and_effort_ride_the_session_it_creates() {
    use komet_proto::{Model, ReasoningLevel};
    let mut app = populated();
    app.act(Action::NewSession);
    app.act(Action::PickModel);
    app.apply(Update::Models(vec![Model {
        id: "opus-5".into(),
        label: "Opus 5".into(),
        description: None,
        reasoning_levels: vec![ReasoningLevel::High],
        options: vec![],
    }]));
    app.act(Action::OverlayConfirm);

    app.composer.set_text("go");
    let effects = app.act(Action::Send);
    let start = effects
        .iter()
        .find_map(|c| match c {
            komet_tui::link::Command::StartSession(start) => Some(start),
            _ => None,
        })
        .expect("a StartSession");
    let config = start.config.as_ref().expect("the draft's config");
    assert_eq!(config["model"], "opus-5");
    assert_eq!(config["reasoning"], "high");
}

#[test]
fn the_caret_is_drawn_so_a_trailing_space_is_visible() {
    // Relying on the terminal's own cursor makes a trailing space invisible:
    // the row is rendered without it, so nothing tells you it is there.
    let mut app = populated();
    app.focus = Focus::Composer;
    for ch in "hi ".chars() {
        app.act(Action::Edit(komet_tui::keys::Edit::Insert(ch)));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let cursor = terminal.get_cursor_position().expect("a caret");
    let cell = &terminal.backend().buffer()[(cursor.x, cursor.y)];
    assert_eq!(
        cell.bg, app.theme.text,
        "the caret cell must be painted, not left to the terminal"
    );
    assert_eq!(cell.symbol(), " ", "and it sits on the space just typed");

    // One column left is the 'i' — so the block really is past the space.
    let before = &terminal.backend().buffer()[(cursor.x - 1, cursor.y)];
    assert_eq!(before.symbol(), "i");
}

/// Two spaces, two sessions each, signed in.
fn two_spaces() -> App {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        space("s2", "/dev/beta"),
    ]));
    let mut rows = Vec::new();
    for (id, title, space_id, age) in [
        ("a1", "Alpha one", "s1", 1i64),
        ("a2", "Alpha two", "s1", 2),
        ("b1", "Beta one", "s2", 3),
        ("b2", "Beta two", "s2", 4),
    ] {
        let mut chat = chat(id, title);
        chat.space_id = Some(space_id.into());
        chat.last_message_at = Some(Utc::now() - chrono::Duration::minutes(age));
        rows.push(chat);
    }
    app.apply(Update::Chats(rows));
    app
}

#[test]
fn switching_spaces_returns_you_to_where_you_were() {
    let mut app = two_spaces();

    // Be somewhere specific in each space.
    app.activate_space("s1".into());
    app.select_chat(Some("a2".into()));
    app.activate_space("s2".into());
    app.select_chat(Some("b2".into()));

    // Coming back lands on the session you left, not the newest one.
    app.activate_space("s1".into());
    assert_eq!(app.selected_chat.as_deref(), Some("a2"));
    app.activate_space("s2".into());
    assert_eq!(app.selected_chat.as_deref(), Some("b2"));
}

#[test]
fn a_space_you_have_never_opened_lands_on_its_newest_session() {
    let mut app = two_spaces();
    app.activate_space("s2".into());
    // `chats` is recency-sorted, so the newest of s2 is b1.
    assert_eq!(app.selected_chat.as_deref(), Some("b1"));
    assert!(app.draft.is_none());
}

#[test]
fn an_empty_space_opens_the_new_session_canvas() {
    let mut app = two_spaces();
    // Archive everything in s2, leaving it empty.
    let chats: Vec<komet_proto::Chat> = app
        .chats
        .iter()
        .cloned()
        .map(|mut chat| {
            if chat.space_id.as_deref() == Some("s2") {
                chat.archived = true;
            }
            chat
        })
        .collect();
    app.apply(Update::Chats(chats));

    app.activate_space("s2".into());
    assert!(
        app.draft.is_some(),
        "an empty space must not leave a dead pane"
    );
    assert_eq!(app.selected_chat, None);
    assert_eq!(app.draft.as_ref().unwrap().space_id, "s2");
}

#[test]
fn a_remembered_session_that_vanished_falls_back() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.select_chat(Some("a2".into()));

    // a2 is deleted elsewhere.
    let chats: Vec<komet_proto::Chat> = app
        .chats
        .iter()
        .filter(|chat| chat.id != "a2")
        .cloned()
        .collect();
    app.apply(Update::Chats(chats));

    app.activate_space("s2".into());
    app.activate_space("s1".into());
    assert_eq!(
        app.selected_chat.as_deref(),
        Some("a1"),
        "a stale memory must fall back to the newest, not to nothing"
    );
}

#[test]
fn clicking_a_space_row_restores_its_session() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.select_chat(Some("a2".into()));
    app.activate_space("s2".into());

    let (x, y) = cell_of(&mut app, 100, 28, "alpha");
    app.click(x, y);
    assert_eq!(app.selected_space.as_deref(), Some("s1"));
    assert_eq!(app.selected_chat.as_deref(), Some("a2"));
}

#[test]
fn a_draft_survives_the_chats_frames_that_arrive_while_you_type() {
    // The regression: `heal_chat_selection` treats "no chat selected" as "pick
    // one", but a draft is *deliberately* in that state. Chats frames arrive
    // constantly while any session streams, so the canvas was being yanked away
    // — and because the draft stayed set, the next send created a session
    // instead of continuing the one you had been moved to.
    let mut app = two_spaces();
    app.act(Action::NewSession);
    let space = app.draft.as_ref().unwrap().space_id.clone();

    for _ in 0..5 {
        let chats = app.chats.clone();
        app.apply(Update::Chats(chats));
        assert!(app.draft.is_some(), "the draft must survive a chats frame");
        assert_eq!(
            app.selected_chat, None,
            "and nothing may be selected under it"
        );
    }
    assert_eq!(app.draft.as_ref().unwrap().space_id, space);

    // Sending now starts the draft — one new session, not a stray one.
    app.composer.set_text("go");
    let effects = app.act(Action::Send);
    assert_eq!(
        effects
            .iter()
            .filter(|c| matches!(c, komet_tui::link::Command::StartSession(_)))
            .count(),
        1
    );
}

#[test]
fn sending_in_an_open_session_never_creates_a_new_one() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.select_chat(Some("a1".into()));

    // Even after opening a draft and then picking a real session again, the
    // draft must be gone — the two states are mutually exclusive.
    app.act(Action::NewSession);
    assert!(app.draft.is_some());
    app.select_chat(Some("a1".into()));
    assert!(app.draft.is_none(), "opening a session abandons the draft");

    app.composer.set_text("continue");
    let effects = app.act(Action::Send);
    assert!(
        !effects
            .iter()
            .any(|c| matches!(c, komet_tui::link::Command::StartSession(_))),
        "a send in an open session must continue it"
    );
    match effects.first() {
        Some(komet_tui::link::Command::Send { chat_id, .. }) => assert_eq!(chat_id, "a1"),
        other => panic!("expected a Send to a1, got {other:?}"),
    }
}

#[test]
fn clicking_a_session_while_drafting_switches_to_it() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.act(Action::NewSession);
    let (x, y) = cell_of(&mut app, 100, 28, "Alpha two");
    app.click(x, y);
    assert!(app.draft.is_none());
    assert_eq!(app.selected_chat.as_deref(), Some("a2"));
}
