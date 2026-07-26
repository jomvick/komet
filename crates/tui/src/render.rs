//! Drawing.
//!
//! The information architecture is komet-native's, read from the desktop shell
//! (`komet-ui/src/shell.rs::render_chat_sidebar`, `shell/spaces.rs`,
//! `shell/tabs.rs`) — **not** the original Electron app's single grouped list:
//!
//! - the sidebar has **two sections**, Spaces and a *flat global* Sessions list;
//! - the selected space's own sessions are the **tab strip** above the
//!   transcript, which is also the header (it replaced one).
//!
//! The visual language is herdr's, since that is the terminal app this lives
//! beside: no boxes and no frames — one vertical divider, one horizontal rule,
//! a section label on the left with its affordance right-aligned on the same
//! row, single-column insets, and a lot of air.
//!
//! ```text
//!  Spaces                +  │  ● Ratatui terminal…   ● Diff sidebar…   +
//!  ▪ komet-native  this dev │ ─────────────────────────────────────────────
//!  ▪ soccertcg     this dev │                       ╭───────────────────╮
//!                           │                       │ the user's prompt │
//!  Sessions                 │                       ╰───────────────────╯
//!  ● Ratatui terminal…  now │                                      14:32
//!    komet-native · komet/… │  The assistant replies as plain text.
//!  ● Rebalance player…   2h │
//!    soccertcg · komet/re…  │  ⌄ Ran 2 commands
//!                           │  │ Run   cargo test --workspace
//!  ────────────────────     │  ◜ Working · 11s · Ctrl-X to interrupt
//!  Wing Lee                 │ ╭─────────────────────────────────────────╮
//!  w@example.com            │ │ Do anything…                            │
//!                           │ ╰──────────────────── Fable 5 · High ─────╯
//! ```
//!
//! Two habits keep the per-frame cost flat:
//!
//! - **Only visible rows are built.** The sidebar walks its row list until the
//!   pane is full; the transcript hands out cached lines by reference
//!   ([`Transcript::for_each_visible`]).
//! - **Almost nothing paints a background.** ratatui diffs the buffer and emits
//!   only changed cells, so leaving cells at the terminal default is the
//!   difference between repainting a pane and repainting nothing — as well as
//!   letting the user's own background (and its transparency) show through.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};

use komet_proto::view::{ConnectionStatus, GatePhase};

use crate::app::{App, Hit, Overlay, Row};
use crate::daemon::Attachment;
use crate::keys::{Focus, HELP};
use crate::loaders;
use crate::theme::{self, Theme};
use crate::wrap;

/// Sidebar width. Wide enough for a title plus a time column and a readable
/// "project · branch" sub-line, capped so a narrow terminal keeps a usable
/// transcript.
const SIDEBAR_MAX: u16 = 30;
const SIDEBAR_MIN: u16 = 20;
/// The composer grows with its content up to this many text rows.
const COMPOSER_MAX_ROWS: u16 = 8;
/// Reading measure for the transcript. The original caps its content column
/// rather than letting prose run the full width of a wide window.
const CONTENT_MAX: u16 = 96;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let area = frame.area();
    if area.width < 24 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("terminal too small").style(theme.subtle()),
            area,
        );
        return;
    }

    // A cell grid has no widget tree, so clicks resolve against a map the draw
    // rebuilds as it goes.
    app.clear_hits();

    let [body, hints] = Layout::vertical([Constraint::Min(6), Constraint::Length(1)]).areas(area);

    let sidebar_width = if app.sidebar_visible {
        SIDEBAR_MAX
            .min(body.width / 3)
            .max(SIDEBAR_MIN.min(body.width / 2))
    } else {
        0
    };
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(12)]).areas(body);

    if sidebar_width > 0 {
        app.push_hit(sidebar, Hit::Pane(Focus::Sidebar));
        draw_sidebar(frame, sidebar, app, &theme);
    }
    draw_main(frame, main, app, &theme);
    draw_hints(frame, hints, app, &theme);

    if let Some(overlay) = &app.overlay {
        let panel = draw_overlay(frame, body, app, overlay, &theme);
        app.push_hit(body, Hit::Overlay);
        let _ = panel;
    }

    if app.help {
        // Everything below the top row: the header (and the sidebar's device row,
        // which shares that row) stay readable, so you keep your place while
        // reading the key map.
        let panel = Rect {
            y: body.y + 1,
            height: body.height.saturating_sub(1),
            ..body
        };
        // Registered last, so it wins over everything beneath it.
        app.push_hit(panel, Hit::Overlay);
        draw_help(frame, panel, &theme);
    }
}

