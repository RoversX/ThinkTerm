import SwiftUI

/// One host: its terminal, the tab strip, the bars over the panes, the
/// key bar, and the sidebar and menus as sheets.
struct TerminalScreen: View {
    @StateObject private var model: TerminalModel
    @ObservedObject private var settings = AppSettings.shared
    @ObservedObject private var lang = AppLanguage.shared
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dismiss) private var dismiss
    @State private var showTree = false
    @State private var showLog = false
    @State private var showSettings = false
    @State private var menu: MenuSheet?
    @State private var editingHost: Host?
    /// A bar being dragged moves the divider it hangs from.
    @State private var barDrag: CGPoint?
    /// The bar's width, for the strip in its middle: a principal item is
    /// centred and clipped when wider than its slot, so it is sized.
    @State private var barWidth: CGFloat = 402
    /// The software keyboard is up: the key bar shows with it.
    @State private var keyboardUp = false
    /// The overview: the terminal shrinks into its thread's card and the
    /// cards fade in around it, as the desktop's Live Overview zooms out.
    @State private var overviewOpen = false
    @State private var overviewShown = false
    @State private var terminalFrame: CGRect = .zero
    @State private var cardFrame: CGRect?
    @State private var zoomScale: CGFloat = 1
    @State private var zoomOffset: CGSize = .zero
    @State private var zoomClip: CGFloat?

    init(host: Host?, store: HostStore?) {
        _model = StateObject(wrappedValue: TerminalModel(host: host, store: store))
    }

    /// The tabs live in the navigation bar, between the system's own back
    /// button and the menu; the keys sit below the terminal, and nothing
    /// else. Every piece of chrome takes the terminal's own background.
    var body: some View {
        ZStack(alignment: .topLeading) {
            if overviewShown {
                OverviewScreen(model: model, isPresented: $overviewShown, cardFrame: $cardFrame, liveThread: currentThreadId)
                    .opacity(overviewOpen ? 1 : 0)
                    .onChange(of: overviewShown) { _, shown in if !shown { closeOverview() } }
            }
            // Above the cards, so the shrunken terminal shows in its card;
            // untouchable meanwhile, so the cards get the taps.
            VStack(spacing: 0) {
                if twoLevel {
                    tabSubstrip.opacity(overviewOpen ? 0 : 1)
                }
                terminal
                    .background(GeometryReader { geo in
                        Color.clear
                            .onAppear { terminalFrame = geo.frame(in: .named("screen")) }
                            .onChange(of: geo.frame(in: .named("screen"))) { _, f in terminalFrame = f }
                    })
                    .mask(alignment: .bottom) {
                        // The card's own corners, once the terminal is in it.
                        UnevenRoundedRectangle(bottomLeadingRadius: 13 / zoomScale, bottomTrailingRadius: 13 / zoomScale)
                            .frame(height: zoomClip)
                    }
                    .scaleEffect(zoomScale, anchor: .topLeading)
                    .offset(zoomOffset)
                    .zIndex(2)
                    .allowsHitTesting(!overviewOpen)
                statusLine.opacity(overviewOpen ? 0 : 1)
                if keyboardUp && !overviewOpen {
                    KeyBar(model: model)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
            .animation(.easeOut(duration: 0.2), value: keyboardUp)
            .zIndex(1)
            .allowsHitTesting(!overviewOpen)
        }
        .coordinateSpace(name: "screen")
        .background(overviewOpen ? Color(white: 0.06) : model.background)
        .background(GeometryReader { geo in
            Color.clear.onAppear { barWidth = geo.size.width }
                .onChange(of: geo.size.width) { _, w in barWidth = w }
        })
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { note in
            let frame = (note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect) ?? .zero
            keyboardUp = frame.height > 0
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
            keyboardUp = false
        }
        .onChange(of: cardFrame) { _, frame in
            // The card's thumbnail is laid out: the terminal goes there.
            if overviewShown, let frame, !overviewOpen { zoom(into: frame) }
        }
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.hidden, for: .tabBar)
        .toolbarBackground(model.background, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .principal) { tabStrip.frame(width: max(barWidth - 150, 120)) }
            ToolbarItem(placement: .topBarTrailing) {
                Button { if overviewShown { overviewShown = false } else { openOverview() } } label: {
                    Image(systemName: "square.grid.2x2")
                }
            }
        }
        .ignoresSafeArea(.container, edges: .bottom)
        .sheet(isPresented: $showTree) {
            TreeSheet(model: model, isPresented: $showTree, menu: $menu)
        }
        .sheet(isPresented: $showSettings) {
            NavigationStack {
                SettingsView(model: model, showLog: $showLog)
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) { Button(tr("done")) { showSettings = false } }
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
            let args = ProcessInfo.processInfo.arguments
            if !args.contains(where: { $0.hasSuffix("test") || $0 == "--autoconnect" }) {
                model.connect()
            }
            #if DEBUG
            // Screenshots of the sheets, which no script can tap open.
            if args.contains("--open-tree") {
                DispatchQueue.main.asyncAfter(deadline: .now() + 14) { showTree = true }
            }
            if args.contains("--open-overview") {
                DispatchQueue.main.asyncAfter(deadline: .now() + 14) { openOverview() }
            }
            #endif
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
        model.host?.display ?? tr("thismac")
    }

    private var statusColor: Color {
        if model.reconnecting { return .orange }
        if connected { return .green }
        if model.connection.hasPrefix("connecting") || model.connection.contains("reconnect") { return .orange }
        return .red
    }

    /// The desktop's two layers, as two rows: threads (the sidebar's
    /// layer, one per workspace) in the navigation bar, the current
    /// thread's tabs in a strip under it. A server without threads (no
    /// session state) shows its tabs in the bar instead.
    private var twoLevel: Bool { settings.tabBarLevels == "two" && !(model.threads?.threads.isEmpty ?? true) }

    /// The dot is the connection: a tap opens what can be done with it.
    private var connectionDot: some View {
        Menu {
            Section(hostName) {
                Button(connected ? tr("disconnect") : tr("reconnect")) {
                    if connected { model.disconnect() } else { model.connect() }
                }
                if model.host != nil {
                    Button(tr("edithost")) { editingHost = model.host }
                }
            }
        } label: {
            Circle().fill(statusColor).frame(width: 7, height: 7)
                .frame(width: 24, height: 24)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var tabStrip: some View {
        HStack(spacing: 2) {
            connectionDot
            ScrollViewReader { reader in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 4) {
                    if twoLevel {
                        ForEach(model.threads?.threads ?? []) { thread in
                            Button {
                                // The one on show opens the tree; another is shown.
                                if thread.current { showTree = true } else { model.sideClick("thread", id: thread.id) }
                            } label: {
                                HStack(spacing: 5) {
                                    Circle().fill(threadColor(thread)).frame(width: 6, height: 6)
                                    Text(thread.name)
                                        .font(.system(size: 12, weight: thread.current ? .semibold : .regular))
                                        .lineLimit(1)
                                }
                                .padding(.horizontal, 10)
                                .frame(minWidth: 64, minHeight: 26)
                                .background(thread.current ? Color.white.opacity(0.18) : Color.white.opacity(0.06))
                                .clipShape(Capsule())
                            }
                            .buttonStyle(.plain)
                            .contextMenu {
                                AppMenuItems(model: model, kind: "thread", id: thread.id)
                            }
                            .id("thread:" + thread.id)
                        }
                        Button { model.sideClick("new-thread") } label: {
                            Image(systemName: "plus").font(.system(size: 13, weight: .semibold)).padding(6)
                                .foregroundColor(Color.white.opacity(0.62))
                        }
                        .buttonStyle(.plain)
                    } else {
                        tabPills(size: 12, height: 26)
                        newTabButton
                    }
                }
            }
            .onChange(of: model.threads?.threads.first(where: { $0.current })?.id) { _, id in
                if let id { reader.scrollTo("thread:" + id, anchor: .center) }
            }
            .onChange(of: model.tabs?.tabs.first(where: { $0.current })?.tab) { _, tab in
                if let tab, !twoLevel { reader.scrollTo("tab:\(tab)", anchor: .center) }
            }
            }
        }
        .foregroundColor(.white)
    }

    /// The current thread's tabs, under the bar.
    private var tabSubstrip: some View {
        HStack(spacing: 4) {
            ScrollViewReader { reader in
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 4) {
                        tabPills(size: 11.5, height: 22)
                    }
                }
                .onChange(of: model.tabs?.tabs.first(where: { $0.current })?.tab) { _, tab in
                    if let tab { reader.scrollTo("tab:\(tab)", anchor: .center) }
                }
            }
            newTabButton
        }
        .foregroundColor(.white)
        .padding(.horizontal, 8)
        .frame(height: 32)
        .contentShape(Rectangle())
        // Pinching the strip zooms out to the overview.
        .gesture(MagnifyGesture().onEnded { value in
            if value.magnification < 0.8 { openOverview() }
        })
        .background(model.background)
    }

    private func tabPills(size: CGFloat, height: CGFloat) -> some View {
        ForEach(model.tabs?.tabs ?? []) { tab in
            Button {
                model.chromeClick("pane", pane: tab.target)
            } label: {
                Text(tab.label.isEmpty ? tr("tab.num", tab.tab) : tab.label)
                    .font(.system(size: size, weight: tab.current ? .semibold : .regular))
                    .lineLimit(1)
                    .padding(.horizontal, 10)
                    .frame(minWidth: 76, maxWidth: 140, minHeight: height)
                    .foregroundColor(tab.current ? .white : Color.white.opacity(0.62))
                    .background(tab.current ? Color.white.opacity(0.18) : Color.white.opacity(0.06))
                    .clipShape(Capsule())
            }
            .buttonStyle(.plain)
            .contextMenu {
                AppMenuItems(model: model, kind: "tab", id: String(tab.tab))
            }
            .id("tab:\(tab.tab)")
        }
    }

    /// "+" opens a tab; held, it offers the splits too.
    private var newTabButton: some View {
        Button { model.chromeClick("new-tab") } label: {
            Image(systemName: "plus").font(.system(size: 13, weight: .semibold)).padding(6)
                .foregroundColor(Color.white.opacity(0.62))
        }
        .buttonStyle(.plain)
        .contextMenu {
            Button(tr("newtab")) { model.chromeClick("new-tab") }
            Button(tr("split.right")) { model.chromeClick("split-right") }
            Button(tr("split.below")) { model.chromeClick("split-below") }
        }
    }

    static func threadColor(status: String, live: Bool) -> Color {
        switch status {
        case "Running": return .green
        case "NeedsAttention": return .orange
        case "Done": return .blue
        default: return live ? .gray : .gray.opacity(0.4)
        }
    }

    private func threadColor(_ t: ThreadView) -> Color { Self.threadColor(status: t.status, live: t.live) }

    // MARK: the overview's zoom

    private var currentThreadId: String? { model.threads?.threads.first(where: { $0.current })?.id }

    private func openOverview() {
        guard !overviewShown else { return }
        model.refreshViews()
        cardFrame = nil
        overviewShown = true
        // Without a card of its own (no threads, or none current) the
        // terminal only fades.
        if currentThreadId == nil {
            withAnimation(.easeOut(duration: 0.25)) { overviewOpen = true }
        }
    }

    /// Shrink the terminal into the card's thumbnail: scaled to its
    /// width, moved into it, and masked to its height from the bottom,
    /// so the card keeps the newest rows as the other cards' previews do.
    private func zoom(into frame: CGRect) {
        guard terminalFrame.width > 0 else { return }
        let scale = frame.width / terminalFrame.width
        let clip = frame.height / scale
        withAnimation(.spring(response: 0.4, dampingFraction: 0.85)) {
            overviewOpen = true
            zoomScale = scale
            zoomOffset = CGSize(
                width: frame.minX - terminalFrame.minX,
                height: frame.minY - terminalFrame.minY - (terminalFrame.height - clip) * scale
            )
            zoomClip = clip
        }
    }

    private func closeOverview() {
        withAnimation(.spring(response: 0.35, dampingFraction: 0.9)) {
            overviewOpen = false
            zoomScale = 1
            zoomOffset = .zero
            zoomClip = nil
        }
        cardFrame = nil
    }

    // MARK: the terminal with its overlays

    private var terminal: some View {
        ZStack(alignment: .topLeading) {
            MetalView()
            TerminalInput()
            if model.navs.count > 1 && settings.paneBars {
                navBars
            }
            if settings.scrollbar && model.scroll.1 > 0 && model.scrollShown {
                scrollbar
            }
            if settings.devMode {
                Text(model.stats)
                    .font(.system(size: 8, design: .monospaced))
                    .foregroundColor(.white.opacity(0.7))
                    .padding(4)
                    .background(Color.black.opacity(0.5))
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
                    .allowsHitTesting(false)
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
            if model.reconnecting && !settings.autoReconnect && ConnectionPhase(status: model.connection) == nil {
                // Asked not to redial: the card offers it instead.
                ConnectionCard(phase: .disconnected, hostName: hostName, canEdit: model.host != nil) { action in
                    switch action {
                    case .back: dismiss()
                    default: model.connect()
                    }
                }
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

    /// A thin bar at the right: where the view is in the scrollback.
    private var scrollbar: some View {
        GeometryReader { geo in
            let (above, max) = model.scroll
            let total = CGFloat(max) + geo.size.height / Swift.max(model.cellHeight, 1)
            let visible = geo.size.height / Swift.max(model.cellHeight, 1)
            let thumb = Swift.max(geo.size.height * visible / Swift.max(total, 1), 24)
            let track = geo.size.height - thumb
            let y = track * (1 - CGFloat(above) / CGFloat(Swift.max(max, 1)))
            RoundedRectangle(cornerRadius: 2)
                .fill(Color.white.opacity(0.35))
                .frame(width: 3, height: thumb)
                .offset(x: geo.size.width - 6, y: y)
        }
        .allowsHitTesting(false)
        .transition(.opacity)
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
        .contextMenu {
            AppMenuItems(model: model, kind: "pane", id: String(nav.rect.pane))
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
        let text = model.copied ? tr("copied") : (model.toastText ?? "")
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

/// The App's context menu for something, as the system menu that a long
/// press opens: its items, checks, separators and submenus, built when
/// the menu shows.
struct AppMenuItems: View {
    @ObservedObject var model: TerminalModel
    var kind: String
    var id: String

    var body: some View {
        let items = model.contextMenu(kind, id: id)
        ForEach(items, id: \.rowId) { item in
            Self.row(item, model: model)
        }
    }

    static func row(_ item: MenuItem, model: TerminalModel) -> AnyView {
        switch item.kind {
        case "separator":
            return AnyView(Divider())
        case "header":
            return AnyView(Text(item.label))
        default:
            if !item.submenu.isEmpty {
                let subs = item.submenu
                return AnyView(Menu(item.label) {
                    ForEach(subs, id: \.rowId) { sub in row(sub, model: model) }
                })
            }
            return AnyView(
                Button {
                    model.menuAction(item.id)
                } label: {
                    if item.checked {
                        Label(item.label, systemImage: "checkmark")
                    } else {
                        Text(item.label)
                    }
                }
                .disabled(!item.enabled)
            )
        }
    }
}

struct MenuList: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
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
                ToolbarItem(placement: .cancellationAction) { Button(tr("done"), action: dismiss) }
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


/// Where a connection is, read off the core's status line.
enum ConnectionPhase: Equatable {
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

    @ObservedObject private var lang = AppLanguage.shared

    var phase: ConnectionPhase
    var hostName: String
    var canEdit: Bool
    var act: (Action) -> Void

    var body: some View {
        VStack(spacing: 14) {
            switch phase {
            case .connecting(let target):
                ProgressView().controlSize(.large)
                Text(tr("card.connecting", hostName)).font(.headline)
                if !target.isEmpty {
                    Text(target).font(.caption.monospaced()).foregroundColor(.secondary)
                }
                steps(done: 0)
            case .attaching:
                ProgressView().controlSize(.large)
                Text(tr("card.attaching", hostName)).font(.headline)
                steps(done: 1)
            case .reconnecting:
                ProgressView().controlSize(.large)
                Text(tr("card.reconnecting", hostName)).font(.headline)
            case .disconnected:
                Image(systemName: "bolt.slash").font(.system(size: 34)).foregroundColor(.secondary)
                Text(tr("conn.disconnected")).font(.headline)
                HStack {
                    Button(tr("back")) { act(.back) }.buttonStyle(.bordered)
                    Button(tr("reconnect")) { act(.retry) }.buttonStyle(.borderedProminent)
                }
            case .failed(let reason):
                Image(systemName: "exclamationmark.triangle").font(.system(size: 34)).foregroundColor(.orange)
                Text(tr("card.failed", hostName)).font(.headline).multilineTextAlignment(.center)
                Text(reason)
                    .font(.caption.monospaced())
                    .foregroundColor(.secondary)
                    .multilineTextAlignment(.center)
                    .lineLimit(6)
                if let hint = Self.hint(for: reason) {
                    Text(hint).font(.footnote).multilineTextAlignment(.center)
                }
                HStack {
                    Button(tr("back")) { act(.back) }.buttonStyle(.bordered)
                    if canEdit {
                        Button(tr("edithost")) { act(.edit) }.buttonStyle(.bordered)
                    }
                    if reason.contains("key changed") && canEdit {
                        Button(tr("forgetretry")) { act(.forgetKeyAndRetry) }.buttonStyle(.borderedProminent)
                    } else {
                        Button(tr("retry")) { act(.retry) }.buttonStyle(.borderedProminent)
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
            step(tr("conn.step1"), state: done > 0 ? 2 : 1)
            step(tr("conn.step2"), state: done > 1 ? 2 : (done == 1 ? 1 : 0))
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
            return tr("hint.auth")
        }
        if r.contains("key changed") {
            return tr("hint.keychanged")
        }
        if r.contains("timed out") || r.contains("connection refused") || r.contains("unreachable") || r.contains("no route") {
            return tr("hint.unreachable")
        }
        if r.contains("not found") || r.contains("exit 127") || r.contains("no such file") {
            return tr("hint.notfound")
        }
        if r.contains("codec") || r.contains("version") {
            return tr("hint.version")
        }
        return nil
    }
}
