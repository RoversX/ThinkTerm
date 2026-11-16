import SwiftUI

/// The panes of the tab on show, in one piece of glass under the terminal:
/// a phone held upright shows one pane at a time, whole (`Core.setOnePane`),
/// and this is where the others are, each by its card alone, the one on
/// show on a disc; a tap shows another, and the `+` at the end --
/// or a long press -- brings up the split as a map (`PaneMap`). A lone
/// pane has nothing to switch to, and no switcher.
struct PaneSwitcher: View {
    @ObservedObject var model: TerminalModel
    var onMap: () -> Void

    /// Every pane of the tab, a stack's one after another, with whether it
    /// is the one on show.
    private var entries: [(pane: CapsuleView, shown: Bool)] {
        (model.panes?.panes ?? []).flatMap { box in
            box.members.map { ($0, box.focused && $0.current) }
        }
    }

    var body: some View {
        let entries = self.entries
        if entries.count > 1 {
            // As wide as its panes, and scrolling once they outgrow the
            // screen.
            ViewThatFits(in: .horizontal) {
                row(entries)
                ScrollView(.horizontal, showsIndicators: false) { row(entries) }
            }
            .frame(height: Self.height)
            .clipShape(Capsule())
            .glassCapsule()
            .padding(.horizontal, 10)
            .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        }
    }

    static let height: CGFloat = 36

    private func row(_ entries: [(pane: CapsuleView, shown: Bool)]) -> some View {
        HStack(spacing: 2) {
            ForEach(entries, id: \.pane.pane) { entry in
                Button { model.chromeClick("pane", pane: entry.pane.pane) } label: {
                    TabIconCard(
                        core: model.core, id: entry.pane.icon ?? model.tabs?.icon, size: 22,
                        busy: entry.pane.busy, revision: model.iconsRevision
                    )
                    .frame(width: 30, height: 30)
                    .background(entry.shown ? model.ink.opacity(0.16) : Color.clear, in: Circle())
                    .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(LongPressGesture(minimumDuration: 0.4).onEnded { _ in onMap() })
                .accessibilityLabel(entry.pane.title.isEmpty ? tr("pane.title") : entry.pane.title)
                .accessibilityAddTraits(entry.shown ? .isSelected : [])
            }
            Button(action: onMap) {
                Image(systemName: "plus")
                    .font(.system(size: 14, weight: .medium))
                    .frame(width: 30, height: 30)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(tr("panes.title"))
        }
        .foregroundColor(model.ink)
        .padding(.horizontal, 3)
    }
}

/// The split of the tab on show as a map over the terminal: each pane
/// where the split puts it, the one on show ringed. A tap on a pane shows
/// it; the buttons under the map split the pane on show to its right or
/// below; a long press on a pane closes it. It goes with the choice.
struct PaneMap: View {
    @ObservedObject var model: TerminalModel
    var onDone: () -> Void

    var body: some View {
        ZStack {
            // What is behind takes it away.
            Color.black.opacity(0.4)
                .ignoresSafeArea()
                .onTapGesture(perform: onDone)
            VStack(spacing: 14) {
                Text(tr("panes.title")).font(.headline)
                if let panes = model.panes, panes.cols > 0, panes.rows > 0 {
                    map(panes)
                }
                HStack(spacing: 10) {
                    split("pane.split.right", symbol: "rectangle.righthalf.inset.filled", action: "split-right")
                    split("pane.split.below", symbol: "rectangle.bottomhalf.inset.filled", action: "split-below")
                }
            }
            .padding(16)
            .frame(maxWidth: 340)
            .glassPanel(radius: 28)
            .padding(.horizontal, 24)
            .foregroundColor(model.ink)
            .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        }
        .transition(.opacity)
    }

    /// The split in the tab's own shape (a cell is about twice as high as
    /// it is wide), kept between a square and a strip: a phone's tab is a
    /// tower, and a map of one would cover the terminal it is over.
    private func map(_ panes: PanesView) -> some View {
        let shape = min(max(CGFloat(panes.cols) / (CGFloat(panes.rows) * 2), 1.3), 2.2)
        return GeometryReader { geo in
            let unit = CGSize(width: geo.size.width / CGFloat(panes.cols), height: geo.size.height / CGFloat(panes.rows))
            ForEach(panes.panes) { box in
                let frame = CGRect(
                    x: CGFloat(box.left) * unit.width, y: CGFloat(box.top) * unit.height,
                    width: CGFloat(box.cols) * unit.width, height: CGFloat(box.rows) * unit.height
                ).insetBy(dx: 3, dy: 3)
                cell(box)
                    .frame(width: max(frame.width, 1), height: max(frame.height, 1))
                    .position(x: frame.midX, y: frame.midY)
            }
        }
        .aspectRatio(shape, contentMode: .fit)
    }

    private func cell(_ box: PaneBox) -> some View {
        let front = box.members.first(where: \.current) ?? box.members.first
        return Button {
            model.chromeClick("pane", pane: box.pane)
            onDone()
        } label: {
            VStack(spacing: 6) {
                TabIconCard(
                    core: model.core, id: front?.icon ?? model.tabs?.icon, size: 32,
                    busy: front?.busy ?? false, revision: model.iconsRevision
                )
                Text(front.map { $0.title.isEmpty ? "shell" : $0.title } ?? "")
                    .font(.caption)
                    .lineLimit(1)
                    .foregroundColor(box.focused ? model.ink : model.ink.opacity(0.6))
                    .padding(.horizontal, 4)
                if box.members.count > 1 {
                    // A stack: how many more are behind the one in front.
                    Text(verbatim: "+\(box.members.count - 1)")
                        .font(.caption2.monospacedDigit())
                        .foregroundColor(model.ink.opacity(0.6))
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color.black.opacity(0.28), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 14, style: .continuous)
                    .strokeBorder(box.focused ? Color.accentColor : model.ink.opacity(0.12), lineWidth: box.focused ? 2 : 1)
            )
            .contentShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        }
        .buttonStyle(.plain)
        .contextMenu {
            Button(role: .destructive) {
                model.chromeClick("close-pane", pane: box.pane)
                onDone()
            } label: {
                Label(tr("pane.close"), systemImage: "xmark")
            }
        }
        .accessibilityLabel(front?.title ?? tr("pane.title"))
        .accessibilityAddTraits(box.focused ? .isSelected : [])
    }

    private func split(_ key: String, symbol: String, action: String) -> some View {
        Button {
            model.chromeClick(action)
            onDone()
        } label: {
            Label(tr(key), systemImage: symbol)
                .font(.subheadline.weight(.medium))
                .lineLimit(1)
                .minimumScaleFactor(0.8)
                .frame(maxWidth: .infinity)
                .frame(height: 42)
                .background(model.ink.opacity(0.1), in: Capsule())
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
    }
}