/// A status dot. A running session shows komet's 2×3 mini gradient spinner —
/// one braille cell, the same ring chase the desktop session rows animate —
/// while every other state is the static dot, coloured by meaning.
fn status_dot(
    status: komet_proto::ChatIndicator,
    app: &App,
    theme: &Theme,
    base: Style,
) -> (String, Style) {
    if status == komet_proto::ChatIndicator::Working {
        let (glyph, tint) = loaders::mini_spinner(app.elapsed());
        return (glyph, base.patch(Style::default().fg(tint)));
    }
    (theme::DOT.to_string(), base.patch(theme.dot(status)))
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // A single hairline divider — the original has no boxes, just this edge.
    let block =
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(if app.focus == Focus::Sidebar {
                theme.subtle()
            } else {
                theme.rule()
            });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width < 4 {
        return;
    }

    // The user row is pinned to the bottom, as in the original; everything else
    // scrolls above it.
    let user = app
        .rows
        .iter()
        .position(|row| matches!(row, Row::User { .. }));
    let (list_rows, footer_rows): (&[Row], &[Row]) = match user {
        Some(at) => (&app.rows[..at], &app.rows[at..]),
        None => (&app.rows, &[]),
    };
    let footer_height: u16 = footer_rows.iter().map(|row| row.height()).sum();
    let list_height = inner.height.saturating_sub(footer_height);

    // Scroll so the cursor's row is visible. Rows are 1–2 lines tall, so the
    // window is computed in lines, not indices.
    let first = first_visible(list_rows, app.cursor, list_height);
    // Collected before drawing, because drawing needs `&App` while registering
    // needs `&mut App`.
    let mut visible: Vec<(usize, Rect)> = Vec::new();
    let mut y = inner.y;
    for (index, row) in list_rows.iter().enumerate().skip(first) {
        let height = row.height();
        if y + height > inner.y + list_height {
            break;
        }
        visible.push((index, Rect { y, height, ..inner }));
        y += height;
    }
    let footer_start = inner.y + list_height;
    let mut footer: Vec<(usize, Rect)> = Vec::new();
    let mut y = footer_start;
    for (offset, row) in footer_rows.iter().enumerate() {
        footer.push((
            list_rows.len() + offset,
            Rect {
                y,
                height: row.height(),
                ..inner
            },
        ));
        y += row.height();
    }

    for (index, slot) in visible.iter().chain(footer.iter()) {
        if app.rows[*index].selectable() {
            app.push_hit(*slot, Hit::Row(*index));
        }
        // The Spaces header's affordance is its own target.
        if let Row::Section {
            action: Some(_),
            label,
        } = &app.rows[*index]
            && label == "Spaces"
        {
            let width = 3u16.min(slot.width);
            app.push_hit(
                Rect {
                    x: slot.x + slot.width - width,
                    width,
                    ..*slot
                },
                Hit::AddSpace,
            );
        }
    }
    for (index, slot) in visible.into_iter().chain(footer) {
        let selected = index == app.cursor && index < app.rows.len();
        let row = app.rows[index].clone();
        draw_sidebar_row(frame, slot, app, &row, selected, theme);
    }
}

/// First row index to draw so `cursor` fits in `height` lines.
fn first_visible(rows: &[Row], cursor: usize, height: u16) -> usize {
    let mut first = 0usize;
    loop {
        let mut used = 0u16;
        let mut visible_end = first;
        for row in &rows[first..] {
            if used + row.height() > height {
                break;
            }
            used += row.height();
            visible_end += 1;
        }
        if cursor < visible_end || first + 1 >= rows.len() {
            return first;
        }
        first += 1;
    }
}

