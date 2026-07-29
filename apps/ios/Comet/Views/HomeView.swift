// Home — the mobile shell. The desktop sidebar's two sections become the
// phone's home screen: Spaces (grouped work) and Sessions (the global
// attention-sorted list). Tabs-as-sessions don't fit a phone; a space opens
// into its own session list instead, and close=archive becomes swipe-to-archive.

import SwiftUI

enum Route: Hashable {
    case space(String)
    case chat(String)
    case newSession(spaceId: String)
}

struct HomeView: View {
    @Environment(AppModel.self) private var model
    @State private var path: [Route] = []
    @State private var showNewSpace = false

    var body: some View {
        NavigationStack(path: $path) {
            List {
                spacesSection
                sessionsSection
            }
            .listStyle(.plain)
            .environment(\.defaultMinListRowHeight, 10)
            .contentMargins(.top, 2, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollEdgeEffectStyle(.soft, for: .top)
            .background(Theme.surface.ignoresSafeArea())
            .navigationTitle("Komet")  // feeds the back menu; not displayed
            .navigationBarTitleDisplayMode(.inline)
            .toolbar(removing: .title)
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .space(let id): SpaceView(spaceId: id, path: $path)
                case .chat(let id): SessionView(chatId: id)
                case .newSession(let spaceId): NewSessionView(spaceId: spaceId, path: $path)
                }
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    // In the bar, not the list: as a list row it appeared and
                    // vanished with the connection and shoved the content down.
                    if !model.connected {
                        ProgressView()
                            .controlSize(.mini)
                            .tint(Theme.textMuted)
                            .accessibilityLabel("Connecting")
                    }
                }
                // Bare spinner — no glass capsule behind it.
                .sharedBackgroundVisibility(.hidden)
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        showNewSpace = true
                    } label: {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("New space")
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        if model.demo != nil {
                            Text("Demo mode")
                        }
                        Button("Sign out", role: .destructive) { model.signOut() }
                    } label: {
                        Image(systemName: "person.circle")
                    }
                }
            }
            .sheet(isPresented: $showNewSpace) {
                NewSpaceSheet { spaceId in
                    path.append(.space(spaceId))
                }
            }
            .task(id: model.overviewChats.map(\.id).joined()) {
                model.preloadSessions()
            }
            .onAppear {
                if let route = model.launchRoute {
                    model.launchRoute = nil
                    // Push the whole stack atomically — appending from a child's
                    // onAppear mid-transition gets dropped by NavigationStack.
                    if case .space(let id) = route, model.launchSheet == "newsession" {
                        model.launchSheet = nil
                        path = [route, .newSession(spaceId: id)]
                    } else {
                        path = [route]
                    }
                }
                if model.launchSheet == "newspace" {
                    model.launchSheet = nil
                    showNewSpace = true
                }
            }
        }
    }

    // MARK: Spaces

    private var spacesSection: some View {
        Section {
            if model.spaces.isEmpty {
                Text("No spaces yet — add one from a desktop device")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(model.spaces) { space in
                Button {
                    path.append(.space(space.id))
                } label: {
                    SpaceRow(space: space)
                }
                .buttonStyle(PressWashButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 2, trailing: 12))
            }
        } header: {
            sectionHeader("Spaces")
        }
    }

    // MARK: Sessions

    private var sessionsSection: some View {
        Section {
            let chats = model.overviewChats
            if chats.isEmpty {
                Text("No sessions yet")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(chats) { chat in
                Button {
                    path.append(.chat(chat.id))
                } label: {
                    ChatRow(chat: chat, showLocation: true)
                }
                .buttonStyle(PressWashButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        model.archive(chatId: chat.id)
                    } label: {
                        Label("Archive", systemImage: "archivebox")
                    }
                    .tint(Theme.surfaceRaised)
                }
            }
            .motionAnimation(Motion.resort, value: chats.map(\.id))
        } header: {
            sectionHeader("Sessions")
        }
    }

    private func sectionHeader(_ title: String) -> some View {
        Text(title)
            .font(Theme.sans(11, weight: .medium))
            .foregroundStyle(Theme.textMuted.opacity(0.6))
            .textCase(nil)
            .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

// MARK: - Rows

struct SpaceRow: View {
    @Environment(AppModel.self) private var model
    let space: Space

    var body: some View {
        HStack(spacing: 8) {
            // Leading 6pt aggregate dot — position stable, most-urgent member.
            let agg = model.spaceIndicator(space.id)
            Circle()
                .fill((agg == .working || agg == .awaitingInput) ? (agg?.dotColor ?? whiteAlpha(0.14)) : whiteAlpha(0.14))
                .frame(width: 6, height: 6)
            Image(systemName: "folder")
                .font(.system(size: 13))
                .foregroundStyle(Theme.textMuted)
            Text(space.displayName)
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
            Spacer(minLength: 8)
            deviceTag
            Image(systemName: "chevron.right")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(Theme.textFaint.opacity(0.6))
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 8)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }

    private var deviceTag: some View {
        let online = model.deviceOnline(space.deviceId)
        let name = model.deviceName(space.deviceId)
        return Text(online ? "@ \(name)" : "@ \(name) · offline")
            .font(Theme.sans(12))
            .foregroundStyle(online ? Theme.textMuted.opacity(0.6) : Theme.warning.opacity(0.8))
            .lineLimit(1)
    }
}

/// The desktop session row (shell.rs `render_chat_row`), line for line: the
/// status rail leads a muted context line carrying the space name and the
/// relative time; the title sits on its own line below; harness mark and branch
/// close it out. Lines 2 and 3 indent by rail + gap so they start exactly under
/// the context line rather than beside the rail.
///
/// The one addition the phone needs: the desktop row names only the space
/// because its sidebar sits on the machine running the work. Here the Sessions
/// list interleaves every device, and a session whose host has gone offline
/// can't be driven at all — so the context line reads "space · device".
struct ChatRow: View {
    @Environment(AppModel.self) private var model
    let chat: Chat
    var showLocation: Bool

    /// Rail (6) + gap (8) — see `render_chat_row`'s `pl(px(14.0))`.
    private static let indent: CGFloat = StatusRail.width + 8

    private var subline: Color { Theme.textMuted.opacity(0.5) }

    var body: some View {
        let indicator = model.indicator(for: chat)
        VStack(alignment: .leading, spacing: 2) {
            // Line 1: status rail, space · device, time-ago.
            HStack(spacing: 8) {
                StatusRail(indicator: indicator)
                if showLocation {
                    Text(location)
                        .font(Theme.sans(11))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Spacer(minLength: 4)
                }
                Text(relativeTime(chat.lastMessageAt ?? chat.createdAt))
                    .font(Theme.sans(11))
                    .foregroundStyle(subline)
                    .fixedSize()
            }

            // Line 2: the session title.
            Text(chat.displayTitle)
                .font(Theme.sans(13))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.leading, Self.indent)

            // Line 3: harness brand mark, then the branch when the engine
            // stamped one.
            HStack(spacing: 4) {
                if let harness = chat.config?.harness {
                    HarnessBadge(harness: harness, size: 11, neutral: subline)
                }
                if let branch = chat.branch?.trimmingCharacters(in: .whitespaces), !branch.isEmpty {
                    LineIconView(.gitBranch, size: 11, color: subline)
                    Text(branch)
                        .font(Theme.sans(11))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Spacer(minLength: 0)
            }
            .padding(.leading, Self.indent)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }

    /// "space · device", with offline marker. The space name (not the cwd
    /// basename) is what the desktop row shows — they differ once a space has
    /// been renamed, or when the session runs in a worktree off to the side.
    private var location: String {
        let space = model.space(for: chat)?.displayName
            ?? chat.cwd.map { ($0 as NSString).lastPathComponent }
            ?? "?"
        let name = model.deviceName(chat.deviceId)
        return model.deviceOnline(chat.deviceId)
            ? "\(space) · \(name)"
            : "\(space) · \(name) (offline)"
    }
}

func relativeTime(_ ms: Int64) -> String {
    let delta = max(0, nowMs() - ms) / 1000
    if delta < 60 { return "now" }
    if delta < 3600 { return "\(delta / 60)m" }
    if delta < 86_400 { return "\(delta / 3600)h" }
    return "\(delta / 86_400)d"
}
