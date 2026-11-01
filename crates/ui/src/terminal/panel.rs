//! The terminal panel: session-scoped tabs over engine PTYs.
//!
//! Feature-inventory §1.10: tabs are per selected chat and restored on return
//! (emulators — and their server-side PTYs — survive navigation; detach is not
//! close). Tab bar supports pointer drag-reorder with 150 ms sliding
//! transforms, middle-click close, and a "+" new-tab button. Cmd/Ctrl+J
//! toggles the panel (the shell owns the height animation + persistence).
//!
//! Chrome shortcuts (key context "Terminal", bound in [`init`]): mod+shift+T
//! new tab, mod+shift+W close, mod+shift+] / [ cycle, F2 rename, mod+= / - /
//! 0 font zoom, mod+shift+Up / Down walk the OSC 133 command prompts and
//! mod+shift+O jumps to the output of the command under the viewport top. Tab
//! labels prefer a rename, then the OSC title, then `cwd · shell`.
//!
//! Data path per tab: `OpenTerminal` → `SubscribeTerminal` stream; Data frames
//! (base64) feed the [`Emulator`]; query responses write back; the stream
//! reconnects with exponential backoff resuming from `afterSeq`; Exit appends
//! the "[process exited N]" line and stops. Keyboard bytes coalesce for 12 ms
//! before `WriteTerminal`; viewport-driven resizes debounce 80 ms before
//! `ResizeTerminal` (the emulator resizes immediately).

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyBinding, KeyDownEvent,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task, Window, actions, div,
    prelude::*, px,
};

use komet_proto::{TerminalEvent, TerminalSession};
use komet_rpc::methods;

use crate::motion::{self, AnimationExt as _, TAB_SLIDE};
use crate::settings::{
    TERMINAL_FONT_SIZE_DEFAULT, TERMINAL_FONT_SIZE_MAX, TERMINAL_FONT_SIZE_MIN, TERMINAL_MAX_VH,
    TERMINAL_MIN_HEIGHT, TERMINAL_SCROLLBACK_DEFAULT, platform_combo,
};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

use super::emulator::{
    CellSnapshot, CommandBlock, CursorSnapshot, Emulator, GridPoint, MouseModes, MouseProtocol,
    SelectionType, Side,
};
use super::view::{
    COALESCE_MS, InputCoalescer, MouseRoute, PointerEvent, PointerMods, RESIZE_DEBOUNCE_MS,
    ReportButton, SELECTION_DRAG_THRESHOLD, TerminalElement, cell_at, keystroke_bytes,
    line_height_for, paste_bytes, route_pointer, terminal_panel_bg,
};

/// Fixed tab width — drag-reorder math stays analytic.
pub const TAB_WIDTH: f32 = 118.0;
pub const TAB_BAR_HEIGHT: f32 = 40.0;

actions!(
    terminal,
    [
        ToggleTerminal,
        NewTerminalTab,
        CloseTerminalTab,
        NextTerminalTab,
        PrevTerminalTab,
        RenameTerminalTab,
        IncreaseTerminalFont,
        DecreaseTerminalFont,
        ResetTerminalFont,
        PrevTerminalCommand,
        NextTerminalCommand,
        CurrentCommandOutput
    ]
);

/// Bind the terminal keymap: Cmd+J on macOS, Ctrl+J elsewhere for the toggle
/// (global — the shell's own keymap binds the customizable variant), and the
/// tab/font chrome ([`chrome_bindings`]) on the "Terminal" focus context so
/// they only steal keys while the grid is focused. Called from
/// `Shell::apply_keymap` after every `clear_key_bindings`, and at startup.
pub fn init(cx: &mut App) {
    let toggle = if cfg!(target_os = "macos") {
        "cmd-j"
    } else {
        "ctrl-j"
    };
    let mut bindings = vec![KeyBinding::new(toggle, ToggleTerminal, None)];
    bindings.extend(chrome_bindings());
    cx.bind_keys(bindings);
}

/// The fixed tab/font chrome shortcuts, bound on the "Terminal" key context
/// (`Shell::apply_keymap` re-binds these after clearing the keymap, so they
/// must never also be bound by `init` in the same session). Also the OSC 133
/// command-navigation set.
pub fn chrome_bindings() -> Vec<KeyBinding> {
    let ctx = Some("Terminal");
    vec![
        KeyBinding::new(&platform_combo("mod-shift-t"), NewTerminalTab, ctx),
        KeyBinding::new(&platform_combo("mod-shift-w"), CloseTerminalTab, ctx),
        KeyBinding::new(&platform_combo("mod-shift-]"), NextTerminalTab, ctx),
        KeyBinding::new(&platform_combo("mod-shift-["), PrevTerminalTab, ctx),
        KeyBinding::new("f2", RenameTerminalTab, ctx),
        KeyBinding::new(&platform_combo("mod-="), IncreaseTerminalFont, ctx),
        KeyBinding::new(&platform_combo("mod--"), DecreaseTerminalFont, ctx),
        KeyBinding::new(&platform_combo("mod-0"), ResetTerminalFont, ctx),
        // OSC 133 command navigation. Deliberately platform-modified: a bare
        // Up/Down belongs to the shell (history recall, `less`), and these must
        // never reach the PTY — see `action_command_jump`.
        KeyBinding::new(&platform_combo("mod-shift-up"), PrevTerminalCommand, ctx),
        KeyBinding::new(&platform_combo("mod-shift-down"), NextTerminalCommand, ctx),
        KeyBinding::new(&platform_combo("mod-shift-o"), CurrentCommandOutput, ctx),
    ]
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Panel height clamp: 160 px … 55 % of the viewport (§1.10).
pub fn clamp_terminal_height(height: f32, viewport_h: f32) -> f32 {
    let max = (viewport_h * TERMINAL_MAX_VH).max(TERMINAL_MIN_HEIGHT);
    if height.is_finite() {
        height.clamp(TERMINAL_MIN_HEIGHT, max)
    } else {
        TERMINAL_MIN_HEIGHT
    }
}

/// Reconnect backoff: 500 ms doubling to an 8 s ceiling.
pub fn backoff_ms(attempt: u32) -> u64 {
    (500u64 << attempt.min(4)).min(8_000)
}

/// Move a tab from `from` to `to` (indices into the same vec).
pub fn reorder_tabs<T>(tabs: &mut Vec<T>, from: usize, to: usize) {
    if from >= tabs.len() || to >= tabs.len() || from == to {
        return;
    }
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
}

/// Where a drag hovering at `rel_x` inside the tab strip would land.
pub fn drop_index(rel_x: f32, tab_w: f32, count: usize) -> usize {
    if count == 0 || tab_w <= 0.0 {
        return 0;
    }
    ((rel_x / tab_w).floor().max(0.0) as usize).min(count - 1)
}

/// Sliding transform (in tab-width units) for tab `ix` while `from` is dragged
/// over `over`: tabs between the two shift one slot toward the vacated gap.
pub fn slide_offset(ix: usize, from: usize, over: usize) -> f32 {
    if from < over && ix > from && ix <= over {
        -1.0
    } else if over < from && ix >= over && ix < from {
        1.0
    } else {
        0.0
    }
}

/// Active index after a reorder commit.
pub fn active_after_reorder(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// Map a gpui pointer button onto the encoder's button vocabulary. Extra
/// buttons (forward/back) have no terminal report, hence `None`.
fn report_button_of(button: MouseButton) -> Option<ReportButton> {
    match button {
        MouseButton::Left => Some(ReportButton::Left),
        MouseButton::Middle => Some(ReportButton::Middle),
        MouseButton::Right => Some(ReportButton::Right),
        _ => None,
    }
}

/// The shift/alt/ctrl bits a pointer report carries.
fn pointer_mods(mods: &Modifiers) -> PointerMods {
    PointerMods {
        shift: mods.shift,
        alt: mods.alt,
        ctrl: mods.control,
    }
}

/// Merge the `targetDeviceId` passthrough into RPC params (no-op for chats on
/// the connected engine's own device).
fn with_target(mut params: serde_json::Value, target: &Option<String>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

/// Active index after closing `closed` (given the new, shorter length).
pub fn active_after_close(active: usize, closed: usize, len_after: usize) -> usize {
    let shifted = if closed < active { active - 1 } else { active };
    if len_after == 0 {
        0
    } else {
        shifted.min(len_after - 1)
    }
}

/// The `[process exited N]` trailer, dimmed (§1.10).
pub fn exit_message(code: i32) -> Vec<u8> {
    format!("\r\n\x1b[90m[process exited {code}]\x1b[0m\r\n").into_bytes()
}

/// Tab-map / panel key: a live chat id, or `space-canvas:{space}` on the
/// new-session canvas so Terminal and Files share one per-project bucket.
pub fn canvas_session_key(selected_chat: Option<&str>, selected_space: Option<&str>) -> String {
    match selected_chat.filter(|id| !id.is_empty()) {
        Some(chat) => chat.to_string(),
        None => format!("space-canvas:{}", selected_space.unwrap_or_default()),
    }
}

/// Tab title from the session's shell path ("/bin/zsh" → "zsh").
pub fn shell_title(shell: &str) -> String {
    let name = shell.rsplit(['/', '\\']).next().unwrap_or(shell).trim();
    if name.is_empty() {
        "terminal".to_string()
    } else {
        name.to_string()
    }
}

/// The tab label, by priority: the user's rename (an explicit choice always
/// wins), then the OSC 0/2 title the running program set, then the session's
/// own identity `cwd · shell` (the fix for every tab reading "Terminal N"),
/// then the positional fallback. Empty/whitespace candidates are skipped —
/// zsh in particular sends `OSC 2 ; ST` (an empty title) right after a BEL,
/// and honouring it would blank the label.
pub fn tab_title(
    custom: Option<&str>,
    osc: Option<&str>,
    cwd: Option<&str>,
    shell: Option<&str>,
    fallback: &str,
) -> String {
    let usable = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    if let Some(title) = custom.and_then(usable) {
        return title;
    }
    if let Some(title) = osc.and_then(usable) {
        return title;
    }
    match (
        cwd.filter(|c| !c.is_empty()),
        shell.filter(|s| !s.is_empty()),
    ) {
        (Some(cwd), Some(shell)) => format!("{cwd} · {}", shell_title(shell)),
        (Some(cwd), None) => cwd.to_string(),
        (None, Some(shell)) => shell_title(shell),
        (None, None) => fallback.to_string(),
    }
}

fn decode_base64(data: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "terminal: dropping undecodable data frame");
            Vec::new()
        })
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// A grid snapshot handed to the paint element.
pub struct GridSnapshot {
    pub lines: Vec<Vec<CellSnapshot>>,
    pub cursor: Option<CursorSnapshot>,
}

/// Where the grid landed this frame, in window coordinates.
///
/// Reported by element prepaint because that is the only place the measured
/// font metrics exist. Mouse events arrive on the wrapping div in window
/// space, so mapping a pointer to a cell needs the glyph origin and the cell
/// size the *current* frame used — a stale one puts the selection a row off
/// after a resize.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeometry {
    /// Top-left of the first glyph (bounds origin plus padding).
    pub origin: gpui::Point<Pixels>,
    pub cell_w: f32,
    pub line_h: f32,
    pub cols: u16,
    pub rows: u16,
}

/// An in-flight left-button gesture.
///
/// A press alone does not select. It arms this, and only pointer travel past
/// [`SELECTION_DRAG_THRESHOLD`] promotes it to a real selection — otherwise the
/// click that focuses the panel would leave a one-cell selection behind
/// whenever the hand moves a pixel.
#[derive(Debug, Clone, Copy)]
struct SelectionDrag {
    /// Press position, in window space: both the threshold origin and the
    /// selection's anchor, so the selection starts where the press landed
    /// rather than where the threshold happened to trip.
    origin: gpui::Point<Pixels>,
    armed: bool,
}

