// Transcript — virtualized block-granularity rows with stick-to-bottom.
//
// Desktop parity (transcript.rs): GAP_TURN 14 / GAP_BLOCK 8 / MD_BLOCK_GAP 12,
// content column max 736, re-engage band 70, jump-button threshold 320,
// bottom pad 24. Rows are identified by stable ids and versioned by content
// fingerprints, so a streamed token re-renders exactly one row. SwiftUI's lazy
// stack + scroll APIs stand in for gpui's list(): the pin breaks only on
// user scroll-up and re-engages when approaching the bottom.

import SwiftUI

struct TranscriptView: View {
    let store: SessionStore
    let chatId: String

    init(store: SessionStore, chatId: String) {
        self.store = store
        self.chatId = chatId
        // Seeded, not defaulted to false: a transcript that has already been
        // revealed must come back visible on the FIRST frame after any view
        // re-creation, or it blinks out mid-typing when the composer resizes.
        _settled = State(initialValue: store.hasRevealed)
        _hydrated = State(initialValue: store.hasRevealed)
    }

    static let gapTurn: CGFloat = 14
    static let gapBlock: CGFloat = 8
    static let maxContentWidth: CGFloat = 736
    static let stickThreshold: CGFloat = 70
    static let jumpThreshold: CGFloat = 320