fn draw_sidebar_row(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    row: &Row,
    selected: bool,
    theme: &Theme,
) {
    let width = area.width as usize;
    match row {
        // Label left, affordance right — herdr's header row.
        Row::Section { label, action } => {
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {label}"), theme.label())),
                area,
            );
            if let Some(action) = action {
                let action_width = wrap::width_of(action) as u16 + 1;
                if action_width < area.width {
                    frame.render_widget(
                        Paragraph::new(Span::styled(format!("{action} "), theme.hint())),
                        Rect {
                            x: area.x + area.width - action_width,
                            width: action_width,
                            ..area
                        },
                    );
                }
            }
        }
        Row::Space {
            label,
            device,
            attention,
            offline,
            ..
        } => {
            let base = if selected {
                theme.selected()
            } else {
                Style::default()
            };
            if selected {
                frame.render_widget(Paragraph::new("").style(theme.selected()), area);
            }
            // The attention dot only appears when a member session is live, so a
            // quiet space stays quiet.
            let (dot, dot_style) = match attention {
                Some(status) => status_dot(*status, app, theme, base),
                None => (" ".to_string(), base),
            };
            let device = if *offline {
                format!("{device} · offline")
            } else {
                device.clone()
            };
            let device_width = wrap::width_of(&device);
            let label_width = width.saturating_sub(4 + device_width).max(1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ".to_string(), base),
                    Span::styled(dot.to_string(), dot_style),
                    Span::styled(
                        format!(" {}", wrap::truncate(label, label_width)),
                        base.patch(if selected {
                            theme.body()
                        } else {
                            theme.subtle()
                        }),
                    ),
                ])),
                area,
            );
            let tail = device_width as u16 + 1;
            if tail < area.width {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!("{device} "),
                        base.patch(if *offline {
                            Style::default().fg(theme.warning)
                        } else {
                            theme.hint()
                        }),
                    )),
                    Rect {
                        x: area.x + area.width - tail,
                        width: tail,
                        ..area
                    },
                );
            }
        }
        Row::Blank => {}
        Row::Empty { label } => {
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {label}"), theme.hint())),
                area,
            );
        }
        Row::Rule => {
            // Full width, then a tee into the divider — an inset rule leaves the
            // same notch the horizontal rules used to.
            frame.render_widget(
                Paragraph::new(Span::styled("─".repeat(width), theme.rule())),
                area,
            );
            frame.render_widget(
                Paragraph::new(Span::styled("┤", theme.rule())),
                Rect {
                    x: area.x + area.width,
                    width: 1,
                    height: 1,
                    ..area
                },
            );
        }
        Row::Chat {
            id,
            title,
            location,
            indicator,
            archived,
            activity,
            ..
        } => {
            let base = if selected {
                theme.selected()
            } else {
                Style::default()
            };
            if selected {
                for offset in 0..area.height {
                    frame.render_widget(
                        Paragraph::new("").style(theme.selected()),
                        Rect {
                            y: area.y + offset,
                            height: 1,
                            ..area
                        },
                    );
                }
            }
            let open = app.selected_chat.as_deref() == Some(id.as_str());
            let when = app.relative_time(*activity);
            // inset(1) + dot(1) + gap(1) + a column of air + the time column.
            let title_width = width.saturating_sub(5 + wrap::width_of(&when)).max(1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ".to_string(), base),
                    {
                        let (glyph, style) = status_dot(*indicator, app, theme, base);
                        Span::styled(glyph, style)
                    },
                    Span::styled(
                        format!(" {}", wrap::truncate(title, title_width)),
                        base.patch(if open { theme.body() } else { theme.subtle() }),
                    ),
                ])),
                Rect { height: 1, ..area },
            );
            let time_width = wrap::width_of(&when) as u16 + 1;
            if time_width < area.width {
                frame.render_widget(
                    Paragraph::new(Span::styled(format!("{when} "), base.patch(theme.hint()))),
                    Rect {
                        x: area.x + area.width - time_width,
                        width: time_width,
                        height: 1,
                        ..area
                    },
                );
            }
            // The sub-line sits under the title, aligned past the dot.
            if area.height > 1 {
                let mut sub = location.clone().unwrap_or_default();
                if *archived {
                    sub = if sub.is_empty() {
                        "archived".into()
                    } else {
                        format!("{sub} · archived")
                    };
                }
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!("   {}", wrap::truncate(&sub, width.saturating_sub(4))),
                        base.patch(theme.hint()),
                    )),
                    Rect {
                        y: area.y + 1,
                        height: 1,
                        ..area
                    },
                );
            }
        }
        Row::User { name, email } => {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}", wrap::truncate(name, width.saturating_sub(2))),
                    theme.subtle(),
                )),
                Rect { height: 1, ..area },
            );
            if area.height > 1 && !email.is_empty() && email != name {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!(" {}", wrap::truncate(email, width.saturating_sub(2))),
                        theme.hint(),
                    )),
                    Rect {
                        y: area.y + 1,
                        height: 1,
                        ..area
                    },
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main panel
// ---------------------------------------------------------------------------

fn draw_main(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // The tab strip IS the header — in the desktop shell it replaced one
    // (`shell/tabs.rs`), so there is no separate title row.
    let [tabs, rule, rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
    ])
    .areas(area);
    draw_tab_strip(frame, tabs, app, theme);
    draw_rule(frame, rule, app, theme);

    match app.gate() {
        GatePhase::Ready => {}
        phase => {
            draw_gate(frame, rest, app, theme, phase);
            return;
        }
    }

    // The prompt gets air above and below it: at one row tall, wedged between a
    // rule and the hint bar, it reads as a cramped afterthought rather than the
    // thing you are meant to type into.
    let text_rows = composer_rows(app, rest.width);
    let [transcript, strip, divider, pad_top, composer, pad_bottom] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(text_rows),
        Constraint::Length(1),
    ])
    .areas(rest);
    let _ = (pad_top, pad_bottom);

    app.push_hit(transcript, Hit::Pane(Focus::Transcript));
    app.push_hit(composer, Hit::Pane(Focus::Composer));
    draw_transcript(frame, transcript, app, theme);
    draw_status_strip(frame, strip, app, theme);
    draw_rule(frame, divider, app, theme);
    draw_composer(frame, composer, app, theme);
}