struct TerminalTab {
    key: u64,
    title: SharedString,
    terminal_id: Option<String>,
    emulator: Emulator,
    exited: Option<i32>,
    last_seq: u64,
    coalescer: InputCoalescer,
    flush_task: Option<Task<()>>,
    resize_task: Option<Task<()>>,
    /// Open + subscribe/reconnect lifecycle; dropping it cancels the stream.
    _run: Option<Task<()>>,
    /// Session identity from `OpenTerminal` — feeds the `cwd · shell` label.
    session_cwd: Option<String>,
    session_shell: Option<String>,
    /// Rename applied by the user (F2 / context menu); wins over everything.
    custom_title: Option<SharedString>,
}

/// Settings the panel renders from (kept in sync by the shell, which owns
/// persistence). Scrollback only affects tabs opened after the change —
/// `alacritty_terminal` bakes the capacity into the grid at construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalPrefs {
    pub font_size: f32,
    pub scrollback_lines: u32,
}

impl Default for TerminalPrefs {
    fn default() -> Self {
        Self {
            font_size: TERMINAL_FONT_SIZE_DEFAULT,
            scrollback_lines: TERMINAL_SCROLLBACK_DEFAULT,
        }
    }
}

/// Panel → shell communication. Font events carry the new value so the
/// shell can persist it and mirror it onto the other panel instance (drawer
/// vs embedded); `TabClosed` lets the embedded surface strip drop its tab.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalPanelEvent {
    FontSizeChanged(f32),
    TabActivated(u64),
    TabClosed(u64),
}

/// Font size after a zoom step, clamped to the settings bounds. `0.0` delta
/// means "reset" (handled by the caller passing the default size).
pub fn zoomed_font_size(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(TERMINAL_FONT_SIZE_MIN, TERMINAL_FONT_SIZE_MAX)
}

/// Wrap-around tab cycling: next/prev from `active` in a strip of `len`.
/// `None` when there is nothing to cycle.
pub fn tab_step(active: usize, len: usize, delta: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let len = len as i32;
    Some((active as i32 + delta).rem_euclid(len) as usize)
}

// ---------------------------------------------------------------------------
// OSC 133 command navigation (pure logic)
// ---------------------------------------------------------------------------

/// Which way the command-navigation keystroke walks the blocks
/// [`komet_term::Emulator`] built from the shell's OSC 133 markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMotion {
    /// The nearest command prompt above the top of the viewport.
    Older,
    /// The nearest one below it.
    Newer,
    /// The start of the output printed by the command the viewport sits in.
    CurrentOutput,
}

/// Where a command-navigation keystroke puts the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandJump {
    /// Put this absolute content line at the top of the viewport (the panel
    /// turns it into a display offset with
    /// [`komet_term::Emulator::scroll_offset_for_line`], which clamps to the
    /// scrollback the grid still holds).
    Line(usize),
    /// Forward walk with nothing below: the walk ends on the live prompt, so go
    /// to the bottom of the buffer.
    Bottom,
    /// Nothing to do — the viewport must not move.
    Nothing,
}

/// The line the navigation lands on for one block: the `A` marker's prompt line
/// when the shell sent one (what the human reads as "where this command
/// starts"), else the first line the command printed.
pub fn command_landing_line(block: &CommandBlock) -> usize {
    block.prompt_line.unwrap_or(block.output_start_line)
}

/// Decide a command-navigation jump from the blocks the grid describes.
///
/// `current_top` is the absolute content line at the top of the viewport
/// ([`komet_term::Emulator::top_visible_line`]); every motion is *strictly*
/// relative to it, so pressing the same shortcut again always moves one
/// boundary further instead of sticking on the command you can already see.
///
/// Only marker-backed blocks count, and `saw_markers == false` answers
/// [`CommandJump::Nothing`] for every motion. That is the documented fallback
/// for a shell without the injected markers (or a PS1 that overwrote them):
/// the silence fallback does synthesize blocks, but those admit knowing
/// neither the command line nor where its prompt was, and a navigation that
/// jumps to a guessed boundary looks exactly like a real one. A shortcut that
/// visibly does nothing beats one that silently lies.
///
/// The walk is relative to the top row, so a prompt already on screen is
/// *behind* the next press rather than its target: scrolling can only push the
/// viewport back into history, and a line below row 0 cannot be raised to row 0
/// without scrolling past the live prompt. From the bottom of a short viewport
/// the newest command is therefore already the top row, and the first press
/// shows the command before it.
pub fn command_jump(
    saw_markers: bool,
    blocks: &[CommandBlock],
    current_top: usize,
    motion: CommandMotion,
) -> CommandJump {
    if !saw_markers {
        return CommandJump::Nothing;
    }
    let marked: Vec<&CommandBlock> = blocks.iter().filter(|b| b.marker_backed).collect();
    match motion {
        CommandMotion::Older => marked
            .iter()
            .rfind(|b| command_landing_line(b) < current_top)
            .map_or(CommandJump::Nothing, |b| {
                CommandJump::Line(command_landing_line(b))
            }),
        CommandMotion::Newer => marked
            .iter()
            .find(|b| command_landing_line(b) > current_top)
            .map_or(CommandJump::Bottom, |b| {
                CommandJump::Line(command_landing_line(b))
            }),
        // The command the viewport is in: the nearest block at or above the
        // current top. Its output start is what the human wants to re-read —
        // and when the viewport already sits on it, the caller's delta is 0
        // and nothing moves.
        CommandMotion::CurrentOutput => marked
            .iter()
            .rev()
            .find(|b| command_landing_line(b) <= current_top)
            .map_or(CommandJump::Nothing, |b| {
                CommandJump::Line(b.output_start_line)
            }),
    }
}

/// The scroll delta a jump implies on the grid the user is looking at, or
/// `None` when the viewport already sits where the jump points — the caller's
/// "did anything move" answer. `offset_for_target` is
/// [`komet_term::Emulator::scroll_offset_for_line`] for the line the jump
/// names, and `0` for [`CommandJump::Bottom`] (the bottom is display offset 0).
/// Keeping the arithmetic here means a jump toward a line the scrollback
/// dropped is measured against what the grid can actually show, clamped, not
/// against a scroll that would run off the end of the buffer.
pub fn jump_delta(
    jump: CommandJump,
    display_offset: usize,
    offset_for_target: usize,
) -> Option<i32> {
    let target = match jump {
        CommandJump::Nothing => return None,
        CommandJump::Bottom => 0,
        CommandJump::Line(_) => offset_for_target,
    };
    (target != display_offset).then(|| target as i32 - display_offset as i32)
}

/// One keystroke while a tab rename is in flight. Pure so the editing
/// behaviour is testable without a window: returns what the panel should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameEdit {
    /// Insert this text (printable char or typed char).
    Insert(char),
    Backspace,
    Commit,
    Cancel,
    /// Not ours — the rename field swallows everything while open.
    Ignored,
}

pub fn rename_edit_for(key: &str, key_char: Option<&str>) -> RenameEdit {
    match key {
        "enter" => RenameEdit::Commit,
        "escape" => RenameEdit::Cancel,
        "backspace" => RenameEdit::Backspace,
        _ => key_char
            .and_then(|c| c.chars().next())
            .filter(|c| !c.is_control())
            .map(RenameEdit::Insert)
            .unwrap_or(RenameEdit::Ignored),
    }
}

#[derive(Default)]
struct ChatTabs {
    tabs: Vec<TerminalTab>,
    active: usize,
}

/// Drag-reorder state; `epoch` keys the 150 ms slide animation restarts.
struct DragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-tab payload (gpui drag-and-drop).
struct TabDragPayload {
    chat: String,
    from: usize,
    title: SharedString,
}

struct TabGhost {
    title: SharedString,
}

/// Right-click menu availability: Copy needs a live selection, Paste needs
/// clipboard text, the tab rows need a tab. Snapshotted at open time — the
/// rows grey out from this snapshot instead of re-probing the emulator
/// mid-menu (EditorMenuAvailability pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalMenuAvailability {
    copy: bool,
    paste: bool,
}

impl TerminalMenuAvailability {
    fn new(has_selection: bool, clipboard_has_text: bool) -> Self {
        Self {
            copy: has_selection,
            paste: clipboard_has_text,
        }
    }
}

/// The open right-click menu's state (Copy/Paste). The position is the
/// window-space click point; `menu_at` anchors the popover there.
struct TerminalContextMenu {
    position: gpui::Point<Pixels>,
    availability: TerminalMenuAvailability,
}

impl Render for TabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(TAB_WIDTH))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(12.0))
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}