    @State private var veils = VeilStore()
    @State private var folds: [String: Bool] = [:]
    /// One-shot guard for the first non-empty projection.
    @State private var hydrated = false
    /// Gates the reveal: false until the transcript has landed at the bottom.
    @State private var settled = false
    /// Per-frame scroll tracking. A reference type, NOT view @State: writing
    /// @State on every scroll frame re-evaluated this whole body (a ForEach
    /// over every row) per frame — the further up the user had scrolled, the
    /// more realized rows each pass had to diff, which is exactly the
    /// "scrolling gets laggier the deeper I go" jank.
    @State private var scroll = ScrollState()
    @State private var scrollPosition = ScrollPosition(edge: .bottom)
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        // The parse cache lives on the store (one per session, prewarmed
        // off-main), so opening a chat assembles rows from settled parses
        // instead of re-parsing the whole transcript on the main thread.
        let rows = store.transcriptCache.rows(revision: store.revision,
                                              entries: store.entries,
                                              pendingSends: store.pendingSends)
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(rows) { row in
                    rowView(row).id(row.id)
                }
                Color.clear.frame(height: 44)  // bottom pad clears the fade + floating status strip
            }
            .frame(maxWidth: Self.maxContentWidth)
            .frame(maxWidth: .infinity)
        }
        .scrollPosition($scrollPosition)
        .defaultScrollAnchor(.bottom)
        // Drag past the composer and the keyboard follows the finger down —
        // the Messages-style interactive dismissal.
        .scrollDismissesKeyboard(.interactively)
        // Tap anywhere in the transcript to put the keyboard away (t3's
        // tap-to-blur; TapGesture already cancels on drag-sized movement).
        // Simultaneous so fold toggles and row buttons still receive theirs.
        .simultaneousGesture(TapGesture().onEnded {
            UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder),
                                            to: nil, from: nil, for: nil)
        })
        // Held invisible until it has settled at the bottom, then faded in.
        // The settling itself is unavoidable (see settleToBottom) — what is
        // avoidable is WATCHING it: painting mid-settle is what read as the
        // transcript sliding on load.
        .opacity(settled ? 1 : 0)
        .motionAnimation(Motion.fadeQuick, value: settled)
        .background(Theme.bg)
        .task {
            // Warm sessions already have rows at first layout, and `onChange`
            // never fires for an initial value — this is the only hook for them.
            await settleToBottom()
        }
        .onChange(of: rows.isEmpty) { _, isEmpty in
            // Projection is off-main, so a cached transcript usually lands after
            // the pass above ran on an empty list. Only ever hides a transcript
            // that has never been shown — re-hiding a visible one is what made
            // it blink out mid-typing. `hydrated` is the one-shot: it seeds
            // true for stores that have already revealed content, so this
            // fires exactly once, on a fresh store's first non-empty rows.
            guard !isEmpty, !hydrated else { return }
            hydrated = true
            settled = false
            Task { await settleToBottom() }
        }
        .onScrollGeometryChange(for: CGFloat.self) { $0.contentSize.height } action: { [scroll] old, new in
            scroll.contentHeight = new
            // Estimated row heights keep resolving after the settle, and a
            // SHRINK leaves the held offset past the content — a black
            // viewport until the user's scroll clamps it (t3 re-pins on
            // itemLayout for exactly this). Keep a pinned feed glued through
            // reflows; never touch a user's in-flight drag.
            if scroll.pinned, !scroll.userScrolling, new < old - 1 {
                scrollPosition.scrollTo(edge: .bottom)
            }
        }
        .onScrollGeometryChange(for: CGFloat.self) { $0.containerSize.height + $0.contentInsets.bottom } action: { _, _ in
            // The viewport resized under the content — keyboard up/down, the
            // composer's capsule↔card morph, the question-panel swap. t3's
            // rule for exactly this ("keyboardLiftBehavior=whenAtEnd" +
            // inset re-report on remount): a feed pinned at the end follows
            // the new bottom IMMEDIATELY; an unpinned reader stays put —
            // unless the resize stranded the offset past the content (blank
            // viewport), which is clamped back to the bottom. Without this,
            // focusing the composer could leave the transcript scrolled out
            // of range (blank until touched) or resting a viewport short
            // (content "appears" only when the next resize brought it back).
            guard !scroll.userScrolling else { return }  // their drag wins
            if scroll.pinned || scroll.distanceFromBottom < -1 {
                // Unanimated: this tracks the keyboard's own animation
                // frames; a spring here would fight it.
                scrollPosition.scrollTo(edge: .bottom)
            }
        }
        .onScrollPhaseChange { [scroll] _, newPhase in
            // Desktop rule: the pin breaks only on USER input (wheel-up/drag),
            // never on streaming growth. Phases track the gesture.
            scroll.userScrolling = newPhase == .interacting || newPhase == .decelerating
        }
        .onScrollGeometryChange(for: CGFloat.self) { geo in
            // Inset-proof distance: visibleRect is already contentInsets-
            // adjusted, so this is 0 at the VISUAL bottom whether or not the
            // keyboard is up. The old offset+insets formula double-counted
            // the keyboard inset (~keyboard-height at the bottom), which
            // both pinned the jump button on and put the 70pt re-stick band
            // permanently out of reach while typing. Unclamped on purpose:
            // negative = stranded past the content (the resize clamp's cue).
            geo.contentSize.height - geo.visibleRect.maxY
        } action: { [scroll] old, new in
            scroll.distanceFromBottom = new
            if scroll.userScrolling, new > old + 1, new > 2 {
                scroll.pinned = false
            } else if !scroll.pinned, new <= Self.stickThreshold, new < old {
                // Re-stick only when moving TOWARD the bottom inside the 70pt
                // band, else the pin would be unbreakable.
                scroll.pinned = true
            }
            // The only observable write, and only at the threshold crossing —
            // it re-renders the tiny jump button, never this body. Gated on
            // the PIN, not just distance: with the keyboard up, UIKit
            // double-counts the bottom inset (t3's "nativeInsetOvercount"),
            // so distance reads ~keyboard-height at the visual bottom and the
            // raw threshold kept the button up for a pinned feed.
            let show = !scroll.pinned && new > Self.jumpThreshold
            if scroll.showJump != show { scroll.showJump = show }
        }
        .onChange(of: contentSignature(rows)) {
            guard scroll.pinned else { return }
            if reduceMotion {
                scrollPosition.scrollTo(edge: .bottom)
            } else {
                withAnimation(.spring(duration: 0.3)) {
                    scrollPosition.scrollTo(edge: .bottom)
                }
            }
        }
        .overlay(alignment: .top) {
            // Soft fade under the nav bar — content dissolves instead of
            // hard-clipping against the header.
            LinearGradient(
                stops: [
                    .init(color: Theme.bg, location: 0),
                    .init(color: Theme.bg.opacity(0.85), location: 0.45),
                    .init(color: Theme.bg.opacity(0), location: 1),
                ],
                startPoint: .top, endPoint: .bottom
            )
            .frame(height: 130)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
        }
        // The bottom dissolve lives on SessionView's composer inset (one
        // continuous gradient from above the status strip to the physical
        // bottom edge) — a second ramp here would double-darken the rows
        // right where they slide under the glass.
        .overlay(alignment: .bottomTrailing) {
            // Jump-to-bottom floats ABOVE the fades. A child view so only IT
            // observes the show flag — the transcript body stays out of the
            // per-frame scroll path.
            JumpToBottomButton(scroll: scroll) {
                scroll.pinned = true
                withAnimation(.spring(duration: 0.35)) {
                    scrollPosition.scrollTo(edge: .bottom)
                }
            }
            .padding(.trailing, 16)
            // 12pt above the COMPOSER, not the safe-area edge: the edge
            // now sits atop the 24pt status-strip band (SessionView's
            // inset), so dip into it — the strip never hit-tests.
            .padding(.bottom, -12)
        }
    }

    /// Hold the bottom until layout stops moving, then reveal.
    ///
    /// A lazy stack only measures the rows near the viewport; the rest carry
    /// ESTIMATED heights that resolve over the next frames, growing the content
    /// and moving the real bottom. One snap at any single instant lands short —
    /// measured, up to ~37 turns short on a 120-turn transcript. SwiftUI's own
    /// `.defaultScrollAnchor(.bottom, for: .sizeChanges)` does not help: it
    /// PRESERVES the initial estimated position rather than correcting to the
    /// true bottom, which lands short every time instead of sometimes.
    ///
    /// So: re-anchor each frame until the height repeats. Bounded (~480ms worst
    /// case) so a pathological reflow can't spin, and it yields the moment the
    /// user takes the scroll view — their drag wins. `settled` flips either way,
    /// so the transcript can never be left invisible.
    private func settleToBottom() async {
        var lastHeight: CGFloat = -1
        for _ in 0..<16 {
            guard scroll.pinned, !scroll.userScrolling else { break }
            scrollPosition.scrollTo(edge: .bottom)
            if scroll.contentHeight == lastHeight { break }
            lastHeight = scroll.contentHeight
            try? await Task.sleep(nanoseconds: 30_000_000)
        }
        settled = true
        // Revealed-with-CONTENT only: a settle that ran against a still-empty
        // projection must not latch, or the rows landing a beat later find
        // `hasRevealed` true, seed `hydrated`/`settled` from it on the next
        // view identity, and skip their own settle — the transcript then
        // rests wherever the empty pass left it (out of view until a resize
        // — focusing the composer — happened to drag it back).
        if !store.entries.isEmpty || !store.pendingSends.isEmpty {
            store.hasRevealed = true
        }
    }

    // Streamed growth signature: last row id + version + count. Any append or
    // reflow of the tail bumps it; scroll-back through history doesn't.
    private func contentSignature(_ rows: [TranscriptRow]) -> String {
        guard let last = rows.last else { return "" }
        return "\(rows.count)|\(last.id)|\(last.version)"
    }

    // MARK: Row rendering

    @ViewBuilder
    private func rowView(_ row: TranscriptRow) -> some View {
        Group {
            switch row.kind {
            case .user(let text):
                UserBubble(text: text, pending: row.timestamp == nil,
                           deviceId: store.hostDeviceId ?? "")

            case .markdown(let block, let streaming):
                MarkdownRowView(row: row, block: block, streaming: streaming, veils: veils)

            case .toolGroup(let tools, let autoOpen):
                ToolGroupView(tools: tools,
                              open: folds[row.id] ?? autoOpen,
                              userToggled: folds[row.id] != nil) {
                    withAnimation(reduceMotion ? nil : Motion.resize) {
                        folds[row.id] = !(folds[row.id] ?? autoOpen)
                    }
                }

            case .inputChip(let header, let resolved):
                InputChipView(header: header, resolved: resolved)

            case .errorChip(let message):
                ErrorChipView(message: message)
            }
        }
        .padding(.top, row.topGap)
        .padding(.horizontal, 16)
    }
}

