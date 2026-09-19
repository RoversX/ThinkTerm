import SwiftUI

/// One host: its terminal, the tab strip, the bars over the panes, the
/// key bar, and the sidebar and menus as sheets.
struct TerminalScreen: View {
    @StateObject private var model: TerminalModel
    @Environment(\.scenePhase) private var scenePhase
    @State private var showSidebar = false
    @State private var showTabs = false
    @State private var showLog = false
    @State private var menu: MenuSheet?

    init(host: Host?, store: HostStore?) {
        _model = StateObject(wrappedValue: TerminalModel(host: host, store: store))
    }

    var body: some View {
        VStack(spacing: 0) {
            tabStrip
            terminal
            statusLine
            KeyBar(model: model)
        }
        .background(Color.black)
        .ignoresSafeArea(.container, edges: .bottom)
        .navigationTitle(model.title.isEmpty ? (model.host?.display ?? "Terminal") : model.title)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                Button { showSidebar = true } label: { Image(systemName: "sidebar.left") }
            }
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button { model.stepFont(-1) } label: { Image(systemName: "textformat.size.smaller") }
                Button { model.stepFont(1) } label: { Image(systemName: "textformat.size.larger") }
                Menu {
                    Button("New tab") { model.chromeClick("new-tab") }
                    Button("Split right") { model.chromeClick("split-right") }
                    Button("Split below") { model.chromeClick("split-below") }
                    Button("Zoom pane") { model.chromeClick("zoom") }
                    Button("Close pane", role: .destructive) { model.chromeClick("close") }
                    Divider()
                    Toggle("Smooth scrolling", isOn: $model.smoothScroll)
                    Button("Paste") { model.pasteFromClipboard() }
                    Button(model.connection.hasPrefix("disconnected") || model.connection.hasPrefix("failed") ? "Reconnect" : "Disconnect") {
                        if model.connection.hasPrefix("disconnected") || model.connection.hasPrefix("failed") {
                            model.connect()
                        } else {
                            model.disconnect()
                        }
                    }
                    Toggle("Show log", isOn: $showLog)
                } label: { Image(systemName: "ellipsis.circle") }
            }
        }
        .sheet(isPresented: $showSidebar) {
            SidebarSheet(model: model, isPresented: $showSidebar)
        }
        .sheet(item: $menu) { sheet in
            MenuList(model: model, sheet: sheet) { menu = nil }
                .presentationDetents([.medium, .large])
        }
        .onAppear {
            // The automated flows connect on their own schedule.
            if !ProcessInfo.processInfo.arguments.contains(where: { $0.hasSuffix("test") || $0 == "--autoconnect" }) {
                model.connect()
            }
        }
        .onDisappear {
            model.shutdown()
        }
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background: model.detachForBackground()
            case .active: model.reattachAfterBackground()
            default: break
            }
        }
    }

    // MARK: the terminal with its overlays

    private var terminal: some View {
        ZStack(alignment: .topLeading) {
            MetalView()
            TerminalInput { point in
                let pane = paneUnder(point)
                menu = MenuSheet(kind: "pane", id: String(pane ?? 0), title: "Pane")
            }
            navBars
            if let composing = model.composing {
                Text(composing)
                    .font(.system(size: 14, design: .monospaced))
                    .padding(4)
                    .background(Color.yellow.opacity(0.9))
                    .foregroundColor(.black)
                    .padding(8)
            }
            if let card = model.status?.card {
                takeOverCard(card)
            }
            if showLog {
                logView
            }
        }
        .environmentObject(model)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(model.inset)
        .background(Color.black)
    }

    /// The App leaves rows above each pane for its bar; these draw it,
    /// where the App says (points, from the terminal's origin).
    private var navBars: some View {
        // A GeometryReader takes the terminal's box and never grows with
        // its children: a bar the App sizes to a wider tab (the server's,
        // until this phone's claim lands) must not widen the surface, or
        // the surface, the grid and the bars chase each other forever.
        GeometryReader { geo in
            ForEach(model.navs) { nav in
                navBar(nav, maxWidth: max(geo.size.width - nav.rect.left, 0))
            }
        }
    }

    private func navBar(_ nav: NavView, maxWidth: CGFloat) -> some View {
        HStack(spacing: 6) {
            ForEach(nav.members) { member in
                Button {
                    model.chromeClick("pane", pane: member.pane)
                } label: {
                    HStack(spacing: 4) {
                        if member.busy {
                            ProgressView().controlSize(.mini)
                        }
                        Text(member.title.isEmpty ? "shell" : member.title)
                            .lineLimit(1)
                            .font(.system(size: 11, weight: member.current ? .semibold : .regular))
                    }
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(member.current ? Color.white.opacity(nav.focused ? 0.22 : 0.12) : Color.clear)
                    .clipShape(Capsule())
                }
                .buttonStyle(.plain)
            }
            Spacer(minLength: 0)
            Button { model.chromeClick("new-pane", pane: nav.rect.pane) } label: {
                Image(systemName: "plus").font(.system(size: 11))
            }
            .buttonStyle(.plain)
            Button { model.chromeClick("close-pane", pane: nav.rect.pane) } label: {
                Image(systemName: "xmark").font(.system(size: 11))
            }
            .buttonStyle(.plain)
        }
        .foregroundColor(nav.focused ? .white : .gray)
        .padding(.horizontal, 6)
        .frame(width: min(nav.rect.width, maxWidth), height: nav.rect.height, alignment: .leading)
        .background(Color(white: nav.focused ? 0.16 : 0.1))
        .contentShape(Rectangle())
        .onLongPressGesture {
            menu = MenuSheet(kind: "pane", id: String(nav.rect.pane), title: "Pane")
        }
        .offset(x: nav.rect.left, y: nav.rect.top)
    }

    private func paneUnder(_ point: CGPoint) -> Int? {
        // The bar rects tile the panes' tops; the pane whose bar's column
        // holds the point is the one under it, near enough for a menu.
        model.navs.first { nav in
            point.x >= nav.rect.left && point.x < nav.rect.left + nav.rect.width && point.y >= nav.rect.top
        }?.rect.pane ?? model.navs.first?.rect.pane
    }

    private func takeOverCard(_ card: Card) -> some View {
        VStack(spacing: 8) {
            Text(card.title).font(.headline)
            Text(card.hint).font(.caption).multilineTextAlignment(.center)
            Button(card.action) { model.takeOver() }
                .buttonStyle(.borderedProminent)
                .disabled(card.state == "taking")
        }
        .padding(16)
        .frame(maxWidth: 320)
        .background(.ultraThinMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var logView: some View {
        ScrollView {
            Text(model.logText + "\n" + model.stats)
                .font(.system(size: 9, design: .monospaced))
                .foregroundColor(.white)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(6)
        }
        .frame(maxHeight: 220)
        .background(Color.black.opacity(0.8))
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
    }

    // MARK: the tab strip

    private var tabStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 4) {
                ForEach(model.tabs?.tabs ?? []) { tab in
                    Button {
                        model.chromeClick("pane", pane: tab.target)
                    } label: {
                        Text(tab.label.isEmpty ? "Tab \(tab.tab)" : tab.label)
                            .font(.system(size: 12, weight: tab.current ? .semibold : .regular))
                            .lineLimit(1)
                            .padding(.horizontal, 10)
                            .padding(.vertical, 5)
                            .background(tab.current ? Color.white.opacity(0.18) : Color.white.opacity(0.06))
                            .clipShape(Capsule())
                    }
                    .buttonStyle(.plain)
                    .contextMenu {
                        Button("Close tab", role: .destructive) { model.chromeClick("close-tab", tab: tab.tab) }
                        Button("More…") { menu = MenuSheet(kind: "tab", id: String(tab.tab), title: "Tab") }
                    }
                }
                Button { model.chromeClick("new-tab") } label: {
                    Image(systemName: "plus").padding(6)
                }
                .buttonStyle(.plain)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
        }
        .foregroundColor(.white)
        .background(Color(white: 0.12))
        .frame(height: 36)
    }

    // MARK: the status line

    @ViewBuilder
    private var statusLine: some View {
        let text = model.status?.toast?.text ?? ""
        let showing = !text.isEmpty || model.connection.hasPrefix("connecting") || model.connection.hasPrefix("disconnected") || model.connection.hasPrefix("failed") || model.connection.contains("reconnect")
        if showing {
            Text(text.isEmpty ? model.connection : text)
                .font(.system(size: 11, design: .monospaced))
                .lineLimit(2)
                .foregroundColor(.white)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .background(Color(white: 0.2))
        }
    }
}

