import SwiftUI

/// One host: its terminal, the tab strip, the bars over the panes, the
/// key bar, and the sidebar and menus as sheets.
struct TerminalScreen: View {
    @StateObject private var model: TerminalModel
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dismiss) private var dismiss
    @State private var showSidebar = false
    @State private var showSessions = false
    @State private var showLog = false
    @State private var showSettings = false
    @State private var menu: MenuSheet?
    @State private var editingHost: Host?
    /// A bar being dragged moves the divider it hangs from.
    @State private var barDrag: CGPoint?

    init(host: Host?, store: HostStore?) {
        _model = StateObject(wrappedValue: TerminalModel(host: host, store: store))
    }

    /// The tabs live in the navigation bar, between the system's own back
    /// button and the menu; the keys sit below the terminal, and nothing
    /// else. Every piece of chrome takes the terminal's own background.
    var body: some View {
        VStack(spacing: 0) {
            terminal
            statusLine
            KeyBar(model: model)
        }
        .background(model.background)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.hidden, for: .tabBar)
        .toolbarBackground(model.background, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .principal) { tabStrip }
            ToolbarItem(placement: .topBarTrailing) { moreMenu }
        }
        .ignoresSafeArea(.container, edges: .bottom)
        .sheet(isPresented: $showSidebar) {
            SidebarSheet(model: model, isPresented: $showSidebar)
        }
        .sheet(isPresented: $showSessions) {
            SessionsSheet(model: model, isPresented: $showSessions, menu: $menu, showSidebar: $showSidebar)
                .presentationDetents([.medium, .large])
        }
        .sheet(isPresented: $showSettings) {
            NavigationStack {
                SettingsView(model: model, showLog: $showLog)
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) { Button("Done") { showSettings = false } }
                    }
            }
        }
        .onChange(of: showLog) { _, on in model.wantsStats = on }
        .sheet(item: $menu) { sheet in
            MenuList(model: model, sheet: sheet) { menu = nil }
                .presentationDetents([.medium, .large])
        }
        .sheet(item: $editingHost) { host in
            HostEditView(host: host) { edited, secret, passphrase in
                if !secret.isEmpty { Keychain.save(secret, account: edited.id.uuidString) }
                if let passphrase { Keychain.save(passphrase, account: edited.id.uuidString + ".passphrase") }
                model.store?.upsert(edited)
                model.host = edited
                model.connect()
            }
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

    // MARK: the bar under the terminal

    private var connected: Bool {
        !(model.connection.hasPrefix("disconnected") || model.connection.hasPrefix("failed") || model.connection.isEmpty)
    }

    private var hostName: String {
        model.host?.display ?? "This Mac"
    }

    private var statusColor: Color {
        if model.reconnecting { return .orange }
        if connected { return .green }
        if model.connection.hasPrefix("connecting") || model.connection.contains("reconnect") { return .orange }
        return .red
    }

    /// The connection's state and the tabs, in the bar's middle.
    private var tabStrip: some View {
        HStack(spacing: 6) {
            Circle().fill(statusColor).frame(width: 7, height: 7)
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
                        Image(systemName: "plus").font(.system(size: 13, weight: .semibold)).padding(6)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .foregroundColor(.white)
    }

    private var moreMenu: some View {
        Menu {
            Section(hostName) {
                Button(connected ? "Disconnect" : "Reconnect") {
                    if connected { model.disconnect() } else { model.connect() }
                }
            }
            Button("Split right") { model.chromeClick("split-right") }
            Button("Split below") { model.chromeClick("split-below") }
            Button("Zoom pane") { model.chromeClick("zoom") }
            Button("Pane…") { menu = MenuSheet(kind: "pane", id: String(model.tabs?.tabs.first(where: { $0.current })?.target ?? 0), title: "Pane") }
            Divider()
            Button("Sessions…") { showSessions = true }
            Button("Threads…") { showSidebar = true }
            Button("Paste") { model.pasteFromClipboard() }
            Divider()
            Button("Settings…") { showSettings = true }
            Button("Close pane", role: .destructive) { model.chromeClick("close") }
        } label: {
            Image(systemName: "ellipsis")
        }
    }

    // MARK: the terminal with its overlays

    private var terminal: some View {
        ZStack(alignment: .topLeading) {
            MetalView()
            TerminalInput()
            if model.navs.count > 1 {
                navBars
            }
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
            if let phase = ConnectionPhase(status: model.connection) {
                ConnectionCard(phase: phase, hostName: hostName, canEdit: model.host != nil) { action in
                    switch action {
                    case .retry: model.connect()
                    case .forgetKeyAndRetry:
                        if let host = model.host {
                            model.store?.forgetHostKey(for: host.id)
                            model.host?.knownHost = nil
                        }
                        model.connect()
                    case .edit: editingHost = model.host
                    case .back: dismiss()
                    }
                }
            }
        }
        .environmentObject(model)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(model.inset)
        .background(model.background)
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
        .background(model.background)
        .background(Color.white.opacity(nav.focused ? 0.1 : 0.04))
        .contentShape(Rectangle())
        .onLongPressGesture {
            menu = MenuSheet(kind: "pane", id: String(nav.rect.pane), title: "Pane")
        }
        .gesture(barDragGesture(nav))
        .offset(x: nav.rect.left, y: nav.rect.top)
    }

    /// Dragging a bar drags the divider it sits under (or beside, for a
    /// pane split to the right): the press lands on the divider's row or
    /// column, just outside the bar, and the App does the rest.
    private func barDragGesture(_ nav: NavView) -> some Gesture {
        DragGesture(minimumDistance: 10, coordinateSpace: .local)
            .onChanged { value in
                let r = nav.rect
                let origin: CGPoint
                if r.top > 0 {
                    origin = CGPoint(x: r.left + value.startLocation.x, y: r.top - 2)
                } else if r.left > 0 {
                    origin = CGPoint(x: r.left - 2, y: r.top + value.startLocation.y)
                } else {
                    return
                }
                if barDrag == nil {
                    barDrag = origin
                    model.pointer("down", at: origin)
                }
                model.pointer("move", at: CGPoint(x: origin.x + value.translation.width, y: origin.y + value.translation.height))
            }
            .onEnded { value in
                guard let origin = barDrag else { return }
                barDrag = nil
                model.pointer("up", at: CGPoint(x: origin.x + value.translation.width, y: origin.y + value.translation.height))
            }
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

    // MARK: the status line

    @ViewBuilder
    private var statusLine: some View {
        let text = model.copied ? "Copied" : (model.toastText ?? "")
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
        .background(model.background)
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

/// The tabs and their panes, as a list: a tap shows one, a swipe closes
/// a tab. This is where the strip and the pane menu went.
struct SessionsSheet: View {
    @ObservedObject var model: TerminalModel
    @Binding var isPresented: Bool
    @Binding var menu: MenuSheet?
    @Binding var showSidebar: Bool

    var body: some View {
        NavigationStack {
            List {
                ForEach(model.tabs?.tabs ?? []) { tab in
                    Section {
                        ForEach(tab.panes, id: \.pane) { pane in
                            Button {
                                model.chromeClick("pane", pane: pane.pane)
                                isPresented = false
                            } label: {
                                HStack {
                                    Image(systemName: "terminal")
                                        .foregroundColor(pane.current && tab.current ? .accentColor : .secondary)
                                    Text(pane.title.isEmpty ? "shell" : pane.title)
                                        .fontWeight(pane.current && tab.current ? .semibold : .regular)
                                    Spacer()
                                    if pane.current && tab.current {
                                        Image(systemName: "checkmark").foregroundColor(.accentColor)
                                    }
                                }
                            }
                            .foregroundColor(.primary)
                        }
                    } header: {
                        HStack {
                            Text(tab.label.isEmpty ? "Tab \(tab.tab)" : tab.label)
                            Spacer()
                            Button("More…") {
                                menu = MenuSheet(kind: "tab", id: String(tab.tab), title: "Tab")
                                isPresented = false
                            }
                            .font(.caption)
                        }
                    }
                    .swipeActions(edge: .trailing) {
                        Button(role: .destructive) { model.chromeClick("close-tab", tab: tab.tab) } label: {
                            Label("Close tab", systemImage: "xmark")
                        }
                    }
                }
            }
            .navigationTitle("Sessions")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { isPresented = false } }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button { showSidebar = true; isPresented = false } label: { Image(systemName: "sidebar.left") }
                    Button { model.chromeClick("new-tab") } label: { Image(systemName: "plus") }
                }
            }
        }
        .onAppear { model.refreshViews() }
    }
}

/// Where a connection is, read off the core's status line.
enum ConnectionPhase {
    case connecting(String)
    case attaching
    case reconnecting
    case failed(String)
    case disconnected

    init?(status: String) {
        if status.hasPrefix("pane ") { return nil }
        if status.hasPrefix("connecting") {
            self = .connecting(String(status.dropFirst("connecting to ".count)))
        } else if status.hasPrefix("connected") {
            self = .attaching
        } else if status.contains("reconnect") {
            self = .reconnecting
        } else if status.hasPrefix("failed: ") {
            self = .failed(String(status.dropFirst("failed: ".count)))
        } else if status.hasPrefix("attach failed: ") {
            self = .failed(String(status.dropFirst("attach failed: ".count)))
        } else if status.hasPrefix("disconnected: ") {
            self = .failed(String(status.dropFirst("disconnected: ".count)))
        } else if status.hasPrefix("disconnected") {
            self = .disconnected
        } else {
            // "idle", or nothing yet: the connection is about to start.
            self = .connecting("")
        }
    }
}

/// The card over the terminal while there is no terminal to show: the
/// steps of a connection, or what stopped it and what to do about it.
struct ConnectionCard: View {
    enum Action { case retry, forgetKeyAndRetry, edit, back }

    var phase: ConnectionPhase
    var hostName: String
    var canEdit: Bool
    var act: (Action) -> Void

    var body: some View {
        VStack(spacing: 14) {
            switch phase {
            case .connecting(let target):
                ProgressView().controlSize(.large)
                Text("Connecting to \(hostName)").font(.headline)
                if !target.isEmpty {
                    Text(target).font(.caption.monospaced()).foregroundColor(.secondary)
                }
                steps(done: 0)
            case .attaching:
                ProgressView().controlSize(.large)
                Text("Starting ThinkTerm on \(hostName)").font(.headline)
                steps(done: 1)
            case .reconnecting:
                ProgressView().controlSize(.large)
                Text("Reconnecting to \(hostName)…").font(.headline)
            case .disconnected:
                Image(systemName: "bolt.slash").font(.system(size: 34)).foregroundColor(.secondary)
                Text("Disconnected").font(.headline)
                HStack {
                    Button("Back") { act(.back) }.buttonStyle(.bordered)
                    Button("Reconnect") { act(.retry) }.buttonStyle(.borderedProminent)
                }
            case .failed(let reason):
                Image(systemName: "exclamationmark.triangle").font(.system(size: 34)).foregroundColor(.orange)
                Text("Couldn't connect to \(hostName)").font(.headline).multilineTextAlignment(.center)
                Text(reason)
                    .font(.caption.monospaced())
                    .foregroundColor(.secondary)
                    .multilineTextAlignment(.center)
                    .lineLimit(6)
                if let hint = Self.hint(for: reason) {
                    Text(hint).font(.footnote).multilineTextAlignment(.center)
                }
                HStack {
                    Button("Back") { act(.back) }.buttonStyle(.bordered)
                    if canEdit {
                        Button("Edit host") { act(.edit) }.buttonStyle(.bordered)
                    }
                    if reason.contains("key changed") && canEdit {
                        Button("Forget key & retry") { act(.forgetKeyAndRetry) }.buttonStyle(.borderedProminent)
                    } else {
                        Button("Retry") { act(.retry) }.buttonStyle(.borderedProminent)
                    }
                }
            }
        }
        .padding(20)
        .frame(maxWidth: 340)
        .background(.regularMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 16))
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black.opacity(0.4))
    }

    private func steps(done: Int) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            step("Secure connection", state: done > 0 ? 2 : 1)
            step("ThinkTerm on the host", state: done > 1 ? 2 : (done == 1 ? 1 : 0))
        }
        .font(.footnote)
        .padding(.top, 4)
    }

    private func step(_ label: String, state: Int) -> some View {
        HStack(spacing: 8) {
            Image(systemName: state == 2 ? "checkmark.circle.fill" : (state == 1 ? "circle.dotted" : "circle"))
                .foregroundColor(state == 2 ? .green : .secondary)
            Text(label).foregroundColor(state == 0 ? .secondary : .primary)
        }
    }

    /// What the reason usually means, in plain words.
    static func hint(for reason: String) -> String? {
        let r = reason.lowercased()
        if r.contains("refused the login") || r.contains("auth") {
            return "The host did not accept the user name with this key or password."
        }
        if r.contains("key changed") {
            return "The host's key is not the one seen before. If the host was reinstalled, forget the old key."
        }
        if r.contains("timed out") || r.contains("connection refused") || r.contains("unreachable") || r.contains("no route") {
            return "The host did not answer on this address and port. Check the network, or a VPN such as Tailscale."
        }
        if r.contains("not found") || r.contains("exit 127") || r.contains("no such file") {
            return "ThinkTerm is not installed on the host, or not on its PATH."
        }
        if r.contains("codec") || r.contains("version") {
            return "The host runs another version of ThinkTerm. Update one of the two."
        }
        return nil
    }
}