/// Per-frame scroll tracking for the transcript. Only `showJump` is
/// observable — everything else is written every scroll frame and read only
/// inside closures/the settle loop, so observation (and with it, whole-body
/// re-evaluation per frame) must not see those writes.
@Observable
final class ScrollState {
    /// Flips at the jump threshold; observed by JumpToBottomButton alone.
    var showJump = false
    @ObservationIgnored var distanceFromBottom: CGFloat = 0
    @ObservationIgnored var contentHeight: CGFloat = 0
    @ObservationIgnored var pinned = true
    @ObservationIgnored var userScrolling = false
}

private struct JumpToBottomButton: View {
    let scroll: ScrollState
    let action: () -> Void

    var body: some View {
        ZStack(alignment: .bottomTrailing) {
            if scroll.showJump {
                Button(action: action) {
                    Image(systemName: "arrow.down")
                        .font(.system(size: 14, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .frame(width: 36, height: 36)
                }
                .glassEffect(.regular.interactive(), in: Circle())
                .transition(.opacity.combined(with: .move(edge: .bottom)))
            }
        }
        .motionAnimation(Motion.fadeQuick, value: scroll.showJump)
    }
}

/// Row-build cache: one incremental parser per streaming part plus a memo of
/// settled parses. Owned by the SessionStore (NOT view @State) so the parses
/// survive across view instances — re-opening a chat re-parses nothing.
@MainActor
final class TranscriptBuilderCache {
    private var parsers: [String: IncrementalMarkdownParser] = [:]
    private var completed: [String: CompletedParse] = [:]
    private var cachedRevision: UInt64?
    private var cachedRows: [TranscriptRow] = []
    private var prewarming = false

    /// Rows for the store's current `revision`. Rows only change when the doc
    /// does — gate on the revision and hand back the same array.
    func rows(revision: UInt64,
              entries: [MessageEntry],
              pendingSends: [(messageId: String, text: String, at: Int64)]) -> [TranscriptRow] {
        if cachedRevision == revision { return cachedRows }
        cachedRows = TranscriptRowBuilder.rows(entries: entries, pendingSends: pendingSends,
                                               parsers: &parsers, completed: &completed)
        cachedRevision = revision
        return cachedRows
    }

    /// Parse every settled part OFF the main thread and merge into the memo,
    /// so the first `rows()` of a freshly hydrated long session assembles from
    /// memo hits instead of parsing the whole transcript inside body — that
    /// synchronous parse was the "empty transcript for a while" on open.
    func prewarm(entries: [MessageEntry]) {
        guard !prewarming else { return }
        var jobs: [(key: String, text: String)] = []
        for entry in entries where entry.role != .user {
            let streaming = entry.status == .streaming
            let lastIx = entry.parts.indices.last
            for (ix, part) in entry.parts.enumerated() {
                guard case .text(let partId, let text) = part, !text.isEmpty else { continue }
                if streaming && ix == lastIx { continue }  // live tail: incremental parser's job
                let key = "\(entry.id)#\(partId)"
                if completed[key]?.source != text {
                    jobs.append((key, text))
                }
            }
        }
        guard !jobs.isEmpty else { return }
        prewarming = true
        Task { @MainActor [weak self] in
            let parsed = await Task.detached(priority: .userInitiated) {
                jobs.map { (key: $0.key, text: $0.text, blocks: MarkdownParser.parse($0.text)) }
            }.value
            guard let self else { return }
            self.prewarming = false
            for job in parsed where self.completed[job.key]?.source != job.text {
                self.completed[job.key] = CompletedParse(source: job.text, blocks: job.blocks)
            }
        }
    }
}

/// Veil registry — one RowVeil per live row, dropped on the live→complete flip.
@Observable
final class VeilStore {
    @ObservationIgnored private var veils: [String: RowVeil] = [:]

    func veil(for rowId: String, seeded: Bool) -> RowVeil {
        if let existing = veils[rowId] { return existing }
        let veil = RowVeil()
        veils[rowId] = veil
        return veil
    }

    func drop(_ rowId: String) {
        veils.removeValue(forKey: rowId)
    }
}

// MARK: - User bubble (transcript.rs:1671)

struct UserBubble: View {
    let text: String
    var pending = false
    /// The chat's host device — where attachment files live (read-back key).
    var deviceId = ""

    var body: some View {
        // Attachment refs ride the message text (message-attachments.ts
        // transport); split them out and render thumbnails above the bubble,
        // exactly like the desktop's user rows.
        let parsed = parseUserMessageImages(text)
        VStack(alignment: .trailing, spacing: 8) {
            if !parsed.attachments.isEmpty, !deviceId.isEmpty {
                UserAttachmentsStrip(deviceId: deviceId, attachments: parsed.attachments)
            }
            if !parsed.text.isEmpty {
                Text(parsed.text)
                    .font(Theme.sans(MD.textSize))
                    .lineSpacing(MD.lineHeight - MD.textSize - 4)
                    .foregroundStyle(Theme.text)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: Theme.bubbleRadius))
                    .frame(maxWidth: TranscriptView.maxContentWidth * 0.8, alignment: .trailing)
                    .contextMenu {
                        Button {
                            UIPasteboard.general.string = parsed.text
                        } label: {
                            Label("Copy", systemImage: "doc.on.doc")
                        }
                    }
            }
        }
        .opacity(pending ? 0.65 : 1)
        .frame(maxWidth: .infinity, alignment: .trailing)
    }
}