/// What a menu is about, for the sheet.
struct MenuSheet: Identifiable {
    var kind: String
    var id: String
    var title: String
    var key: String { kind + ":" + id }
}

struct MenuList: View {
    @ObservedObject var model: TerminalModel
    var sheet: MenuSheet
    var dismiss: () -> Void
    @State private var items: [MenuItem] = []

    var body: some View {
        NavigationStack {
            List {
                ForEach(items, id: \.rowId) { item in
                    row(item)
                }
            }
            .navigationTitle(sheet.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done", action: dismiss) }
            }
        }
        .onAppear { items = model.contextMenu(sheet.kind, id: sheet.id) }
    }

    /// Rows nest (a submenu is a row that pushes more rows), so the view
    /// is erased rather than recursively opaque.
    private func row(_ item: MenuItem) -> AnyView {
        switch item.kind {
        case "separator":
            return AnyView(Divider())
        case "header":
            return AnyView(Text(item.label).font(.caption).foregroundColor(.secondary))
        default:
            if !item.submenu.isEmpty {
                let subs = item.submenu
                return AnyView(
                    NavigationLink(item.label) {
                        List {
                            ForEach(subs, id: \.rowId) { sub in row(sub) }
                        }
                        .navigationTitle(item.label)
                    }
                )
            }
            return AnyView(
                Button {
                    model.menuAction(item.id)
                    dismiss()
                } label: {
                    HStack {
                        Text(item.label)
                        Spacer()
                        if item.checked { Image(systemName: "checkmark") }
                    }
                }
                .disabled(!item.enabled)
            )
        }
    }
}