/// A full-width horizontal rule that *joins* the sidebar divider.
///
/// Without the tee, a rule that starts one column right of the divider reads as
/// two unrelated lines with a notch between them. Drawing `├` in the divider's
/// column is the difference between a frame and a set of loose strokes.
fn draw_rule(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled("─".repeat(area.width as usize), theme.rule())),
        area,
    );
    if app.sidebar_visible && area.x > 0 {
        frame.render_widget(
            Paragraph::new(Span::styled("├", theme.rule())),
            Rect {
                x: area.x - 1,
                width: 1,
                height: 1,
                ..area
            },
        );
    }
}

/// Width of one tab. The desktop strip uses a fixed 140px; a fixed column width
/// is the same idea, and keeps tabs from jittering as titles stream in.
const TAB_WIDTH: usize = 22;

/// The session tab strip: every non-archived session of the selected space,
/// with `+` to start another. The active tab carries the selection wash.
fn draw_tab_strip(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let engine = engine_label(app);
    let engine_width = wrap::width_of(&engine) as u16;
    // Reserve the engine label first: whether quitting leaves work running is
    // the one thing about this app a user must never have to guess.
    let strip_width = area.width.saturating_sub(engine_width + 1);

    let tabs = app.tabs();
    if tabs.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                match app.selected_space {
                    Some(_) => "  No sessions here — n starts one",
                    None => "  No space selected",
                },
                theme.hint(),
            )),
            Rect {
                width: strip_width.max(1),
                ..area
            },
        );
    } else {
        // Scroll so the active tab is visible; `+` always trails the last tab.
        let visible = (strip_width as usize / TAB_WIDTH).max(1);
        let active = tabs.iter().position(|tab| tab.active).unwrap_or(0);
        let first = active.saturating_sub(visible.saturating_sub(1));
        let mut x = area.x;
        for (offset, tab) in tabs.iter().enumerate().skip(first).take(visible) {
            let remaining = (area.x + strip_width).saturating_sub(x);
            let slot = Rect {
                x,
                width: (TAB_WIDTH as u16).min(remaining),
                height: 1,
                ..area
            };
            if slot.width < 6 {
                break;
            }
            app.push_hit(slot, Hit::Tab(offset));
            let base = if tab.active {
                theme.selected()
            } else {
                Style::default()
            };
            if tab.active {
                frame.render_widget(Paragraph::new("").style(theme.selected()), slot);
            }
            let title_width = (slot.width as usize).saturating_sub(4);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ".to_string(), base),
                    {
                        let (glyph, style) = status_dot(tab.indicator, app, theme, base);
                        Span::styled(glyph, style)
                    },
                    Span::styled(
                        format!(" {}", wrap::truncate(&tab.title, title_width)),
                        base.patch(if tab.active {
                            theme.body()
                        } else {
                            theme.subtle()
                        }),
                    ),
                ])),
                slot,
            );
            x += slot.width;
        }
        if x + 3 <= area.x + strip_width {
            let plus = Rect {
                x,
                width: 3,
                height: 1,
                ..area
            };
            app.push_hit(plus, Hit::NewSession);
            frame.render_widget(Paragraph::new(Span::styled(" + ", theme.hint())), plus);
        }
    }

    if engine_width + 2 < area.width {
        frame.render_widget(
            Paragraph::new(Span::styled(engine, theme.hint())),
            Rect {
                x: area.x + area.width - engine_width,
                width: engine_width,
                ..area
            },
        );
    }
}

fn engine_label(app: &App) -> String {
    match (&app.connection, &app.attachment) {
        (ConnectionStatus::Connecting, _) => "connecting… ".to_string(),
        (ConnectionStatus::Failed(_), _) => "engine down ".to_string(),
        (ConnectionStatus::Ready, Some(Attachment::Spawned { pid })) => {
            format!("engine {pid} · started here ")
        }
        (ConnectionStatus::Ready, Some(Attachment::Attached)) => "engine attached ".to_string(),
        (ConnectionStatus::Ready, None) => "engine ready ".to_string(),
    }
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // Cap the reading measure, as the original caps its content column.
    let column = Rect {
        width: area.width.min(CONTENT_MAX),
        ..area
    };
    // Lay out against the real column before reading anything from it; this is
    // the only place the transcript learns its width, so a resize lands here.
    app.lay_out_transcript(column.width, column.height as usize);

    if app.selected_chat.is_none() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::default(),
                Line::from(Span::styled("  No session open.", theme.subtle())),
                Line::from(Span::styled("  Press n to start one.", theme.hint())),
            ]),
            column,
        );
        return;
    }
    if app.transcript.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Nothing here yet.",
                theme.hint(),
            ))),
            column,
        );
        return;
    }

    let buffer = frame.buffer_mut();
    app.transcript
        .for_each_visible(column.height as usize, |row, line| {
            // Rendered by reference: drawing a frame clones no line and
            // allocates no `Text`.
            line.render(
                Rect {
                    y: column.y + row,
                    height: 1,
                    ..column
                },
                buffer,
            );
        });
}