// MARK: - Markdown row with veil

struct MarkdownRowView: View {
    let row: TranscriptRow
    let block: MDBlock
    let streaming: Bool
    let veils: VeilStore

    var body: some View {
        if streaming, isVeilable {
            TimelineView(.animation) { _ in
                veiledText
            }
            .onDisappear { veils.drop(row.id) }
        } else {
            MarkdownBlockView(block: block, cacheKey: row.id)
        }
    }

    private var isVeilable: Bool {
        switch block {
        case .paragraph, .heading: return true
        default: return false
        }
    }

    @ViewBuilder
    private var veiledText: some View {
        let veil = veils.veil(for: row.id, seeded: false)
        switch block {
        case .paragraph(let runs):
            let _ = veil.noteLength(runs.map(\.text.count).reduce(0, +))
            runs.styledVeiled(veil: veil)
                .textRenderer(InlineCodeRenderer())
                .lineSpacing(MD.lineHeight - MD.textSize - 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .heading(let level, let runs):
            let m = MD.headingMetrics(level)
            let _ = veil.noteLength(runs.map(\.text.count).reduce(0, +))
            runs.styledVeiled(size: m.size, weight: .semibold, veil: veil)
                .textRenderer(InlineCodeRenderer())
                .lineSpacing(m.line - m.size - 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        default:
            MarkdownBlockView(block: block, cacheKey: row.id)
        }
    }
}

// MARK: - Tool group (transcript.rs render_tool_group)

struct ToolGroupView: View {
    let tools: [ToolItem]
    let open: Bool
    let userToggled: Bool
    let toggle: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Header stays quiet even on failure — chips carry the red.
            Button(action: toggle) {
                HStack(spacing: 8) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(Theme.textMuted)
                        .rotationEffect(.degrees(open ? 90 : 0))
                        .frame(width: 18, height: 18)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 5))
                    Text(toolGroupSummary(tools))
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                    Spacer(minLength: 0)
                }
                .frame(height: 26)
                .contentShape(Rectangle())
            }
            .buttonStyle(PressWashButtonStyle(cornerRadius: 6))

            if open {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(tools.enumerated()), id: \.offset) { _, tool in
                        ToolChipRow(tool: tool)
                    }
                }
                .padding(.top, 2)
            }
        }
    }
}