pub struct TerminalPanel {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    chats: HashMap<String, ChatTabs>,
    /// Shell-driven visibility gate: no RPC happens while closed (lazy).
    open: bool,
    /// Right-pane surface host mode: the SHELL owns the tab strip (surface
    /// tabs), so the internal bar hides, tabs are only ever created
    /// explicitly (no ensure-on-open/chat-switch), and closing the last tab
    /// must not dispatch the bottom drawer's [`ToggleTerminal`].
    embedded: bool,
    tab_seq: u64,
    drag: Option<DragState>,
    last_selected: Option<String>,
    /// Last reported grid placement; `None` until the first prepaint.
    geometry: Option<GridGeometry>,
    /// Left-button gesture in flight, if any.
    selection_drag: Option<SelectionDrag>,
    /// The button currently relayed to the PTY because an application asked
    /// for mouse reporting. While it is `Some`, moves encode drag/hover reports
    /// and the release is sent instead of starting a UI selection.
    pointer_relay: Option<ReportButton>,
    /// Open right-click Copy/Paste menu, if any.
    context_menu: crate::popover::Popup<TerminalContextMenu>,
    /// Live chrome settings (font size, scrollback for new tabs).
    prefs: TerminalPrefs,
    /// `(tab key, buffer)` while a rename is being typed; the grid swallows
    /// keystrokes into the buffer until Enter/Esc.
    renaming: Option<(u64, String)>,
    _observe: Subscription,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

impl TerminalPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        Self {
            state,
            focus_handle: cx.focus_handle(),
            chats: HashMap::new(),
            open: false,
            embedded: false,
            tab_seq: 0,
            drag: None,
            last_selected: None,
            geometry: None,
            selection_drag: None,
            pointer_relay: None,
            context_menu: crate::popover::Popup::default(),
            prefs: TerminalPrefs::default(),
            renaming: None,
            _observe: observe,
        }
    }

    /// A panel in right-pane surface-host mode (see the `embedded` field).
    pub fn new_embedded(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::new(state, cx);
        panel.embedded = true;
        panel
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Shell toggle hook. Opening lazily creates the first tab for the
    /// selected chat (drawer mode; embedded tabs are explicit); closing
    /// keeps every session alive (detach ≠ close).
    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        if open && !self.embedded {
            self.ensure_tab(cx);
        }
        cx.notify();
    }

    /// A tab's display label — see [`tab_title`]: rename wins, then the OSC
    /// 0/2 title, then `cwd · shell` from the session identity, then the
    /// positional "Terminal N".
    fn display_title(tab: &TerminalTab) -> SharedString {
        tab_title(
            tab.custom_title.as_deref(),
            tab.emulator.title(),
            tab.session_cwd.as_deref(),
            tab.session_shell.as_deref(),
            tab.title.as_ref(),
        )
        .into()
    }

    // ---- chrome settings (font zoom, scrollback) ----

    /// Push the persisted prefs down from the shell. Font size applies live
    /// (the next prepaint re-measures the grid, which resizes the PTY);
    /// scrollback applies to tabs opened from now on — changing a running
    /// alacritty grid's capacity would require rebuilding it, losing state.
    pub fn set_prefs(&mut self, prefs: TerminalPrefs, cx: &mut Context<Self>) {
        if self.prefs != prefs {
            self.prefs = prefs;
            cx.notify();
        }
    }

    /// The configured grid font size, read by the paint element each frame.
    pub fn font_size(&self) -> f32 {
        self.prefs.font_size
    }

    /// Grid line height implied by [`Self::font_size`] — the scroll wheel's
    /// pixel→line conversion needs it.
    fn line_height(&self) -> f32 {
        line_height_for(self.prefs.font_size)
    }

    /// Zoom the grid font by `delta` px (clamped) and ask the shell to
    /// persist + mirror onto the other panel instance.
    fn zoom_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        let next = if delta == 0.0 {
            TERMINAL_FONT_SIZE_DEFAULT
        } else {
            zoomed_font_size(self.prefs.font_size, delta)
        };
        if next == self.prefs.font_size {
            return;
        }
        self.prefs.font_size = next;
        cx.emit(TerminalPanelEvent::FontSizeChanged(next));
        cx.notify();
    }

    // ---- embedded (right-pane surface) API — the shell's tab strip drives
    // ---- these; keys are stable across reorders/closes.

    /// `(key, title, exited)` for the selected chat's tabs, in tab order.
    pub fn tab_summaries(&self, cx: &App) -> Vec<(u64, SharedString, bool)> {
        let chat = self.session_key(cx);
        self.chats
            .get(&chat)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .map(|t| (t.key, Self::display_title(t), t.exited.is_some()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Open a fresh tab for the selected chat (or the canvas project) and
    /// return its key.
    pub fn open_tab_for_selected(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        let chat = self.session_key(cx);
        self.open_tab(chat, cx)
    }

    /// Make `key` the rendered tab of the selected chat.
    pub fn select_tab_by_key(&mut self, key: u64, cx: &mut Context<Self>) {
        let chat = self.session_key(cx);
        let Some(ix) = self
            .chats
            .get(&chat)
            .and_then(|tabs| tabs.tabs.iter().position(|t| t.key == key))
        else {
            return;
        };
        self.select_tab(&chat, ix, cx);
    }

    /// Close the selected chat's tab `key` (surface-tab ✕).
    pub fn close_tab_by_key(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let chat = self.session_key(cx);
        self.close_tab(&chat, key, window, cx);
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let selected = Some(self.session_key(cx));
        let switched = selected != self.last_selected;
        if switched {
            self.last_selected = selected;
            self.drag = None;
        }
        if self.open && !self.embedded {
            // Returning to a chat with tabs restores them; a fresh chat (or an
            // engine that only just finished booting) gets its first tab —
            // ensure_tab is idempotent, so calling on every state change is safe.
            // Embedded: surface tabs are explicit — a chat switch just shows
            // that chat's own tabs (or the shell's surface picker).
            self.ensure_tab(cx);
        }
        if switched {
            cx.notify();
        }
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// The chat's host device when it differs from the connected engine's own —
    /// the PTY lives on the chat's device (feature-inventory §2.1 "terminals
    /// live on the chat's host device"), so every terminal RPC for a remote
    /// chat needs the `targetDeviceId` passthrough. Without it the local
    /// engine checks the chat's cwd against its OWN filesystem and fails with
    /// "Session working directory is unavailable" (user report).
    fn chat_target(&self, chat: &str, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.chats.iter().find(|c| c.id == chat)?.device_id.clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    /// Tab-map key: the selected chat, or `space-canvas:{id}` on the
    /// new-session canvas (same scheme as [`crate::shell::Shell`]'s panel
    /// key) so Terminal / Files work before a session exists.
    fn session_key(&self, cx: &App) -> String {
        let state = self.state.read(cx);
        canvas_session_key(
            state.selected_chat.as_deref(),
            state.selected_space.as_deref(),
        )
    }

    fn session_target(&self, key: &str, cx: &App) -> Option<String> {
        if let Some(target) = self.chat_target(key, cx) {
            return Some(target);
        }
        let state = self.state.read(cx);
        let space = state.selected_space_row()?;
        (state.local_device_id.as_deref() != Some(space.device_id.as_str()))
            .then(|| space.device_id.clone())
    }

    fn open_identity(&self, key: &str, cx: &App) -> (Option<String>, Option<String>) {
        let state = self.state.read(cx);
        if state.chats.iter().any(|chat| chat.id == key) {
            (Some(key.to_string()), None)
        } else {
            (
                None,
                state.selected_space_row().map(|space| space.id.clone()),
            )
        }
    }

    fn ensure_tab(&mut self, cx: &mut Context<Self>) {
        let chat = self.session_key(cx);
        if self.chats.get(&chat).is_none_or(|c| c.tabs.is_empty()) {
            self.open_tab(chat, cx);
        }
    }

    fn tab_mut(&mut self, chat: &str, key: u64) -> Option<&mut TerminalTab> {
        self.chats
            .get_mut(chat)?
            .tabs
            .iter_mut()
            .find(|t| t.key == key)
    }

    fn active_tab(&self, cx: &App) -> Option<&TerminalTab> {
        let chat = self.session_key(cx);
        let tabs = self.chats.get(&chat)?;
        tabs.tabs.get(tabs.active)
    }

    // ---- open / stream lifecycle ----

    fn open_tab(&mut self, chat: String, cx: &mut Context<Self>) -> Option<u64> {
        let engine = self.engine(cx)?;
        self.tab_seq += 1;
        let key = self.tab_seq;
        let entry = self.chats.entry(chat.clone()).or_default();
        let tab_no = entry.tabs.len() + 1;
        entry.tabs.push(TerminalTab {
            key,
            title: format!("Terminal {tab_no}").into(),
            terminal_id: None,
            emulator: Emulator::with_scrollback(80, 24, self.prefs.scrollback_lines as usize),
            exited: None,
            last_seq: 0,
            coalescer: InputCoalescer::default(),
            flush_task: None,
            resize_task: None,
            _run: None,
            session_cwd: None,
            session_shell: None,
            custom_title: None,
        });
        entry.active = entry.tabs.len() - 1;

        let target = self.session_target(&chat, cx);
        let (chat_id, space_id) = self.open_identity(&chat, cx);
        let run = Self::spawn_session(chat.clone(), chat_id, space_id, key, engine, target, cx);
        if let Some(tab) = self.tab_mut(&chat, key) {
            tab._run = Some(run);
        }
        cx.notify();
        Some(key)
    }

    /// OpenTerminal, then pump SubscribeTerminal with reconnect backoff.
    fn spawn_session(
        chat: String,
        chat_id: Option<String>,
        space_id: Option<String>,
        key: u64,
        engine: EngineHandle,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let (cols, rows) = this
                .update(cx, |panel, _| {
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.emulator.cols() as u16, t.emulator.rows() as u16))
                        .unwrap_or((80, 24))
                })
                .unwrap_or((80, 24));

            let mut params = serde_json::json!({ "cols": cols, "rows": rows });
            if let Some(chat_id) = chat_id {
                params["chatId"] = serde_json::Value::String(chat_id);
            }
            if let Some(space_id) = space_id {
                params["spaceId"] = serde_json::Value::String(space_id);
            }
            let opened = engine
                .client()
                .call_as::<TerminalSession>(
                    methods::OPEN_TERMINAL,
                    with_target(params, &target),
                )
                .await;
            let session = match opened {
                Ok(session) => session,
                Err(err) => {
                    tracing::warn!(error = %err, "OpenTerminal failed");
                    let _ = this.update(cx, |panel, cx| {
                        if let Some(tab) = panel.tab_mut(&chat, key) {
                            tab.emulator.feed(
                                format!("\x1b[31mfailed to open terminal: {err}\x1b[0m\r\n")
                                    .as_bytes(),
                            );
                            tab.exited = Some(-1);
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let terminal_id = session.id.clone();
            let attached = this
                .update(cx, |panel, cx| {
                    if let Some(tab) = panel.tab_mut(&chat, key) {
                        tab.terminal_id = Some(terminal_id.clone());
                        // Session identity for the tab label (`cwd · shell`).
                        tab.session_cwd = Some(session.cwd.clone());
                        tab.session_shell = Some(session.shell.clone());
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !attached {
                // Tab was closed before the open completed — release the PTY.
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id }),
                            &target,
                        ),
                    )
                    .await;
                return;
            }

            let mut attempt: u32 = 0;
            loop {
                let Ok(after_seq) = this.update(cx, |panel, _| {
                    panel.tab_mut(&chat, key).map(|t| t.last_seq)
                }) else {
                    return; // entity released
                };
                let Some(after_seq) = after_seq else { return }; // tab closed

                let subscribed = engine
                    .client()
                    .subscribe(
                        methods::SUBSCRIBE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id, "afterSeq": after_seq }),
                            &target,
                        ),
                    )
                    .await;
                let mut rx = match subscribed {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(error = %err, attempt, "SubscribeTerminal failed; backing off");
                        cx.background_executor()
                            .timer(Duration::from_millis(backoff_ms(attempt)))
                            .await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                };

                while let Some(value) = rx.recv().await {
                    let event: TerminalEvent = match serde_json::from_value(value) {
                        Ok(event) => event,
                        Err(err) => {
                            tracing::warn!(error = %err, "terminal: malformed stream frame");
                            continue;
                        }
                    };
                    attempt = 0;
                    let outcome = this.update(cx, |panel, cx| {
                        panel.apply_stream_event(&chat, key, &engine, event, cx)
                    });
                    match outcome {
                        Ok(StreamDisposition::Continue) => {}
                        Ok(StreamDisposition::Stop) => return,
                        Err(_) => return,
                    }
                }

                // Stream dropped without an exit — reconnect from afterSeq.
                let done = this
                    .update(cx, |panel, _| {
                        panel.tab_mut(&chat, key).map(|t| t.exited.is_some()).unwrap_or(true)
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(backoff_ms(attempt)))
                    .await;
                attempt = attempt.saturating_add(1);
            }
        })
    }

    fn apply_stream_event(
        &mut self,
        chat: &str,
        key: u64,
        engine: &EngineHandle,
        event: TerminalEvent,
        cx: &mut Context<Self>,
    ) -> StreamDisposition {
        let target = self.chat_target(chat, cx);
        let Some(tab) = self.tab_mut(chat, key) else {
            return StreamDisposition::Stop;
        };
        match event {
            TerminalEvent::Data { seq, data } => {
                tab.last_seq = seq;
                let responses = tab.emulator.feed(&decode_base64(&data));
                if !responses.is_empty()
                    && let Some(id) = tab.terminal_id.clone()
                {
                    // Query responses (DSR etc.) go straight back, no coalescing.
                    let engine = engine.clone();
                    let data = encode_base64(&responses);
                    cx.spawn(async move |_, _| {
                        let _ = engine
                            .client()
                            .call(
                                methods::WRITE_TERMINAL,
                                with_target(
                                    serde_json::json!({ "terminalId": id, "data": data }),
                                    &target,
                                ),
                            )
                            .await;
                    })
                    .detach();
                }
                cx.notify();
                StreamDisposition::Continue
            }
            TerminalEvent::Exit { seq, exit_code, .. } => {
                tab.last_seq = seq;
                tab.exited = Some(exit_code);
                tab.emulator.feed(&exit_message(exit_code));
                cx.notify();
                StreamDisposition::Stop
            }
        }
    }

    // ---- input ----

    /// Queue keyboard bytes on the active tab (12 ms coalescing window).
    fn queue_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let chat = self.session_key(cx);
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.exited.is_some() {
            return;
        }
        // A keypress while scrolled back snaps to the live bottom (xterm).
        if tab.emulator.display_offset() > 0 {
            tab.emulator.scroll_to_bottom();
        }
        let key = tab.key;
        if tab.coalescer.push(bytes) {
            tab.flush_task = Some(Self::schedule_flush(chat, key, cx));
        }
    }

    fn schedule_flush(chat: String, key: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(COALESCE_MS))
                .await;
            let _ = this.update(cx, |panel, cx| panel.flush_input(chat, key, cx));
        })
    }

    fn flush_input(&mut self, chat: String, key: u64, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.session_target(&chat, cx);
        let Some(tab) = self.tab_mut(&chat, key) else {
            return;
        };
        if tab.coalescer.is_empty() {
            return;
        }
        let Some(id) = tab.terminal_id.clone() else {
            // OpenTerminal still in flight — keep the buffer, retry shortly.
            if tab.exited.is_none() {
                tab.flush_task = Some(Self::schedule_flush(chat, key, cx));
            }
            return;
        };
        let data = encode_base64(&tab.coalescer.take());
        cx.spawn(async move |_, _| {
            let _ = engine
                .client()
                .call(
                    methods::WRITE_TERMINAL,
                    with_target(
                        serde_json::json!({ "terminalId": id, "data": data }),
                        &target,
                    ),
                )
                .await;
        })
        .detach();
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self
            .active_tab(cx)
            .map(|tab| tab.emulator.bracketed_paste_mode())
            .unwrap_or(false);
        let bytes = paste_bytes(&text, bracketed);
        self.queue_input(&bytes, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        // A rename in flight owns the keyboard: chars edit the buffer,
        // Enter commits, Escape cancels, everything else is swallowed so it
        // cannot leak to the PTY mid-rename.
        if self.renaming.is_some() {
            self.apply_rename_edit(&ks.key, ks.key_char.as_deref(), cx);
            cx.stop_propagation();
            return;
        }
        // Paste: Cmd+V (macOS) / Ctrl+Shift+V.
        if ks.key == "v" && (mods.platform || (mods.control && mods.shift)) {
            self.paste_clipboard(cx);
            cx.stop_propagation();
            return;
        }
        // Copy: Cmd+C (macOS) / Ctrl+Shift+C. Only swallowed when it actually
        // copied — so Ctrl+Shift+C with nothing selected still falls through
        // to the interrupt, and plain Ctrl+C (no shift) never reaches here.
        if ks.key == "c"
            && (mods.platform || (mods.control && mods.shift))
            && self.copy_selection(cx)
        {
            cx.stop_propagation();
            return;
        }
        let app_cursor = self
            .active_tab(cx)
            .map(|tab| tab.emulator.app_cursor_mode())
            .unwrap_or(false);
        if let Some(bytes) = keystroke_bytes(&ks.key, ks.key_char.as_deref(), mods, app_cursor) {
            self.queue_input(&bytes, cx);
            cx.stop_propagation();
        }
    }

    // ---- grid metrics / element hooks ----

    /// Called from element prepaint with the frame's grid placement. Resizes
    /// the emulator immediately; the `ResizeTerminal` RPC debounces 80 ms.
    pub fn on_grid_metrics(&mut self, geometry: GridGeometry, cx: &mut Context<Self>) {
        // Stash unconditionally, before the early returns below: pointer
        // mapping needs the placement even on frames where nothing resized,
        // which is almost all of them.
        self.geometry = Some(geometry);
        let (cols, rows) = (geometry.cols, geometry.rows);
        let chat = self.session_key(cx);
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.emulator.cols() == cols as usize && tab.emulator.rows() == rows as usize {
            return;
        }
        tab.emulator.resize(cols, rows);
        let key = tab.key;
        let engine = self.engine(cx);
        let target = self.session_target(&chat, cx);
        if let (Some(engine), Some(tab)) = (engine, self.tab_mut(&chat, key)) {
            let id = tab.terminal_id.clone();
            tab.resize_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_DEBOUNCE_MS))
                    .await;
                // Re-read the *current* size — later prepaints may have
                // resized again inside the debounce window.
                let Ok(current) = this.update(cx, |panel, _| {
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.terminal_id.clone(), t.emulator.cols(), t.emulator.rows()))
                }) else {
                    return;
                };
                let Some((stored_id, cols, rows)) = current else {
                    return;
                };
                let Some(id) = stored_id.or(id) else { return };
                let _ = engine
                    .client()
                    .call(
                        methods::RESIZE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": id, "cols": cols, "rows": rows }),
                            &target,
                        ),
                    )
                    .await;
            }));
        }
        // Deliberately no cx.notify(): this runs during prepaint of the
        // current frame, which already paints the resized grid.
    }

    /// Snapshot for the paint element.
    pub fn active_grid_snapshot(&self, cx: &App) -> Option<GridSnapshot> {
        let tab = self.active_tab(cx)?;
        Some(GridSnapshot {
            lines: tab.emulator.lines(),
            cursor: tab.emulator.cursor(),
        })
    }

    // ---- selection ----

    /// Run `f` against the active tab's emulator.
    fn with_active_emulator<R>(
        &mut self,
        cx: &App,
        f: impl FnOnce(&mut Emulator) -> R,
    ) -> Option<R> {
        let chat = self.session_key(cx);
        let tabs = self.chats.get_mut(&chat)?;
        let active = tabs.active;
        tabs.tabs.get_mut(active).map(|tab| f(&mut tab.emulator))
    }

    /// Window position → grid point, using this frame's placement. `None`
    /// before the first prepaint, or when no tab is active.
    fn grid_point_at(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &App,
    ) -> Option<(GridPoint, Side)> {
        let geometry = self.geometry?;
        let hit = cell_at(
            f32::from(position.x - geometry.origin.x),
            f32::from(position.y - geometry.origin.y),
            geometry.cell_w,
            geometry.line_h,
            geometry.cols as usize,
            geometry.rows as usize,
        );
        let point = self.with_active_emulator(cx, |emu| emu.grid_point(hit.row, hit.col))?;
        Some((point, hit.side))
    }

    // ---- pointer routing (mouse reporting) ----

    /// The active tab's mouse modes, or `None` with no tab.
    fn active_mouse_modes(&self, cx: &App) -> Option<MouseModes> {
        self.active_tab(cx).map(|tab| tab.emulator.mouse_modes())
    }

    /// The **viewport cell** (col, row) a window-space position lands on —
    /// the terminal grid coordinate a mouse report is about. Deliberately not
    /// `grid_point_at`: a report addresses the on-screen row, not the
    /// scrollback line a selection anchor uses. `None` before the first
    /// prepaint.
    fn cell_hit_at(&self, position: gpui::Point<Pixels>) -> Option<(usize, usize)> {
        let geometry = self.geometry?;
        let hit = cell_at(
            f32::from(position.x - geometry.origin.x),
            f32::from(position.y - geometry.origin.y),
            geometry.cell_w,
            geometry.line_h,
            geometry.cols as usize,
            geometry.rows as usize,
        );
        Some((hit.col, hit.row))
    }

    /// Try to relay a press to the PTY. Returns `true` when a mouse protocol
    /// captured it (send or drop) — the caller then skips its UI behaviour.
    /// `false` means no protocol or Shift held: keep selection / paste / menu.
    fn relay_press(
        &mut self,
        button: MouseButton,
        position: gpui::Point<Pixels>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(rb) = report_button_of(button) else {
            return false;
        };
        // Fast path: no protocol means the pointer is the UI's, untouched.
        if self
            .active_mouse_modes(cx)
            .is_none_or(|m| !m.protocol.is_active())
        {
            return false;
        }
        let modes = self.active_mouse_modes(cx).expect("checked above");
        let Some((col, row)) = self.cell_hit_at(position) else {
            return false;
        };
        let route = route_pointer(
            &modes,
            &PointerEvent::Press { button: rb },
            col,
            row,
            &pointer_mods(mods),
        );
        match route {
            MouseRoute::Send(bytes) => {
                self.pointer_relay = Some(rb);
                self.queue_input(&bytes, cx);
                true
            }
            MouseRoute::Ignore => {
                self.pointer_relay = Some(rb);
                true
            }
            MouseRoute::Local => false,
        }
    }

    /// Relay a drag/hover motion while a gesture is in flight.
    fn relay_motion(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(modes) = self.active_mouse_modes(cx) else {
            return;
        };
        let Some((col, row)) = self.cell_hit_at(event.position) else {
            return;
        };
        let pointer = match event.pressed_button.and_then(report_button_of) {
            Some(b) => PointerEvent::Drag { button: b },
            None => PointerEvent::Motion,
        };
        if let MouseRoute::Send(bytes) =
            route_pointer(&modes, &pointer, col, row, &pointer_mods(&event.modifiers))
        {
            self.queue_input(&bytes, cx);
        }
    }

    /// Relay a buttonless hover; only DECSET 1003 (any-event) asks for it, so
    /// every other state stays a no-op (the pre-existing behaviour).
    fn relay_hover(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self
            .active_mouse_modes(cx)
            .is_none_or(|m| m.protocol != MouseProtocol::Any)
        {
            return;
        }
        self.relay_motion(event, cx);
    }

    /// Relay the release that ends an in-flight gesture, keyed off the button
    /// the press captured.
    fn relay_release(
        &mut self,
        position: gpui::Point<Pixels>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(rb) = self.pointer_relay else {
            return;
        };
        let Some(modes) = self.active_mouse_modes(cx) else {
            return;
        };
        let Some((col, row)) = self.cell_hit_at(position) else {
            return;
        };
        if let MouseRoute::Send(bytes) = route_pointer(
            &modes,
            &PointerEvent::Release { button: rb },
            col,
            row,
            &pointer_mods(mods),
        ) {
            self.queue_input(&bytes, cx);
        }
    }

    /// Middle-click on the text grid: with a mouse protocol active the app
    /// owns the button, so this relays a report. The tab strip's own
    /// middle-click (close tab) is a different element and is untouched.
    fn on_middle_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.relay_press(MouseButton::Middle, event.position, &event.modifiers, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        // A mouse-reporting app owns the pointer: relay the press to the PTY
        // and remember the held button so the drag/release follow. `false`
        // means no protocol (or Shift held) — fall through to selection.
        if self.relay_press(event.button, event.position, &event.modifiers, cx) {
            return;
        }
        let Some((point, side)) = self.grid_point_at(event.position, cx) else {
            return;
        };
        // Click count picks the granularity, the same mapping every terminal
        // uses: drag, word, line.
        let ty = match event.click_count {
            0 => return,
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        let shift = event.modifiers.shift;
        if ty == SelectionType::Simple {
            // Shift+click extends an existing selection instead of replacing
            // it — the one gesture that reaches text off the bottom of a long
            // drag without redoing the whole thing.
            let extended = shift
                && self
                    .with_active_emulator(cx, |emu| {
                        let extend = emu.has_selection();
                        if extend {
                            emu.update_selection(point, side);
                        }
                        extend
                    })
                    .unwrap_or(false);
            if extended {
                self.selection_drag = Some(SelectionDrag {
                    origin: event.position,
                    armed: true,
                });
                cx.notify();
                return;
            }
            // A plain press clears and arms; the selection itself only begins
            // once the pointer travels far enough to mean it.
            self.with_active_emulator(cx, |emu| emu.clear_selection());
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                armed: false,
            });
        } else {
            // Word and line selections are complete on the press, so they need
            // no threshold — but keep the drag live so the pointer can extend
            // them at that granularity.
            self.with_active_emulator(cx, |emu| emu.start_selection(ty, point, side));
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                armed: true,
            });
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A gesture relayed to the PTY keeps relaying: motion is a drag report
        // (button held) or, under any-event tracking, a hover report. It never
        // touches the UI selection.
        if self.pointer_relay.is_some() {
            self.relay_motion(event, cx);
            return;
        }
        if !event.dragging() {
            // No in-flight relay and no button down. Only DECSET 1003
            // (any-event) asks for this hover; everything else stays exactly as
            // before mouse relay existed (a no-op).
            self.relay_hover(event, cx);
            return;
        }
        let Some(drag) = self.selection_drag else {
            return;
        };
        if !drag.armed {
            let dx = f32::from(event.position.x - drag.origin.x);
            let dy = f32::from(event.position.y - drag.origin.y);
            if dx.hypot(dy) < SELECTION_DRAG_THRESHOLD {
                return;
            }
            // Threshold tripped: anchor at the *press*, not here, so the
            // selection covers the whole gesture.
            let Some((anchor, side)) = self.grid_point_at(drag.origin, cx) else {
                return;
            };
            self.with_active_emulator(cx, |emu| {
                emu.start_selection(SelectionType::Simple, anchor, side)
            });
            self.selection_drag = Some(SelectionDrag {
                armed: true,
                ..drag
            });
        }
        let Some((point, side)) = self.grid_point_at(event.position, cx) else {
            return;
        };
        self.with_active_emulator(cx, |emu| emu.update_selection(point, side));
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // A relayed gesture ends with a release report to the PTY, keyed off the
        // button the press captured (not the release event), then clears state.
        if self.pointer_relay.is_some() {
            self.relay_release(event.position, &event.modifiers, cx);
            self.pointer_relay = None;
            return;
        }
        self.selection_drag = None;
    }

    /// Right-click opens the Copy/Paste context menu at the click point.
    /// Focus rides along so typing keeps routing to the terminal once the
    /// menu closes; the selection is untouched — Copy needs it alive until
    /// the row actually runs (a right-click must never clear a selection
    /// the user is about to copy).
    fn on_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        // Right-button mouse reporting (DECSET 1000+): the app asked for the
        // pointer, so a right-press is a report, not the Copy/Paste menu. The
        // menu survives only when no protocol captured it (or Shift is held).
        if self.relay_press(MouseButton::Right, event.position, &event.modifiers, cx) {
            return;
        }
        let has_selection = self
            .with_active_emulator(cx, |emu| emu.has_selection())
            .unwrap_or(false);
        let clipboard_has_text = cx
            .read_from_clipboard()
            .is_some_and(|item| item.text().is_some());
        self.context_menu.open(TerminalContextMenu {
            position: event.position,
            availability: TerminalMenuAvailability::new(has_selection, clipboard_has_text),
        });
        cx.notify();
    }

    fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.begin_close() {
            crate::popover::reap_popup(cx, |panel: &mut Self| &mut panel.context_menu);
            cx.notify();
        }
    }

    /// Copy from the menu: the availability gate already proved a
    /// selection exists, the bool return is irrelevant here.
    fn copy_from_menu(&mut self, cx: &mut Context<Self>) {
        let _ = self.copy_selection(cx);
    }

    /// Paste from the menu (bracketed-paste aware, same as Ctrl+Shift+V).
    fn paste_from_menu(&mut self, cx: &mut Context<Self>) {
        self.paste_clipboard(cx);
    }

    fn context_menu_row(
        theme: &Theme,
        id: &'static str,
        label: SharedString,
        enabled: bool,
        run: fn(&mut Self, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        crate::popover::menu_row(theme, false, id, cx.entity_id())
            .id(id)
            .when(!enabled, |row| row.opacity(0.38).cursor_default())
            .when(enabled, |row| {
                row.on_click(cx.listener(move |this, _, window, cx| {
                    this.close_context_menu(cx);
                    run(this, window, cx);
                }))
            })
            .child(label)
            .into_any_element()
    }

    fn render_context_menu(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let menu = self.context_menu.get()?;
        let position = menu.position;
        let availability = menu.availability;
        let closing = self.context_menu.closing_since();

        // `cx` is a parameter, not a capture: the closure must not hold the
        // mutable context while `.child(row(...))` chains re-borrow it.
        let row = |id: &'static str,
                   label: SharedString,
                   enabled: bool,
                   run: fn(&mut Self, &mut Window, &mut Context<Self>),
                   cx: &mut Context<Self>| {
            Self::context_menu_row(theme, id, label, enabled, run, cx)
        };

        let mut card = crate::popover::popover_card(theme)
            .w(px(170.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_context_menu(cx)))
            .flex()
            .flex_col();
        card = card
            .child(row(
                "terminal-context-copy",
                "Copy".into(),
                availability.copy,
                |this, _, cx| {
                    this.copy_from_menu(cx);
                },
                cx,
            ))
            .child(row(
                "terminal-context-paste",
                "Paste".into(),
                availability.paste,
                |this, _, cx| {
                    this.paste_from_menu(cx);
                },
                cx,
            ));

        Some(crate::popover::menu_at(
            "terminal-context-menu",
            position,
            card.into_any_element(),
            closing,
        ))
    }

    /// Copy the selection. Returns whether anything was copied, so the caller
    /// can decide whether to swallow the keystroke.
    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self
            .with_active_emulator(cx, |emu| emu.selection_text())
            .flatten()
        else {
            return false;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        true
    }

    /// Route a wheel gesture to the PTY when an app owns the pointer: mouse-
    /// reporting wheel buttons (protocol active) or AltScroll arrows (alt
    /// screen, no protocol). One report per notch. Returns `true` when handled;
    /// `false` lets the wheel drive the UI scrollback as before.
    fn relay_wheel(&mut self, event: &ScrollWheelEvent, step: i32, cx: &mut Context<Self>) -> bool {
        let Some(modes) = self.active_mouse_modes(cx) else {
            return false;
        };
        let Some((col, row)) = self.cell_hit_at(event.position) else {
            return false;
        };
        let mods = pointer_mods(&event.modifiers);
        let up = step > 0;
        let mut bytes = Vec::new();
        for _ in 0..step.abs() {
            match route_pointer(&modes, &PointerEvent::Wheel { up }, col, row, &mods) {
                MouseRoute::Send(mut chunk) => bytes.append(&mut chunk),
                // A notch that isn't the app's means the whole event is the
                // UI's — scroll the local scrollback instead of a partial send.
                _ => return false,
            }
        }
        if bytes.is_empty() {
            return false;
        }
        self.queue_input(&bytes, cx);
        true
    }

    fn scroll_active(&mut self, delta_lines: i32, cx: &mut Context<Self>) {
        if delta_lines == 0 {
            return;
        }
        let chat = self.session_key(cx);
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        if let Some(tab) = tabs.tabs.get_mut(active) {
            tab.emulator.scroll(delta_lines);
            cx.notify();
        }
    }

    /// Walk the OSC 133 command blocks of the active tab. Returns whether the
    /// viewport moved, so a caller can tell a jump from an inert keystroke.
    ///
    /// The blocks come from the tab's own emulator, so the walk covers the
    /// scrollback exactly as far as the grid still holds it:
    /// [`komet_term::Emulator::scroll_offset_for_line`] clamps a jump toward a
    /// line the scrollback dropped to the oldest line that survives, which is
    /// the nearest real position rather than a failed scroll.
    fn jump_command(&mut self, motion: CommandMotion, cx: &mut Context<Self>) -> bool {
        let chat = self.session_key(cx);
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return false;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return false;
        };
        let jump = command_jump(
            tab.emulator.saw_markers(),
            &tab.emulator.command_blocks(),
            tab.emulator.top_visible_line(),
            motion,
        );
        let line_offset = match jump {
            CommandJump::Line(line) => tab.emulator.scroll_offset_for_line(line),
            _ => 0,
        };
        // `jump_delta` only answers `Some` for a delta that moves the viewport,
        // so a real jump always repaints and an inert keystroke never does.
        let Some(delta) = jump_delta(jump, tab.emulator.display_offset(), line_offset) else {
            if !tab.emulator.saw_markers() {
                // The one case where a press has no explanation on screen: log
                // which shell it is, so "the shortcut does nothing" is
                // diagnosable rather than mysterious.
                tracing::debug!(
                    chat = %chat,
                    shell = tab.session_shell.as_deref().unwrap_or("?"),
                    "terminal: OSC 133 navigation inert (no markers from this shell)"
                );
            }
            return false;
        };
        tab.emulator.scroll(delta);
        cx.notify();
        true
    }

    // ---- tab management ----

    fn select_tab(&mut self, chat: &str, ix: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat)
            && ix < tabs.tabs.len()
        {
            tabs.active = ix;
            cx.notify();
        }
    }

    fn close_tab(&mut self, chat: &str, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine(cx);
        let target = self.chat_target(chat, cx);
        let Some(tabs) = self.chats.get_mut(chat) else {
            return;
        };
        let Some(ix) = tabs.tabs.iter().position(|t| t.key == key) else {
            return;
        };
        let tab = tabs.tabs.remove(ix);
        tabs.active = active_after_close(tabs.active, ix, tabs.tabs.len());
        let now_empty = tabs.tabs.is_empty();
        self.drag = None;
        // Tell the shell to drop any surface tab bound to this key (embedded
        // strip). The handler only touches shell state — safe to fire while
        // this entity is being updated.
        cx.emit(TerminalPanelEvent::TabClosed(tab.key));
        // Closing the LAST terminal closes the drawer too — an empty dock is
        // dead space (user request). Same path as the collapse chevron.
        // Embedded, the SHELL owns emptiness (it falls back to the surface
        // picker) — dispatching here would toggle the bottom drawer instead.
        if now_empty && self.open && !self.embedded {
            window.dispatch_action(Box::new(ToggleTerminal), cx);
        }
        if let (Some(engine), Some(id)) = (engine, tab.terminal_id.clone()) {
            cx.spawn(async move |_, _| {
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(serde_json::json!({ "terminalId": id }), &target),
                    )
                    .await;
            })
            .detach();
        }
        cx.notify();
    }

    fn commit_reorder(&mut self, chat: &str, from: usize, to: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat) {
            let active = tabs.active;
            reorder_tabs(&mut tabs.tabs, from, to);
            tabs.active = active_after_reorder(active, from, to);
        }
        self.drag = None;
        cx.notify();
    }

    // ---- chrome actions (keyboard shortcuts + menu rows) ----

    /// mod+shift+T / context menu. Embedded, the panel must NOT open a raw
    /// internal tab: the shell owns the surface strip, so the action keeps
    /// propagating and `Shell` routes it through `add_terminal_surface`
    /// (open tab + push the surface + activate it). In the drawer, the panel
    /// owns its bar and handles it directly.
    fn action_new_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.embedded {
            return; // let it bubble to the shell
        }
        let chat = self.session_key(cx);
        self.open_tab(chat, cx);
        cx.stop_propagation();
    }

    fn action_close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chat = self.session_key(cx);
        let Some(key) = self.active_tab_key(cx) else {
            return;
        };
        self.close_tab(&chat, key, window, cx);
        cx.stop_propagation();
    }

    fn action_cycle_tab(&mut self, delta: i32, cx: &mut Context<Self>) {
        let chat = self.session_key(cx);
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let Some(next) = tab_step(tabs.active, tabs.tabs.len(), delta) else {
            return;
        };
        if next != tabs.active {
            tabs.active = next;
            let key = tabs.tabs[next].key;
            // The embedded surface strip highlights from shell state; tell it
            // which tab the grid now shows. (No panel update from the shell
            // here: this entity is mid-update.)
            cx.emit(TerminalPanelEvent::TabActivated(key));
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn action_rename_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, current)) = self.active_rename_seed(cx) else {
            return;
        };
        self.renaming = Some((key, current));
        cx.stop_propagation();
        cx.notify();
    }

    /// Seed for the rename buffer: the tab's key + its current label
    /// (preferring the existing rename, else the displayed title).
    fn active_rename_seed(&self, cx: &App) -> Option<(u64, String)> {
        let tab = self.active_tab(cx)?;
        let label = match tab.custom_title.as_deref() {
            Some(t) => t.to_string(),
            None => Self::display_title(tab).to_string(),
        };
        Some((tab.key, label))
    }

    fn apply_rename_edit(&mut self, key: &str, key_char: Option<&str>, cx: &mut Context<Self>) {
        use RenameEdit::{Backspace, Cancel, Commit, Ignored, Insert};
        let edit = rename_edit_for(key, key_char);
        if edit == Ignored {
            return;
        }
        if matches!(edit, Commit | Cancel) {
            let finished = std::mem::take(&mut self.renaming);
            if let Some((tab_key, buffer)) = finished
                && edit == Commit
            {
                self.set_tab_title(tab_key, buffer, cx);
            }
            cx.notify();
            return;
        }
        if let Some((_, buffer)) = self.renaming.as_mut() {
            match edit {
                Insert(ch) => buffer.push(ch),
                Backspace => {
                    buffer.pop();
                }
                _ => {}
            }
            cx.notify();
        }
    }

    fn set_tab_title(&mut self, tab_key: u64, title: String, cx: &mut Context<Self>) {
        let title = title.trim().to_string();
        let Some(tab) = self
            .chats
            .values_mut()
            .flat_map(|tabs| tabs.tabs.iter_mut())
            .find(|t| t.key == tab_key)
        else {
            return;
        };
        tab.custom_title = if title.is_empty() {
            None
        } else {
            Some(title.into())
        };
        cx.notify();
    }

    fn active_tab_key(&self, cx: &App) -> Option<u64> {
        self.active_tab(cx).map(|tab| tab.key)
    }

    fn action_zoom_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.zoom_font(delta, cx);
        cx.stop_propagation();
    }

    /// Command-navigation keystroke. The keystroke is swallowed whether or not
    /// the jump moved the viewport: on Linux these combos are Ctrl+Shift+{Up,
    /// Down,O}, and an inert jump (a shell that never sent a marker) must not
    /// fall through to `keystroke_bytes` and type into the user's prompt.
    fn action_command_jump(&mut self, motion: CommandMotion, cx: &mut Context<Self>) {
        self.jump_command(motion, cx);
        cx.stop_propagation();
    }

    fn update_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.drag = Some(DragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    // ---- render ----

    /// The in-flight rename editor: a small floating strip over the grid —
    /// the buffer is typed via the keyboard capture in [`Self::on_key_down`]
    /// (Enter commits, Escape cancels). Rendered only while renaming.
    fn render_rename_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (_, buffer) = self.renaming.as_ref()?;
        let theme = Theme::of(cx);
        let displayed = if buffer.is_empty() {
            " "
        } else {
            buffer.as_str()
        };
        Some(
            div()
                .id("terminal-rename-overlay")
                .absolute()
                .top(px(10.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .bg(theme.surface_raised)
                        .border_1()
                        .border_color(theme.border_strong)
                        .text_size(px(12.0))
                        .text_color(theme.text)
                        .child(format!(
                            "Rename tab · {displayed} · Enter to save, Esc to cancel"
                        )),
                )
                .into_any_element(),
        )
    }

    fn render_tab_bar(&mut self, chat: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = Theme::of(cx).clone();
        let tabs = self.chats.get(chat);
        let (active, count) = tabs.map(|t| (t.active, t.tabs.len())).unwrap_or((0, 0));
        let drag = self
            .drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));
        let chat_owned = chat.to_string();

        let tab_elements: Vec<_> = tabs
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .enumerate()
                    .map(|(ix, tab)| {
                        let selected = ix == active;
                        let key = tab.key;
                        // Contextual label (user request): rename > OSC title >
                        // `cwd · shell` > "Terminal N".
                        let title = Self::display_title(tab);
                        let exited = tab.exited.is_some();
                        (ix, key, title, selected, exited)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let bar_chat = chat_owned.clone();
        let drop_chat = chat_owned.clone();
        // Komet terminal-panel.tsx: `flex h-10 items-center border-b
        // border-white/[0.07] pl-2 pr-1.5` on the #090909 panel — no separate
        // bar fill.
        div()
            .id("terminal-tab-bar")
            .h(px(TAB_BAR_HEIGHT))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.07))
            .on_drag_move::<TabDragPayload>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<TabDragPayload>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.chat != bar_chat {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x) - f32::from(event.bounds.left());
                    let over = drop_index(rel_x, TAB_WIDTH, count);
                    this.update_drag_over(from, over, cx);
                },
            ))
            .on_drop::<TabDragPayload>(cx.listener(move |this, payload: &TabDragPayload, _, cx| {
                if payload.chat != drop_chat {
                    this.drag = None;
                    cx.notify();
                    return;
                }
                let to = this.drag.as_ref().map(|d| d.over).unwrap_or(payload.from);
                let chat = drop_chat.clone();
                this.commit_reorder(&chat, payload.from, to, cx);
            }))
            .children(
                tab_elements
                    .into_iter()
                    .map(|(ix, key, title, selected, exited)| {
                        let chat_select = chat_owned.clone();
                        let chat_close = chat_owned.clone();
                        let chat_close2 = chat_owned.clone();
                        let chat_drag = chat_owned.clone();
                        let ghost_title = title.clone();
                        // Komet tab: `h-7 rounded-lg pl-2 pr-1 gap-1.5 text-xs`,
                        // terminal glyph + label + close; active = white/8 wash.
                        let (text_color, bg, glyph_alpha) = if selected {
                            (theme.text, crate::theme::ink(0.08), 0.8)
                        } else {
                            (
                                theme.text_muted.opacity(0.6),
                                gpui::transparent_black(),
                                0.6,
                            )
                        };
                        let close_btn = div()
                            .id(("terminal-tab-close", key))
                            .size(px(20.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .when(!selected, |el| el.invisible())
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::theme::ink(0.09)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(&chat_close2, key, window, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted.opacity(0.8)),
                            );
                        let tab_el = div()
                            .id(("terminal-tab", key))
                            .w(px(TAB_WIDTH))
                            .h(px(28.0))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .pl(px(8.0))
                            .pr(px(4.0))
                            .rounded(px(8.0))
                            // komet terminal-panel.tsx tab: `transition-colors`.
                            .bg(motion::hover_blend(
                                &format!("term-tab-{key}"),
                                bg,
                                theme.element_hover,
                            ))
                            .on_hover(motion::hover_listener(
                                format!("term-tab-{key}"),
                                cx.entity_id(),
                            ))
                            .text_size(px(12.0))
                            .text_color(text_color)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_tab(&chat_select, ix, cx);
                            }))
                            // Middle-click closes (§1.10).
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(move |this, _, window, cx| {
                                    this.close_tab(&chat_close, key, window, cx);
                                }),
                            )
                            .on_drag(
                                TabDragPayload {
                                    chat: chat_drag,
                                    from: ix,
                                    title: ghost_title,
                                },
                                |payload, _point, _, cx| {
                                    let title = payload.title.clone();
                                    cx.stop_propagation();
                                    cx.new(|_| TabGhost { title })
                                },
                            )
                            .when(exited, |el| el.opacity(0.55))
                            .child(
                                crate::icons::icon(crate::icons::TERMINAL)
                                    .size(px(16.0))
                                    .text_color(text_color.opacity(glyph_alpha)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .child(close_btn);

                        // Sliding transform while a sibling is dragged over: animate
                        // 150 ms between committed offsets.
                        match drag {
                            Some((from, over, epoch, prev_over)) if ix != from => {
                                let target = slide_offset(ix, from, over) * TAB_WIDTH;
                                let start = slide_offset(ix, from, prev_over) * TAB_WIDTH;
                                div()
                                    .relative()
                                    .child(tab_el.with_animation(
                                        ("terminal-tab-slide", key | ((epoch as u64) << 32)),
                                        TAB_SLIDE.animation(),
                                        move |el, t| el.left(px(motion::lerp(start, target, t))),
                                    ))
                                    .into_any_element()
                            }
                            // Invisible spacer — the ghost carries the tab; a
                            // dimmed original overlapped the sibling that
                            // slides into the vacated slot.
                            Some((from, ..)) if ix == from => div()
                                .w(px(TAB_WIDTH))
                                .h(px(28.0))
                                .flex_none()
                                .into_any_element(),
                            _ => tab_el.into_any_element(),
                        }
                    }),
            )
            .child(
                div()
                    .id("terminal-new-tab")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    // komet terminal-panel.tsx icon buttons: `transition-colors`.
                    .bg(motion::hover_blend(
                        "term-new-tab",
                        gpui::transparent_black(),
                        crate::theme::ink(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-new-tab", cx.entity_id()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let chat = this.session_key(cx);
                        this.open_tab(chat, cx);
                    }))
                    .child(
                        crate::icons::icon(crate::icons::PLUS)
                            .size(px(16.0))
                            .text_color(theme.text_muted.opacity(0.6)),
                    ),
            )
            // Collapse chevron pinned right (komet "Hide terminal" ⌘J).
            .child(div().flex_1())
            .child(
                div()
                    .id("terminal-collapse")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "term-collapse",
                        gpui::transparent_black(),
                        crate::theme::ink(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-collapse", cx.entity_id()))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleTerminal), cx);
                    })
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                            .size(px(13.0))
                            .text_color(theme.text_muted.opacity(0.55)),
                    ),
            )
    }
}

