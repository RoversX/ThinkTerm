import SwiftUI

/// The desktop's right sidebar on a phone: the left drawer's mirror, under
/// the screen at its right edge, which slides off it by the button at the
/// bar's right end or a swipe from the right edge and comes back with a
/// tap on what shows of it or a swipe back. Two of the desktop's panels to
/// begin with: the agents and what each is doing, and the snippets.
struct RightPanel: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    @ObservedObject private var snippets = SnippetStore.shared
    var done: () -> Void
    @State private var face: Face = .agents
    @State private var room = EdgeInsets()

    enum Face: String, CaseIterable, Identifiable {
        case agents, snippets
        var id: String { rawValue }
        var label: String { tr(self == .agents ? "panel.agents" : "p.snippets") }
    }

    private static let headerHeight: CGFloat = 60

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2) {
                switch face {
                case .agents: agents
                case .snippets: snippetRows
                }
            }
            .padding(.horizontal, 10)
            .padding(.bottom, 24)
        }
        .foregroundColor(.primary)
        .background(Color.chromeBackdrop.ignoresSafeArea())
        .floatingBar(edge: .top, fading: Color.chromeBackdrop, scrim: true) { faces }
        .barRoom(.top, height: Self.headerHeight, $room)
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
    }

    /// Which panel: one piece of glass, the one on show lit.
    private var faces: some View {
        HStack(spacing: 4) {
            ForEach(Face.allCases) { item in
                Button { face = item } label: {
                    Text(item.label)
                        .font(.subheadline.weight(face == item ? .semibold : .regular))
                        .foregroundColor(face == item ? .primary : .secondary)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity)
                        .frame(height: 38)
                        .background(face == item ? Color.primary.opacity(0.14) : Color.clear, in: Capsule())
                        .contentShape(Capsule())
                }
                .buttonStyle(.plain)
                .accessibilityAddTraits(face == item ? .isSelected : [])
            }
        }
        .padding(4)
        .glassCapsule()
        .padding(.horizontal, 14)
        .padding(.top, 6 + max(0, room.top))
        .padding(.bottom, 10)
    }

    // MARK: agents

    /// The states in the order a person wants them: what waits on them
    /// first, then what went wrong, what runs, what is done, what rests.
    private static let order = ["blocked", "error", "working", "done", "idle", "unknown"]

    @ViewBuilder
    private var agents: some View {
        let rows = model.agents?.rows ?? []
        if rows.isEmpty {
            Text(model.agents?.summary ?? "")
                .font(.subheadline)
                .foregroundColor(.secondary)
                .frame(maxWidth: .infinity)
                .padding(.top, 40)
        } else {
            ForEach(Self.order, id: \.self) { state in
                let group = rows.filter { $0.state == state }
                if let first = group.first {
                    Text(first.state_label)
                        .font(.footnote)
                        .foregroundColor(.secondary)
                        .padding(.horizontal, 12)
                        .padding(.top, 14)
                        .padding(.bottom, 2)
                    ForEach(group) { row in agentRow(row) }
                }
            }
        }
    }

    private func agentRow(_ row: AgentRow) -> some View {
        // What it said about itself, else what it waits for, else its title.
        let detail = [row.said, row.kind, row.title].compactMap { $0 }.first { !$0.isEmpty }
        let more = row.progress != nil || !row.subtasks_summary.isEmpty
        return SidebarRow(
            lead: AnyView(ThreadCards(core: model.core, icons: [row.pane_card ?? model.tabs?.icon].compactMap { $0 }, revision: model.iconsRevision)),
            mark: AnyView(ThreadMark(status: Self.status(row.state), dot: "Open", size: 9)),
            title: row.place.isEmpty ? row.name : row.place,
            subtitle: detail,
            below: more ? AnyView(agentMore(row)) : nil
        ) {
            model.chromeClick("pane", pane: row.pane)
            done()
        }
        // The row is one element to VoiceOver: the fold is one of its
        // actions there.
        .accessibilityActions {
            if !row.subtasks_summary.isEmpty {
                Button(row.subtasks_summary) { model.toggleSubtasks(pane: row.pane) }
            }
        }
    }

    /// What an agent said of its progress under its row, as the web's
    /// panel shows it: how far it got, and its sub-tasks folded into one
    /// line that a tap opens into the list.
    @ViewBuilder
    private func agentMore(_ row: AgentRow) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            if let progress = row.progress {
                ProgressView(value: Double(min(max(progress, 0), 100)), total: 100)
                    .progressViewStyle(.linear)
                    .tint(.accentColor)
                    .padding(.vertical, 4)
            }
            if !row.subtasks_summary.isEmpty {
                // A target of its own inside the row: it opens or folds the
                // list rather than showing the pane.
                Button {
                    model.toggleSubtasks(pane: row.pane)
                } label: {
                    HStack(spacing: 6) {
                        DesktopIcon(name: "chevron-right", size: 12)
                            .foregroundColor(.secondary)
                            .rotationEffect(.degrees(row.subtasks_open ? 90 : 0))
                        Text(row.subtasks_summary)
                            .font(.footnote)
                            .foregroundColor(.secondary)
                            .lineLimit(1)
                        Spacer(minLength: 0)
                    }
                    .frame(minHeight: 32)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                if row.subtasks_open {
                    VStack(alignment: .leading, spacing: 6) {
                        ForEach(Array(row.subtasks.enumerated()), id: \.offset) { _, sub in
                            HStack(spacing: 6) {
                                ThreadMark(status: Self.status(sub.state), dot: "Open", size: 7)
                                Text(sub.label)
                                    .font(.footnote)
                                    .foregroundColor(.secondary)
                                    .lineLimit(1)
                            }
                        }
                        if row.more_subtasks > 0 {
                            Text(tr("agents.more", row.more_subtasks))
                                .font(.footnote)
                                .foregroundColor(.secondary)
                        }
                    }
                    .padding(.leading, 18)
                    .padding(.bottom, 8)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.bottom, row.subtasks_open ? 0 : 4)
    }

    /// An agent's state as the thread's whose mark stands for it.
    private static func status(_ state: String) -> String {
        switch state {
        case "working": return "Running"
        case "blocked": return "NeedsAttention"
        // Stopped on an error: the desktop's red cross, not a wait's amber.
        case "error": return "Failed"
        case "done": return "Done"
        default: return "Idle"
        }
    }

    // MARK: snippets

    @ViewBuilder
    private var snippetRows: some View {
        ForEach(snippets.items) { snippet in
            Button {
                model.text(snippet.text)
                if snippet.runs { model.key("Enter") }
                KeyHistory.shared.record(snippet.text)
                KeyCaps.tap()
                done()
            } label: {
                HStack(spacing: 10) {
                    Text(snippet.text)
                        .font(.system(size: 14, design: .monospaced))
                        .lineLimit(2)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if snippet.runs {
                        Text(tr("p.run")).font(.footnote).foregroundColor(.accentColor)
                    }
                }
                .padding(.horizontal, 12)
                .frame(minHeight: 44)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
        }
    }
}