/// The reserved strip above the composer (the original's `h-6`): the working
/// indicator, or the scroll notice. Reserved even when empty so the composer
/// never shifts under the cursor.
fn draw_status_strip(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if let Some(elapsed) = app.working_elapsed() {
        let seconds = elapsed.as_secs();
        // The same indicator the session rows and tabs use. One running session
        // must not be drawn two different ways depending on where you look.
        let (glyph, tint) = loaders::mini_spinner(app.elapsed());
        let mut spans = vec![
            Span::raw(" ".to_string()),
            Span::styled(glyph, Style::default().fg(tint)),
        ];
        spans.push(Span::styled(" Working".to_string(), theme.subtle()));
        spans.push(Span::styled(
            format!(" · {seconds}s · Ctrl-X to interrupt"),
            theme.hint(),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    if !app.transcript.following() {
        frame.render_widget(
            Paragraph::new(Span::styled(" Scrolled back · G to follow", theme.hint())),
            area,
        );
    }
}

/// How many text rows the prompt wants, clamped.
fn composer_rows(app: &App, width: u16) -> u16 {
    let inner = composer_text_width(width);
    let rows = app.composer.lay_out(inner as usize).rows.len() as u16;
    rows.clamp(1, COMPOSER_MAX_ROWS)
}

/// Text width inside the composer: the pane, less the prompt marker and a
/// trailing column of air.
fn composer_text_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
}

fn draw_composer(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    if area.height == 0 || area.width < 4 {
        return;
    }
    let focused = app.focus == Focus::Composer;

    // The chips ride the rule above, right-aligned — the terminal analogue of
    // the desktop pill's inline chips. Each is its own click target: they open
    // different pickers, so one lumped region would send every click to the
    // same place.
    let chips = app.composer_chips();
    if !chips.is_empty() {
        let widths: Vec<u16> = chips
            .iter()
            .map(|(_, label)| wrap::width_of(label) as u16 + 2)
            .collect();
        let total: u16 = widths.iter().sum::<u16>() + widths.len() as u16 + 1;
        if total + 4 < area.width {
            let mut x = area.x + area.width - total;
            for ((kind, label), width) in chips.iter().zip(&widths) {
                let slot = Rect {
                    x,
                    y: area.y - 1,
                    width: *width,
                    height: 1,
                };
                app.push_hit(slot, Hit::Chip(*kind));
                // A wash makes it read as a button rather than as text that
                // happens to sit on a rule.
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!(" {label} "),
                        Style::default().fg(theme.muted).bg(theme.raised),
                    )),
                    slot,
                );
                x += width + 1;
            }
        }
    }

    // "› " prompt marker, then the text.
    frame.render_widget(
        Paragraph::new(Span::styled(
            " › ",
            if focused {
                theme.subtle()
            } else {
                theme.hint()
            },
        )),
        Rect {
            width: 3,
            height: 1,
            ..area
        },
    );
    let text_area = Rect {
        x: area.x + 3,
        width: area.width.saturating_sub(4),
        ..area
    };

    if app.composer.is_empty() {
        // The original's placeholder, verbatim.
        frame.render_widget(
            Paragraph::new(Span::styled("Do anything…", theme.hint())),
            text_area,
        );
        if focused {
            draw_caret(frame, text_area.x, text_area.y, theme);
            frame.set_cursor_position(Position {
                x: text_area.x,
                y: text_area.y,
            });
        }
        return;
    }

    let laid = app.composer.lay_out(text_area.width as usize);
    let visible = text_area.height as usize;
    let first = laid.cursor_row.saturating_sub(visible.saturating_sub(1));
    for (offset, row) in laid.rows.iter().skip(first).take(visible).enumerate() {
        frame.render_widget(
            Paragraph::new(Span::styled(row.clone(), theme.body())),
            Rect {
                y: text_area.y + offset as u16,
                height: 1,
                ..text_area
            },
        );
    }
    if focused {
        let row = laid.cursor_row.saturating_sub(first) as u16;
        if row < text_area.height {
            let x = text_area.x + (laid.cursor_col as u16).min(text_area.width.saturating_sub(1));
            let y = text_area.y + row;
            draw_caret(frame, x, y, theme);
            frame.set_cursor_position(Position { x, y });
        }
    }
}