enum StreamDisposition {
    Continue,
    Stop,
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the bar.
        if self.drag.is_some() && !cx.has_active_drag() {
            self.drag = None;
        }
        // Embedded, the RIGHT PANE's own surface shows through — a second
        // fill here stacked another shade on the pane (user report); the
        // drawer keeps its own tone.
        let panel_bg: Option<gpui::Hsla> = (!self.embedded).then(|| terminal_panel_bg(&theme));
        let chat = self.session_key(cx);
        let focused = self.focus_handle.is_focused(window);

        // Embedded (right-pane surface host): the shell's surface tabs
        // replace the internal bar.
        let tab_bar: Option<gpui::AnyElement> =
            (!self.embedded).then(|| self.render_tab_bar(&chat, cx).into_any_element());
        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(panel_bg, |el, bg| el.bg(bg))
            .children(tab_bar)
            .children(self.render_context_menu(&theme, cx))
            .child(
                div()
                    .id("terminal-body")
                    .flex_1()
                    .min_h_0()
                    .key_context("Terminal")
                    .track_focus(&self.focus_handle)
                    .on_action(cx.listener(|this, _: &NewTerminalTab, window, cx| {
                        this.action_new_tab(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &CloseTerminalTab, window, cx| {
                        this.action_close_tab(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &NextTerminalTab, _, cx| {
                        this.action_cycle_tab(1, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PrevTerminalTab, _, cx| {
                        this.action_cycle_tab(-1, cx);
                    }))
                    .on_action(cx.listener(|this, _: &RenameTerminalTab, window, cx| {
                        this.action_rename_tab(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &IncreaseTerminalFont, _, cx| {
                        this.action_zoom_font(1.0, cx);
                    }))
                    .on_action(cx.listener(|this, _: &DecreaseTerminalFont, _, cx| {
                        this.action_zoom_font(-1.0, cx);
                    }))
                    .on_action(cx.listener(|this, _: &ResetTerminalFont, _, cx| {
                        this.action_zoom_font(0.0, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PrevTerminalCommand, _, cx| {
                        this.action_command_jump(CommandMotion::Older, cx);
                    }))
                    .on_action(cx.listener(|this, _: &NextTerminalCommand, _, cx| {
                        this.action_command_jump(CommandMotion::Newer, cx);
                    }))
                    .on_action(cx.listener(|this, _: &CurrentCommandOutput, _, cx| {
                        this.action_command_jump(CommandMotion::CurrentOutput, cx);
                    }))
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_context_menu))
                    // Middle-click: relayed when the app owns the pointer. Bound
                    // on the body grid, not the tab strip, so a middle-click on
                    // a tab still closes it.
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_click))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    // Bound on the window, not the element: a drag that ends
                    // outside the panel still has to end the gesture, or the
                    // next unrelated pointer move keeps extending a selection
                    // the user let go of.
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    // Release reports for the middle/right buttons while a mouse
                    // protocol is relaying a gesture (no-ops otherwise).
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        let line_h = this.line_height();
                        let lines = match event.delta {
                            ScrollDelta::Lines(delta) => delta.y,
                            ScrollDelta::Pixels(delta) => f32::from(delta.y) / line_h,
                        };
                        let step = lines.round() as i32;
                        // The app owns the wheel under a mouse protocol, or on
                        // the alt screen (AltScroll); otherwise scroll locally.
                        if this.relay_wheel(event, step, cx) {
                            return;
                        }
                        this.scroll_active(step, cx);
                    }))
                    .child(
                        div()
                            .size_full()
                            .relative()
                            .child(TerminalElement::new(cx.entity(), focused))
                            .children(self.render_rename_overlay(cx)),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests name these; the panel itself reads the blocks through the
    // [`Emulator`] seam.
    use komet_term::{BlockStatus, CommandSource};

    #[test]
    fn menu_availability_tracks_selection_and_clipboard() {
        // Selection + clipboard text: both rows live.
        assert_eq!(
            TerminalMenuAvailability::new(true, true),
            TerminalMenuAvailability {
                copy: true,
                paste: true,
            }
        );
        // No selection: Copy greys out, Paste still offered.
        assert_eq!(
            TerminalMenuAvailability::new(false, true),
            TerminalMenuAvailability {
                copy: false,
                paste: true,
            }
        );
        // Empty clipboard: Paste greys out, Copy still offered.
        assert_eq!(
            TerminalMenuAvailability::new(true, false),
            TerminalMenuAvailability {
                copy: true,
                paste: false,
            }
        );
        assert!(!TerminalMenuAvailability::new(false, false).copy);
        assert!(!TerminalMenuAvailability::new(false, false).paste);
    }

    #[test]
    fn tab_title_priority_rename_then_osc_then_session() {
        // A rename wins over everything, OSC over the session identity.
        assert_eq!(
            tab_title(
                Some("my work"),
                Some("vim main.rs"),
                Some("/home/me"),
                Some("/bin/zsh"),
                "Terminal 1"
            ),
            "my work"
        );
        assert_eq!(
            tab_title(
                None,
                Some("vim main.rs"),
                Some("/home/me"),
                Some("/bin/zsh"),
                "Terminal 1"
            ),
            "vim main.rs"
        );
        // No OSC title: the useful `cwd · shell` label.
        assert_eq!(
            tab_title(None, None, Some("/home/me"), Some("/usr/bin/fish"), "T"),
            "/home/me · fish"
        );
        // Whitespace-only rename falls through to the OSC title.
        assert_eq!(
            tab_title(Some("   "), Some("top"), Some("/x"), Some("zsh"), "T"),
            "top"
        );
        // Empty OSC (zsh sends `ESC]2;` after a BEL): skipped, not blanked.
        assert_eq!(
            tab_title(None, Some(""), Some("/x"), Some("zsh"), "T"),
            "/x · zsh"
        );
        // Partial identity, then the positional fallback.
        assert_eq!(tab_title(None, None, Some("/x"), None, "T"), "/x");
        assert_eq!(tab_title(None, None, None, Some("zsh"), "T"), "zsh");
        assert_eq!(
            tab_title(None, None, None, None, "Terminal 3"),
            "Terminal 3"
        );
    }

    #[test]
    fn tab_step_wraps_and_refuses_empty_strips() {
        assert_eq!(tab_step(0, 3, 1), Some(1));
        assert_eq!(tab_step(2, 3, 1), Some(0), "next at the tail wraps");
        assert_eq!(tab_step(0, 3, -1), Some(2), "prev at the head wraps");
        assert_eq!(tab_step(1, 3, -1), Some(0));
        assert_eq!(tab_step(0, 1, 1), Some(0), "a single tab is stable");
        assert_eq!(tab_step(0, 0, 1), None, "nothing to cycle");
    }

    #[test]
    fn zoomed_font_size_clamps_to_settings_bounds() {
        assert_eq!(zoomed_font_size(13.0, 1.0), 14.0);
        assert_eq!(zoomed_font_size(13.0, -1.0), 12.0);
        assert_eq!(
            zoomed_font_size(TERMINAL_FONT_SIZE_MAX, 1.0),
            TERMINAL_FONT_SIZE_MAX
        );
        assert_eq!(
            zoomed_font_size(TERMINAL_FONT_SIZE_MIN, -1.0),
            TERMINAL_FONT_SIZE_MIN
        );
    }

    #[test]
    fn rename_edits_map_keys_to_buffer_actions() {
        assert_eq!(rename_edit_for("enter", Some("\r")), RenameEdit::Commit);
        assert_eq!(rename_edit_for("escape", Some("\x1b")), RenameEdit::Cancel);
        assert_eq!(
            rename_edit_for("backspace", Some("\x7f")),
            RenameEdit::Backspace
        );
        assert_eq!(rename_edit_for("a", Some("a")), RenameEdit::Insert('a'));
        assert_eq!(rename_edit_for("space", Some(" ")), RenameEdit::Insert(' '));
        assert_eq!(
            rename_edit_for("f2", None),
            RenameEdit::Ignored,
            "function keys carry no char and are swallowed"
        );
        assert_eq!(rename_edit_for("tab", Some("\t")), RenameEdit::Ignored);
        assert_eq!(rename_edit_for("ctrl-a", None), RenameEdit::Ignored);
    }

    #[test]
    fn pointer_button_and_modifier_mapping() {
        // The three reportable buttons map to the encoder's vocabulary.
        assert_eq!(
            report_button_of(MouseButton::Left),
            Some(ReportButton::Left)
        );
        assert_eq!(
            report_button_of(MouseButton::Middle),
            Some(ReportButton::Middle)
        );
        assert_eq!(
            report_button_of(MouseButton::Right),
            Some(ReportButton::Right)
        );
        // A navigation button has no terminal report → the UI keeps it.
        assert_eq!(
            report_button_of(MouseButton::Navigate(gpui::NavigationDirection::Back)),
            None
        );

        let mods = Modifiers {
            control: true,
            alt: true,
            shift: false,
            ..Default::default()
        };
        assert_eq!(
            pointer_mods(&mods),
            PointerMods {
                shift: false,
                alt: true,
                ctrl: true,
            }
        );
    }

    #[test]
    fn height_clamps_between_160_and_55vh() {
        assert_eq!(clamp_terminal_height(300.0, 900.0), 300.0);
        assert_eq!(clamp_terminal_height(10.0, 900.0), 160.0);
        assert_eq!(clamp_terminal_height(4000.0, 900.0), 900.0 * 0.55);
        // Tiny windows: min wins over the 55vh cap.
        assert_eq!(clamp_terminal_height(200.0, 100.0), 160.0);
        assert_eq!(clamp_terminal_height(f32::NAN, 900.0), 160.0);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_ms(0), 500);
        assert_eq!(backoff_ms(1), 1000);
        assert_eq!(backoff_ms(2), 2000);
        assert_eq!(backoff_ms(3), 4000);
        assert_eq!(backoff_ms(4), 8000);
        assert_eq!(backoff_ms(10), 8000);
        assert_eq!(backoff_ms(u32::MAX), 8000);
    }

    #[test]
    fn canvas_session_key_prefers_the_chat_then_the_space() {
        assert_eq!(
            canvas_session_key(Some("chat-1"), Some("space-1")),
            "chat-1"
        );
        assert_eq!(
            canvas_session_key(None, Some("space-1")),
            "space-canvas:space-1"
        );
        assert_eq!(
            canvas_session_key(Some(""), Some("space-1")),
            "space-canvas:space-1"
        );
        assert_eq!(canvas_session_key(None, None), "space-canvas:");
    }

    #[test]
    fn reorder_moves_forward_and_backward() {
        let mut v = vec!["a", "b", "c", "d"];
        reorder_tabs(&mut v, 0, 2);
        assert_eq!(v, ["b", "c", "a", "d"]);
        reorder_tabs(&mut v, 3, 0);
        assert_eq!(v, ["d", "b", "c", "a"]);
        // Out-of-range / no-op moves leave the vec untouched.
        reorder_tabs(&mut v, 9, 0);
        reorder_tabs(&mut v, 1, 1);
        assert_eq!(v, ["d", "b", "c", "a"]);
    }

    #[test]
    fn drop_index_quantizes_and_clamps() {
        assert_eq!(drop_index(-10.0, 150.0, 3), 0);
        assert_eq!(drop_index(0.0, 150.0, 3), 0);
        assert_eq!(drop_index(149.0, 150.0, 3), 0);
        assert_eq!(drop_index(150.0, 150.0, 3), 1);
        assert_eq!(drop_index(700.0, 150.0, 3), 2);
        assert_eq!(drop_index(50.0, 150.0, 0), 0);
    }

    #[test]
    fn slide_offsets_shift_toward_the_gap() {
        // Dragging 0 over 2: tabs 1 and 2 slide left one slot.
        assert_eq!(slide_offset(0, 0, 2), 0.0);
        assert_eq!(slide_offset(1, 0, 2), -1.0);
        assert_eq!(slide_offset(2, 0, 2), -1.0);
        assert_eq!(slide_offset(3, 0, 2), 0.0);
        // Dragging 3 over 1: tabs 1 and 2 slide right.
        assert_eq!(slide_offset(0, 3, 1), 0.0);
        assert_eq!(slide_offset(1, 3, 1), 1.0);
        assert_eq!(slide_offset(2, 3, 1), 1.0);
        assert_eq!(slide_offset(3, 3, 1), 0.0);
        // Hovering the origin: nothing moves.
        for ix in 0..4 {
            assert_eq!(slide_offset(ix, 2, 2), 0.0);
        }
    }

    #[test]
    fn active_index_tracks_reorders() {
        // The active tab itself moves.
        assert_eq!(active_after_reorder(1, 1, 3), 3);
        // A tab hopping over the active one from the left shifts it down.
        assert_eq!(active_after_reorder(2, 0, 3), 1);
        // …and from the right shifts it up.
        assert_eq!(active_after_reorder(1, 3, 0), 2);
        // Disjoint moves leave it alone.
        assert_eq!(active_after_reorder(0, 2, 3), 0);
    }

    #[test]
    fn active_index_tracks_closes() {
        assert_eq!(active_after_close(2, 0, 3), 1); // close left of active
        assert_eq!(active_after_close(1, 1, 2), 1); // close active mid-list
        assert_eq!(active_after_close(2, 2, 2), 1); // close active at tail
        assert_eq!(active_after_close(0, 0, 0), 0); // last tab closed
    }

    #[test]
    fn exit_message_format() {
        let text = String::from_utf8(exit_message(0)).unwrap();
        assert!(text.contains("[process exited 0]"));
        let text = String::from_utf8(exit_message(137)).unwrap();
        assert!(text.contains("[process exited 137]"));
        assert!(text.starts_with("\r\n"));
        assert!(text.ends_with("\r\n"));
    }

    #[test]
    fn shell_titles() {
        assert_eq!(shell_title("/bin/zsh"), "zsh");
        assert_eq!(shell_title("/usr/local/bin/fish"), "fish");
        assert_eq!(shell_title("C:\\Windows\\System32\\cmd.exe"), "cmd.exe");
        assert_eq!(shell_title("bash"), "bash");
        assert_eq!(shell_title(""), "terminal");
    }

    #[test]
    fn stream_events_deserialize_per_contract() {
        let data: TerminalEvent =
            serde_json::from_str(r#"{"type":"data","seq":7,"data":"aGk="}"#).unwrap();
        assert_eq!(
            data,
            TerminalEvent::Data {
                seq: 7,
                data: "aGk=".into()
            }
        );
        let exit: TerminalEvent =
            serde_json::from_str(r#"{"type":"exit","seq":8,"exitCode":130}"#).unwrap();
        assert_eq!(
            exit,
            TerminalEvent::Exit {
                seq: 8,
                exit_code: 130,
                signal: None
            }
        );
        let session: TerminalSession =
            serde_json::from_str(r#"{"id":"t1","cwd":"/w","shell":"/bin/zsh"}"#).unwrap();
        assert_eq!(session.id, "t1");
        assert_eq!(session.shell, "/bin/zsh");
    }

    #[test]
    fn base64_round_trip_and_tolerance() {
        assert_eq!(decode_base64("aGk="), b"hi".to_vec());
        assert_eq!(
            decode_base64("aGk"),
            b"hi".to_vec(),
            "unpadded input tolerated"
        );
        assert_eq!(
            decode_base64("!!!"),
            Vec::<u8>::new(),
            "garbage decodes to nothing"
        );
        assert_eq!(encode_base64(b"hi"), "aGk=");
    }

    #[test]
    fn exit_message_feeds_cleanly_through_the_emulator() {
        let mut emulator = Emulator::new(40, 4);
        emulator.feed(b"$ done");
        emulator.feed(&exit_message(1));
        assert_eq!(emulator.row_text(1), "[process exited 1]");
    }

    // ---- OSC 133 command navigation ----

    /// A hand-built block shaped like the tracker's output for one marked
    /// command: prompt on `prompt_line`, output on `output_start..output_end`.
    fn marked_block(index: usize, prompt_line: usize, output_start: usize) -> CommandBlock {
        CommandBlock {
            index,
            status: BlockStatus::Done,
            command: Some(format!("cmd{index}")),
            command_source: CommandSource::Typed,
            cwd: None,
            exit_code: Some(0),
            marker_backed: true,
            seq_start: 0,
            seq_end: 0,
            output_start_line: output_start,
            output_end_line: output_start + 2,
            prompt_line: Some(prompt_line),
        }
    }

    /// Three commands: prompts on lines 1/4/7, outputs starting on 2/5/8.
    fn three_commands() -> Vec<CommandBlock> {
        vec![
            marked_block(0, 1, 2),
            marked_block(1, 4, 5),
            marked_block(2, 7, 8),
        ]
    }

    /// A silence-fallback block: no prompt line, no marker, nothing to trust.
    fn unmarked_block(index: usize, output_start: usize) -> CommandBlock {
        CommandBlock {
            prompt_line: None,
            marker_backed: false,
            ..marked_block(index, output_start, output_start)
        }
    }

    #[test]
    fn command_navigation_walks_prompts_strictly_relative_to_the_viewport() {
        let blocks = three_commands();
        // Backwards from the live bottom: 7, then 4, then 1, then nothing —
        // each press moves one boundary, never sticking on the same one.
        let mut walked = Vec::new();
        let mut at = 10usize;
        while let CommandJump::Line(line) = command_jump(true, &blocks, at, CommandMotion::Older) {
            walked.push(line);
            at = line;
        }
        assert_eq!(walked, vec![7, 4, 1]);
        // Forwards mirrors it.
        let mut forward = Vec::new();
        let mut at = 1usize;
        while let CommandJump::Line(line) = command_jump(true, &blocks, at, CommandMotion::Newer) {
            forward.push(line);
            at = line;
        }
        assert_eq!(forward, vec![4, 7]);
        // Past the last prompt the walk ends on the live prompt, so the
        // shortcut goes to the bottom instead of dead-ending.
        assert_eq!(
            command_jump(true, &blocks, 7, CommandMotion::Newer),
            CommandJump::Bottom
        );
        // Above the first prompt there is nowhere older to go.
        assert_eq!(
            command_jump(true, &blocks, 1, CommandMotion::Older),
            CommandJump::Nothing
        );
    }

    #[test]
    fn current_command_output_skips_the_prompt_and_shows_the_answer() {
        let blocks = three_commands();
        // Standing on cmd1's prompt (line 4): the jump goes to the first line
        // its command printed, which is where the answer starts.
        assert_eq!(
            command_jump(true, &blocks, 4, CommandMotion::CurrentOutput),
            CommandJump::Line(5)
        );
        // Standing inside the output: the same block, so its first line — which
        // is at or above the viewport top, so `jump_delta` reports no movement.
        assert_eq!(
            command_jump(true, &blocks, 6, CommandMotion::CurrentOutput),
            CommandJump::Line(5)
        );
        assert_eq!(jump_delta(CommandJump::Line(5), 4, 4), None);
        // Above every prompt there is no command to show the output of.
        assert_eq!(
            command_jump(true, &blocks, 0, CommandMotion::CurrentOutput),
            CommandJump::Nothing
        );
    }

    /// The documented fallback: a shell that never sent a marker (no injection,
    /// a PS1 that overwrote it) has no prompt lines to trust, so all three
    /// shortcuts are inert — including the forward one, which would otherwise
    /// answer "go to the bottom" and scroll the viewport for no reason.
    #[test]
    fn command_navigation_is_inert_for_a_shell_without_markers() {
        let blocks = vec![unmarked_block(0, 2), unmarked_block(1, 5)];
        for motion in [
            CommandMotion::Older,
            CommandMotion::Newer,
            CommandMotion::CurrentOutput,
        ] {
            assert_eq!(
                command_jump(false, &blocks, 9, motion),
                CommandJump::Nothing,
                "{motion:?} must not move a marker-less viewport"
            );
        }
        // Same blocks, markers seen elsewhere in the stream: a block that was
        // synthesized from silence is still not a boundary anyone may claim.
        assert_eq!(
            command_jump(true, &blocks, 9, CommandMotion::Older),
            CommandJump::Nothing
        );
    }

    #[test]
    fn jump_delta_reports_whether_the_viewport_actually_moved() {
        // Line jump: offset measured against what the grid can show.
        assert_eq!(
            jump_delta(CommandJump::Line(3), 0, 6),
            Some(6),
            "scrolling back 6 lines into history"
        );
        assert_eq!(
            jump_delta(CommandJump::Line(3), 6, 0),
            Some(-6),
            "and forward again"
        );
        assert_eq!(
            jump_delta(CommandJump::Line(3), 5, 5),
            None,
            "already there: the keystroke is inert, not a jump to nowhere"
        );
        // The bottom is display offset 0.
        assert_eq!(jump_delta(CommandJump::Bottom, 4, 0), Some(-4));
        assert_eq!(jump_delta(CommandJump::Bottom, 0, 0), None);
        assert_eq!(jump_delta(CommandJump::Nothing, 4, 0), None);
    }

    /// End to end on a real grid: feed what the injected shell sends, then walk
    /// the commands the way the panel does. The viewport top has to land on the
    /// prompt line itself, so the human sees the command re-appear at the top
    /// row — in the scrollback, not just in theory.
    #[test]
    fn marked_grid_walks_its_commands_and_shows_each_prompt() {
        let mut emu = Emulator::with_scrollback(40, 4, 200);
        for i in 0..3 {
            emu.feed(
                format!(
                    "\x1b]133;A\x07p$ \x1b]133;B\x07cmd{i}\r\n\
                     \x1b]133;C\x07out{i}a\r\nout{i}b\r\n\x1b]133;D;0\x07"
                )
                .as_bytes(),
            );
        }
        assert!(emu.saw_markers());
        assert_eq!(emu.command_blocks().len(), 3);
        assert!(emu.history_lines() > 0, "the early ones are scrollback");
        // The grid is 4 rows tall and three 3-line commands ran, so the newest
        // command's prompt is already the top row at the live bottom.
        assert_eq!(emu.row_text(0), "p$ cmd2");

        let mut seen = Vec::new();
        for _ in 0..4 {
            let jump = command_jump(
                emu.saw_markers(),
                &emu.command_blocks(),
                emu.top_visible_line(),
                CommandMotion::Older,
            );
            let CommandJump::Line(line) = jump else {
                break;
            };
            let Some(delta) =
                jump_delta(jump, emu.display_offset(), emu.scroll_offset_for_line(line))
            else {
                break;
            };
            emu.scroll(delta);
            assert_eq!(
                emu.top_visible_line(),
                line,
                "the jump put that command's own prompt at the top row"
            );
            seen.push(emu.row_text(0));
        }
        // One boundary per press, and the walk starts above what is already on
        // screen: a line inside the viewport cannot be scrolled to the top row
        // without scrolling past the bottom, so jumping to a visible prompt
        // would be an inert keystroke, not a move.
        assert_eq!(
            seen,
            vec!["p$ cmd1", "p$ cmd0"],
            "two presses walk two boundaries, oldest last"
        );
        // And above the first prompt there is nowhere left to go.
        assert_eq!(
            command_jump(
                emu.saw_markers(),
                &emu.command_blocks(),
                emu.top_visible_line(),
                CommandMotion::Older,
            ),
            CommandJump::Nothing
        );

        // Forward returns the way it came, prompt by prompt.
        for expect in ["p$ cmd1", "p$ cmd2"] {
            let jump = command_jump(
                emu.saw_markers(),
                &emu.command_blocks(),
                emu.top_visible_line(),
                CommandMotion::Newer,
            );
            let CommandJump::Line(line) = jump else {
                panic!("there is a command below: {jump:?}");
            };
            let delta =
                jump_delta(jump, emu.display_offset(), emu.scroll_offset_for_line(line)).unwrap();
            emu.scroll(delta);
            assert_eq!(emu.row_text(0), expect);
        }
        // Nothing below the newest prompt: the walk ends on the live prompt,
        // and the viewport is already home, so the press does not move.
        assert_eq!(
            command_jump(
                emu.saw_markers(),
                &emu.command_blocks(),
                emu.top_visible_line(),
                CommandMotion::Newer,
            ),
            CommandJump::Bottom
        );
        assert_eq!(
            jump_delta(CommandJump::Bottom, emu.display_offset(), 0),
            None
        );
    }

    /// A jump toward a prompt the scrollback already dropped must not fail or
    /// scroll into nothing: the offset is clamped to the oldest line the grid
    /// still holds, which is the nearest position that exists.
    #[test]
    fn a_walk_past_the_scrollback_clamps_to_what_survives() {
        let mut emu = Emulator::with_scrollback(40, 4, 8);
        for i in 0..6 {
            emu.feed(
                format!(
                    "\x1b]133;A\x07p$ \x1b]133;B\x07cmd{i}\r\n\
                     \x1b]133;C\x07out{i}a\r\n\x1b]133;D;0\x07"
                )
                .as_bytes(),
            );
        }
        assert!(emu.saw_markers());
        let blocks = emu.command_blocks();
        let mut stops = 0;
        loop {
            let jump = command_jump(
                emu.saw_markers(),
                &blocks,
                emu.top_visible_line(),
                CommandMotion::Older,
            );
            let CommandJump::Line(line) = jump else {
                break;
            };
            let offset = emu.scroll_offset_for_line(line);
            let Some(delta) = jump_delta(jump, emu.display_offset(), offset) else {
                break;
            };
            assert!(
                offset <= emu.history_lines(),
                "never asks for a line the grid cannot show"
            );
            emu.scroll(delta);
            assert_eq!(
                emu.display_offset(),
                offset,
                "the jump lands where computed, clamped"
            );
            stops += 1;
            assert!(
                stops <= blocks.len(),
                "every press moves one boundary, so the walk terminates"
            );
        }
        assert!(
            stops >= 2,
            "a trimmed history is tolerated, not a dead shortcut: {stops} stops"
        );
    }

    /// No markers on the real grid either: the silence fallback does build a
    /// block, and the shortcuts still refuse to move.
    #[test]
    fn unmarked_grid_keeps_the_shortcuts_inert() {
        let mut emu = Emulator::new(40, 4);
        emu.feed(b"user@host:~$ ls\r\nfile.txt\r\n");
        assert!(emu.settle_silence());
        assert!(!emu.saw_markers());
        assert!(!emu.command_blocks().is_empty());
        for motion in [
            CommandMotion::Older,
            CommandMotion::Newer,
            CommandMotion::CurrentOutput,
        ] {
            let before = emu.display_offset();
            let jump = command_jump(
                emu.saw_markers(),
                &emu.command_blocks(),
                emu.top_visible_line(),
                motion,
            );
            assert_eq!(jump, CommandJump::Nothing, "{motion:?} leaked a jump");
            assert_eq!(
                jump_delta(jump, before, 0),
                None,
                "and the grid never moved"
            );
        }
    }
}