/// 38pt row containing a 30pt card (transcript.rs tool_chip).
struct ToolChipRow: View {
    let tool: ToolItem

    var body: some View {
        HStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: tool.call.chipSymbol)
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.textMuted)
                    .frame(width: 18, height: 18)
                    .background(whiteAlpha(0.08), in: RoundedRectangle(cornerRadius: 5))
                Text(tool.call.chipLabel)
                    .font(Theme.sans(12, weight: .medium))
                    .foregroundStyle(tool.isError ? Theme.danger : Theme.textMuted)
                Text(tool.call.chipDetail)
                    .font(Theme.sans(12))
                    .foregroundStyle(tool.isError ? Theme.danger : Theme.text.opacity(0.85))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: 30)
            .background(whiteAlpha(0.03), in: RoundedRectangle(cornerRadius: 9))
            .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
            .padding(.leading, 12)
        }
        .frame(height: 38)
    }
}

// MARK: - Chips (transcript.rs ErrorChip / InputChip)

struct ErrorChipView: View {
    let message: String

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 10))
                .foregroundStyle(Theme.dangerSoft.opacity(0.8))
                .frame(width: 20, height: 20)
                .background(Theme.danger.opacity(0.12), in: RoundedRectangle(cornerRadius: 6))
            Text("Error")
                .font(Theme.sans(12, weight: .medium))
                .foregroundStyle(Theme.text)
            Text(message)
                .font(Theme.sans(12))
                .foregroundStyle(Theme.text.opacity(0.8))
                .lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .frame(height: 34)
        .background(Theme.danger.opacity(0.05), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.danger.opacity(0.16), lineWidth: 1))
    }
}

struct InputChipView: View {
    let header: String
    let resolved: Bool

    var body: some View {
        // Neutral throughout — resolution never recolors.
        HStack(spacing: 8) {
            Image(systemName: "bubble.left.and.text.bubble.right")
                .font(.system(size: 10))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 20, height: 20)
                .background(whiteAlpha(0.09), in: RoundedRectangle(cornerRadius: 6))
            Text("Question")
                .font(Theme.sans(12, weight: .medium))
                .foregroundStyle(Theme.text)
            Text(resolved ? header : "Awaiting your answer…")
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textMuted)
                .lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .frame(height: 34)
        .background(whiteAlpha(0.045), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(whiteAlpha(0.08), lineWidth: 1))
    }
}