/// The keys a soft keyboard has not got, with sticky Ctrl and Alt.
struct KeyBar: View {
    @ObservedObject var model: TerminalModel

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                key("esc") { model.key("Escape") }
                key("tab") { model.key("Tab") }
                toggle("ctrl", $model.ctrlSticky)
                toggle("alt", $model.altSticky)
                key("↑") { model.key("ArrowUp") }
                key("↓") { model.key("ArrowDown") }
                key("←") { model.key("ArrowLeft") }
                key("→") { model.key("ArrowRight") }
                key("home") { model.key("Home") }
                key("end") { model.key("End") }
                key("pgup") { model.key("PageUp") }
                key("pgdn") { model.key("PageDown") }
                key("-") { model.text("-") }
                key("/") { model.text("/") }
                key("|") { model.text("|") }
                key("~") { model.text("~") }
                key("^C") { model.key("c", ctrl: true) }
                key("^D") { model.key("d", ctrl: true) }
                key("^L") { model.key("l", ctrl: true) }
                key("^Z") { model.key("z", ctrl: true) }
                key("⌫") { model.key("Backspace") }
            }
            .padding(.horizontal, 6)
        }
        .frame(height: 38)
        .background(Color(white: 0.12))
    }

    private func key(_ label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(label)
                .font(.system(size: 13, design: .monospaced))
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(Color.white.opacity(0.08))
                .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .foregroundColor(.white)
    }

    private func toggle(_ label: String, _ on: Binding<Bool>) -> some View {
        Button { on.wrappedValue.toggle() } label: {
            Text(label)
                .font(.system(size: 13, design: .monospaced))
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(on.wrappedValue ? Color.accentColor : Color.white.opacity(0.08))
                .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .foregroundColor(.white)
    }
}