/// Paint the caret cell ourselves.
///
/// The terminal's own cursor is still placed (screen readers and cursor-shape
/// settings follow it), but relying on it alone makes a *trailing space*
/// invisible: the row is drawn without it, so there is nothing to tell you
/// whether you typed one. A block on the caret cell answers that at a glance.
fn draw_caret(frame: &mut Frame, x: u16, y: u16, theme: &Theme) {
    let area = frame.area();
    if x >= area.right() || y >= area.bottom() {
        return;
    }
    let cell = Rect {
        x,
        y,
        width: 1,
        height: 1,
    };
    let under = frame.buffer_mut()[(x, y)].symbol().to_string();
    let glyph = if under.trim().is_empty() {
        " ".to_string()
    } else {
        under
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            glyph,
            Style::default().fg(theme.selection).bg(theme.text),
        )),
        cell,
    );
}

// ---------------------------------------------------------------------------
// Hints, gates, overlays
// ---------------------------------------------------------------------------

fn draw_hints(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    " {}",
                    wrap::truncate(&wrap::sanitize(&notice.text), area.width as usize)
                ),
                Style::default().fg(theme.warning),
            )),
            area,
        );
        return;
    }

    // Focus-aware, because in the composer every letter is text: advertising
    // "q detach" there would be a lie that types a `q`.
    let composing = app.focus == Focus::Composer;
    let mut hints: Vec<(&str, &str)> = vec![match app.focus {
        Focus::Composer => ("Enter", "send"),
        Focus::Sidebar => ("Enter", "open"),
        Focus::Transcript => ("i", "prompt"),
    }];
    hints.push(("Tab", "pane"));
    if !composing {
        hints.push(("n", "new"));
        hints.push(("?", "help"));
    }
    hints.push(("Ctrl-X", "stop"));
    hints.push(if composing {
        ("Ctrl-C", "detach")
    } else {
        ("q", "detach")
    });

    let mut spans = vec![Span::raw(" ".to_string())];
    for (key, what) in hints {
        if spans.len() > 1 {
            spans.push(Span::raw("   ".to_string()));
        }
        spans.push(Span::styled(key.to_string(), theme.subtle()));
        spans.push(Span::styled(format!(" {what}"), theme.hint()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_gate(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, phase: GatePhase) {
    let (title, body): (String, Vec<String>) = match phase {
        GatePhase::Loading => (
            "Starting the engine…".into(),
            vec![
                "Attaching to a running engine, or starting one if none is up.".into(),
                "It will keep running after you close this terminal.".into(),
            ],
        ),
        GatePhase::Failed(err) => (
            "Can't reach the engine".into(),
            err.lines().map(str::to_string).collect(),
        ),
        GatePhase::SignIn => (
            "Sign in".into(),
            vec![
                "The engine has no session.".into(),
                "Run `komet login` in another terminal, then `komet daemon restart`.".into(),
            ],
        ),
        GatePhase::OrgGate => (
            "Choose a workspace".into(),
            vec![
                "This account has no workspace selected.".into(),
                "Pick one in the desktop app; the TUI follows the engine's choice.".into(),
            ],
        ),
        GatePhase::Ready => return,
    };

    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!("  {title}"),
            theme.body().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    let width = area.width.saturating_sub(4) as usize;
    for paragraph in body {
        for chunk in wrap::wrap(&wrap::sanitize(&paragraph), width, "") {
            lines.push(Line::from(Span::styled(
                format!("  {chunk}"),
                theme.subtle(),
            )));
        }
    }
    lines.push(Line::default());
    if matches!(app.connection, ConnectionStatus::Connecting) {
        let mut spans = vec![Span::raw("  ".to_string())];
        spans.extend(loaders::komet_wave(app.elapsed(), theme.text));
        lines.push(Line::from(spans));
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        "  r  retry now      q  quit",
        theme.hint(),
    )));
    frame.render_widget(Paragraph::new(lines), area);

    // While connecting, the animated komet mark sits above the message — the
    // desktop app's boot splash, at terminal resolution.
    if matches!(app.connection, ConnectionStatus::Connecting) {
        let mark = loaders::komet_mark(app.elapsed(), theme);
        let height = mark.len() as u16;
        let width = 14u16;
        if area.height > height + 8 && area.width > width + 4 {
            frame.render_widget(
                Paragraph::new(mark),
                Rect {
                    x: area.x + 2,
                    y: area.y + area.height - height - 1,
                    width,
                    height,
                },
            );
        }
    }
}

fn draw_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    // Clear the whole body, not just the panel. A centered panel over live
    // content leaves the right-hand ends of long transcript lines floating
    // beside it, which reads as a rendering fault rather than as a modal.
    frame.render_widget(Clear, area);

    let widest = HELP
        .iter()
        .map(|(key, what)| wrap::width_of(key) + 2 + wrap::width_of(what))
        .max()
        .unwrap_or(40) as u16;
    let width = (widest + 4).min(area.width.saturating_sub(2));
    let height = (HELP.len() as u16 + 2).min(area.height);
    let panel = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.rule())
        .title(Span::styled(" Keys ", theme.label()));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let key_column = HELP
        .iter()
        .map(|(key, _)| wrap::width_of(key))
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = HELP
        .iter()
        .take(inner.height as usize)
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("{key:>key_column$}  "), theme.subtle()),
                Span::styled(
                    wrap::truncate(what, (inner.width as usize).saturating_sub(key_column + 2)),
                    theme.hint(),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Floating panels
// ---------------------------------------------------------------------------

/// A right-click menu, the model picker, or a rename prompt.
///
/// All three are the same card: a rounded hairline box on a cleared rect. Only
/// the menu is anchored at the pointer — the others are centred, because they
/// are about the session rather than about the row you happened to click.
fn draw_overlay(
    frame: &mut Frame,
    body: Rect,
    app: &App,
    overlay: &Overlay,
    theme: &Theme,
) -> Rect {
    match overlay {
        Overlay::Menu {
            title,
            items,
            active,
            column,
            row,
        } => {
            let width = items
                .iter()
                .map(|item| wrap::width_of(&item.label))
                .chain(std::iter::once(wrap::width_of(title)))
                .max()
                .unwrap_or(12) as u16
                + 4;
            let separators = items.iter().filter(|item| item.separated).count() as u16;
            let height = items.len() as u16 + separators + 2;
            // Flip the anchor when the menu would run off an edge.
            let x = (*column)
                .min(body.right().saturating_sub(width + 1))
                .max(body.x);
            let y = (*row + 1)
                .min(body.bottom().saturating_sub(height))
                .max(body.y);
            let panel = Rect {
                x,
                y,
                width: width.min(body.width),
                height: height.min(body.height),
            };
            let block = card(theme, title);
            let inner = block.inner(panel);
            frame.render_widget(Clear, panel);
            frame.render_widget(block, panel);

            let mut y = inner.y;
            for (index, item) in items.iter().enumerate() {
                if item.separated && y < inner.bottom() {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            "─".repeat(inner.width as usize),
                            theme.rule(),
                        )),
                        Rect {
                            y,
                            height: 1,
                            ..inner
                        },
                    );
                    y += 1;
                }
                if y >= inner.bottom() {
                    break;
                }
                let slot = Rect {
                    y,
                    height: 1,
                    ..inner
                };
                let selected = index == *active;
                if selected {
                    frame.render_widget(Paragraph::new("").style(theme.selected()), slot);
                }
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!(" {}", item.label),
                        if selected {
                            theme.selected().patch(theme.body())
                        } else {
                            theme.subtle()
                        },
                    )),
                    slot,
                );
                y += 1;
            }
            panel
        }

        Overlay::Refs { active } => {
            let refs = app_refs(app);
            let rows: Vec<(String, Option<String>)> = match &refs {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|candidate| {
                        // The same two tags the desktop ref picker shows.
                        let mut tags = Vec::new();
                        if candidate.current {
                            tags.push("current");
                        }
                        if candidate.worktree_path.is_some() {
                            tags.push("worktree");
                        }
                        (
                            candidate.name.clone(),
                            (!tags.is_empty()).then(|| tags.join(" · ")),
                        )
                    })
                    .collect(),
                Some(_) => vec![("Not a git checkout".into(), None)],
                None => vec![("Loading…".into(), None)],
            };
            list_card(frame, body, theme, "Branch", &rows, refs.is_some(), *active)
        }

        Overlay::Checkout { active } => {
            // Two modes, three outcomes: "current" reads differently depending
            // on whether the picked ref is already a worktree.
            let picked = app
                .draft
                .as_ref()
                .and_then(|draft| draft.selected_ref().cloned());
            let local = komet_proto::view::checkout_label(
                komet_proto::view::CheckoutKind::Local,
                picked.as_ref(),
            );
            let rows = vec![
                (
                    local.to_string(),
                    Some("run where the space already points".into()),
                ),
                (
                    "New worktree".to_string(),
                    Some("isolated checkout off the picked ref".into()),
                ),
            ];
            list_card(frame, body, theme, "Run in", &rows, true, *active)
        }

        Overlay::Reasoning { levels, active } => {
            let rows: Vec<(String, Option<String>)> = levels
                .iter()
                .map(|level| (crate::app::reasoning_label(*level).to_string(), None))
                .collect();
            list_card(frame, body, theme, "Effort", &rows, true, *active)
        }

        Overlay::Models { models, active } => {
            let rows: Vec<(String, Option<String>)> = match models {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|model| (model.label.clone(), model.description.clone()))
                    .collect(),
                Some(_) => vec![("No models for this harness".into(), None)],
                None => vec![("Loading…".into(), None)],
            };
            let width = 52u16.min(body.width.saturating_sub(4));
            let height = (rows.len() as u16 + 2).min(body.height.saturating_sub(2));
            let panel = centred(body, width, height);
            let block = card(theme, "Model");
            let inner = block.inner(panel);
            frame.render_widget(Clear, panel);
            frame.render_widget(block, panel);

            for (index, (label, description)) in rows.iter().enumerate() {
                let y = inner.y + index as u16;
                if y >= inner.bottom() {
                    break;
                }
                let slot = Rect {
                    y,
                    height: 1,
                    ..inner
                };
                let selected = models.is_some() && index == *active;
                if selected {
                    frame.render_widget(Paragraph::new("").style(theme.selected()), slot);
                }
                let mut spans = vec![Span::styled(
                    format!(" {label}"),
                    if selected {
                        theme.selected().patch(theme.body())
                    } else {
                        theme.subtle()
                    },
                )];
                if let Some(description) = description {
                    let room = (inner.width as usize).saturating_sub(wrap::width_of(label) + 3);
                    if room > 4 {
                        spans.push(Span::styled(
                            format!("  {}", wrap::truncate(description, room)),
                            theme.hint(),
                        ));
                    }
                }
                frame.render_widget(Paragraph::new(Line::from(spans)), slot);
            }
            panel
        }

        Overlay::Prompt { title, input, .. } => {
            let width = 52u16.min(body.width.saturating_sub(4));
            let panel = centred(body, width, 4);
            let block = card(theme, title);
            let inner = block.inner(panel);
            frame.render_widget(Clear, panel);
            frame.render_widget(block, panel);
            let laid = input.lay_out(inner.width.saturating_sub(2) as usize);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}", laid.rows.first().cloned().unwrap_or_default()),
                    theme.body(),
                )),
                Rect { height: 1, ..inner },
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " Enter to confirm · Esc to cancel",
                    theme.hint(),
                )),
                Rect {
                    y: inner.y + 1,
                    height: 1,
                    ..inner
                },
            );
            let caret_x = inner.x + 1 + (laid.cursor_col as u16).min(inner.width.saturating_sub(2));
            draw_caret(frame, caret_x, inner.y, theme);
            frame.set_cursor_position(Position {
                x: caret_x,
                y: inner.y,
            });
            panel
        }
    }
}

/// The draft's branches, if a draft is open.
fn app_refs(app: &App) -> Option<Vec<komet_proto::RepoRef>> {
    app.draft.as_ref().and_then(|draft| draft.refs.clone())
}

/// A centred list panel: label plus a muted trailing detail per row.
/// `selectable` is false while a list is still loading, so nothing highlights.
fn list_card(
    frame: &mut Frame,
    body: Rect,
    theme: &Theme,
    title: &str,
    rows: &[(String, Option<String>)],
    selectable: bool,
    active: usize,
) -> Rect {
    let width = 56u16.min(body.width.saturating_sub(4));
    let height = (rows.len() as u16 + 2).min(body.height.saturating_sub(2));
    let panel = centred(body, width, height);
    let block = card(theme, title);
    let inner = block.inner(panel);
    frame.render_widget(Clear, panel);
    frame.render_widget(block, panel);

    for (index, (label, detail)) in rows.iter().enumerate() {
        let y = inner.y + index as u16;
        if y >= inner.bottom() {
            break;
        }
        let slot = Rect {
            y,
            height: 1,
            ..inner
        };
        let selected = selectable && index == active;
        if selected {
            frame.render_widget(Paragraph::new("").style(theme.selected()), slot);
        }
        let mut spans = vec![Span::styled(
            format!(" {label}"),
            if selected {
                theme.selected().patch(theme.body())
            } else {
                theme.subtle()
            },
        )];
        if let Some(detail) = detail {
            let room = (inner.width as usize).saturating_sub(wrap::width_of(label) + 3);
            if room > 4 {
                spans.push(Span::styled(
                    format!("  {}", wrap::truncate(detail, room)),
                    theme.hint(),
                ));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), slot);
    }
    panel
}

/// The shared card chrome: rounded hairline box with a title.
fn card<'a>(theme: &Theme, title: &'a str) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.rule())
        .title(Span::styled(format!(" {title} "), theme.label()))
}

fn centred(body: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: body.x + (body.width.saturating_sub(width)) / 2,
        y: body.y + (body.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}
