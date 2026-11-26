import SwiftUI

/// One host: its terminal, the tab strip, the bars over the panes, the
/// key bar, and the sidebar and menus as sheets.
struct TerminalScreen: View {
    /// The host's session, from `Sessions`: the screen shows it and
    /// gives it back on the way out, connection and all.
    @ObservedObject var model: TerminalModel
    @ObservedObject private var settings = AppSettings.shared
    @ObservedObject private var lang = AppLanguage.shared
    @EnvironmentObject private var sessions: Sessions
    @Environment(\.dismiss) private var dismiss
    @Environment(\.displayScale) private var displayScale
    @Environment(\.colorScheme) private var scheme
    /// The drawer is out (a narrow screen).
    @State private var showTree = false
    /// The screen is wide enough to keep the sidebar beside the terminal,
    /// and whether it is kept there.
    @State private var wide = false
    @AppStorage("sidebar.pinned") private var sidebarPinned = true
    @State private var showLog = false
    /// Going back to Threads from a chat opened there: the chat closes
    /// once the screen is gone.
    @State private var leavingToThreads = false
    /// The connection page's sky (`skyFollows`): when it came up, and when
    /// it rushes off, in seconds after that.
    @State private var skyStart: Date?
    @State private var skyRushAt: TimeInterval?
    @State private var showSettings = false
    @State private var menu: MenuSheet?
    @State private var editingHost: Host?
    /// A bar being dragged moves the divider it hangs from.
    @State private var barDrag: CGPoint?
    /// The software keyboard is up for the terminal: the key bar shows
    /// with it.
    @State private var keyboardUp = false
    /// The drawer's rows are made while it is open and while it slides
    /// shut, not while it lies hidden under the screen.
    @State private var drawerShown = false
    /// How far a swipe from the left edge has pulled the screen off the
    /// drawer, as the Claude app's does; it springs back if let go short.
    @GestureState(resetTransaction: Transaction(animation: TerminalScreen.slide))
    private var edgePull: CGFloat = 0
    @State private var edgeDrag = EdgeDrag()
    /// Where the system puts a bar along the top (`barRoom`): on an
    /// unfolded Duo, below the rounded corners, not at the screen's top.
    @State private var topRoom = EdgeInsets()
    /// A name the sidebar asked for (`SidebarView.editing`), kept here so
    /// the question stands wherever the sidebar is.
    @State private var renaming: (kind: String, id: String)?
    @State private var renameText = ""
    /// The overview (`OverviewScreen`): the terminal shrinks into its
    /// thread's card and the cards fade in around it, as the desktop's Live
    /// Overview zooms out; a card tapped grows back into the terminal.
    /// `overviewOpen` while it is the screen's, `overviewVisible` while its
    /// cards are up (a close included).
    @State private var overviewOpen = false
    @State private var overviewVisible = false
    @State private var cardsAlpha: Double = 0
    /// The split of the tab on show is up as a map (`PaneMap`).
    @State private var showPanes = false
    /// The right sidebar is out from under the screen (`RightPanel`).
    @State private var showRight = false
    /// It is made: while it shows, and until it has slid away.
    @State private var rightShown = false
    /// How far the screen has been pulled off it by a swipe from the
    /// right edge, and that swipe.
    @GestureState private var rightPull: CGFloat = 0
    @State private var rightDrag = EdgeDrag()
    /// Where the live card's picture is while the terminal is on its way
    /// into it; every card's is in `moves`.
    @State private var landingThumb: CGRect?
    @State private var terminalFrame: CGRect = .zero
    /// The terminal's top and bottom on the screen: the bar's buttons are
    /// above it and the keys below it scroll sideways, so a swipe from the
    /// edge there is theirs.
    @State private var terminalSpan: ClosedRange<CGFloat> = 0...CGFloat.infinity
    /// The terminal is hidden while a picture of it flies in its place.
    @State private var terminalAlpha: Double = 1
    @State private var flight = Flight()
    /// The thread the terminal went into the card of, and whether the
    /// card's picture shows in its place.
    @State private var liveThread: String?
    @State private var liveShown = false
    @State private var moves = OverviewMoves()
    /// Whether tabs lie past the start of the strip and past its end.
    @State private var stripEnds = StripEnds()
    /// Another host's terminal in this one's place, from the sidebar: a
    /// thread or a Space of that host picked there; for a Space, it comes
    /// with its sidebar out.
    var onHost: ((UUID, Bool) -> Void)?

    /// `panelOut`: the screen comes with its sidebar out, as the sidebar
    /// brought it here from another host's (a Space of this one picked).
    init(model: TerminalModel, panelOut: Bool = false, onHost: ((UUID, Bool) -> Void)? = nil) {
        _model = ObservedObject(wrappedValue: model)
        self.onHost = onHost
        _showTree = State(initialValue: panelOut)
        _drawerShown = State(initialValue: panelOut)
    }

    /// On a phone held upright the sidebar is a drawer the whole screen
    /// slides off, rounding its corner -- by the button or a swipe from the
    /// left edge -- and a tap on what shows of it comes back; on a wide screen (an iPad, an unfolded Duo, landscape) it stays
    /// at the left at full height, and the bar runs over the terminal only,
    /// as on the desktop.
    var body: some View {
        GeometryReader { geo in
            // Decided in the pass that lays it out, so the terminal is sized
            // once; the state follows a pass later, for the buttons.
            let wide = geo.size.width >= Self.wideWidth
            let drawerOpen = showTree && !wide
            let drawerWidth = min(Self.columnWidth + 40, geo.size.width * 0.84)
            let pulled = drawerOpen || edgePull > 0
            // The right sidebar is the drawer's mirror: under the screen
            // at its other edge, the screen sliding off it the other way.
            let rightPulled = showRight || rightPull > 0
            ZStack(alignment: .topLeading) {
                if !wide {
                    // The drawer lies under the screen, which slides off it.
                    sidebar(live: pulled || drawerShown)
                        .frame(width: drawerWidth)
                        .opacity(pulled ? 1 : 0)
                        .allowsHitTesting(drawerOpen)
                        .accessibilityHidden(!drawerOpen)
                }
                if rightPulled || rightShown {
                    RightPanel(model: model) { showRight = false }
                        .frame(width: drawerWidth)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                        .opacity(rightPulled ? 1 : 0)
                        .allowsHitTesting(showRight)
                        .accessibilityHidden(!showRight)
                }
                HStack(spacing: 0) {
                    if wide && sidebarPinned {
                        sidebar(live: true).frame(width: Self.columnWidth)
                        Rectangle().fill(Color.primary.opacity(0.08)).frame(width: 1).ignoresSafeArea()
                    }
                    screen
                        // The whole screen's height, the status bar's band
                        // and the home indicator's included: clipped to its
                        // own frame, the card stopped short of both and the
                        // drawer's ground showed above and below it.
                        .mask {
                            RoundedRectangle(cornerRadius: pulled || rightPulled ? 28 : 0, style: .continuous)
                                .ignoresSafeArea()
                        }
                        .overlay {
                            if drawerOpen || showRight {
                                // What shows of the screen takes it back.
                                // Fainter on a light page, where the dark
                                // one's shade reads as a grey veil.
                                RoundedRectangle(cornerRadius: 28, style: .continuous)
                                    .fill(Color.black.opacity(scheme == .dark ? 0.35 : 0.08))
                                    .ignoresSafeArea()
                                    .onTapGesture {
                                        showTree = false
                                        showRight = false
                                    }
                            }
                        }
                        .shadow(
                            color: .black.opacity(pulled || rightPulled ? (scheme == .dark ? 0.55 : 0.14) : 0),
                            radius: scheme == .dark ? 20 : 12
                        )
                }
                .offset(
                    x: drawerOpen ? drawerWidth
                        : showRight ? -drawerWidth
                        : min(edgePull, drawerWidth) - min(rightPull, drawerWidth)
                )
                if model.chatWith != nil {
                    // The chat with an agent, over the whole screen and its
                    // pane's terminal, which going back shows.
                    // Its own state for each chat: a notice count, a draft.
                    ChatPage(model: model, feed: model.chatFeed)
                        .id(model.chatWith?.pane)
                        .transition(.move(edge: .trailing))
                        .zIndex(10)
                }
            }
            .animation(Self.slide, value: model.chatWith)
            .simultaneousGesture(
                // A swipe right from the left edge opens the drawer, the
                // screen following the finger; one from further in is the
                // terminal's (a selection, a scroll).
                DragGesture(minimumDistance: 10, coordinateSpace: .global)
                    .updating($edgePull) { drag, pull, transaction in
                        // Back once it is none: brought back past where it began.
                        let next = pullsDrawer(drag) ? max(0, drag.translation.width) : 0
                        if next == 0 && pull > 0 {
                            transaction.animation = Self.slide
                        }
                        pull = next
                    }
                    .onEnded { drag in
                        // Far enough, or flung far enough, and not flung back.
                        let far = drag.translation.width > drawerWidth / 3 || drag.predictedEndTranslation.width > drawerWidth / 2
                        if far && drag.velocity.width > -200 && pullsDrawer(drag) { showSidebar() }
                        edgeDrag.start = nil
                    },
                including: wide || drawerOpen ? .subviews : .all
            )
            .simultaneousGesture(
                // The same from the right edge, for the right sidebar.
                DragGesture(minimumDistance: 10, coordinateSpace: .global)
                    .updating($rightPull) { drag, pull, transaction in
                        let next = pullsRight(drag, right: geo.frame(in: .global).maxX) ? max(0, -drag.translation.width) : 0
                        if next == 0 && pull > 0 {
                            transaction.animation = Self.slide
                        }
                        pull = next
                    }
                    .onEnded { drag in
                        let far = -drag.translation.width > drawerWidth / 3 || -drag.predictedEndTranslation.width > drawerWidth / 2
                        if far && drag.velocity.width < 200 && pullsRight(drag, right: geo.frame(in: .global).maxX) { openRight() }
                        rightDrag.start = nil
                    },
                including: drawerOpen || showRight || overviewOpen ? .subviews : .all
            )
            .simultaneousGesture(
                DragGesture(minimumDistance: 24).onEnded { drag in
                    // A swipe back toward the side it came from, not a
                    // scroll of the list that drifts. One across the drawer
                    // is its list's, which goes to another Space with it.
                    let move = drag.translation
                    guard abs(move.width) > abs(move.height) else { return }
                    if drawerOpen && move.width < -60 && drag.startLocation.x > drawerWidth { showTree = false }
                    if showRight && move.width > 60 { showRight = false }
                },
                including: drawerOpen || showRight ? .all : .subviews
            )
            .animation(Self.slide, value: showRight)
            .animation(Self.slide, value: showTree)
            .animation(Self.slide, value: sidebarPinned)
            .onAppear {
                self.wide = wide
                // Wide, the sidebar is a column: the drawer has no place.
                if wide { showTree = false }
                model.edgePullsDrawer = !wide && !showTree
                // No room for a split's panes side by side: one at a time.
                model.setOnePane(!wide)
            }
            .onChange(of: wide) { _, wide in
                self.wide = wide
                if wide { showTree = false }
                model.edgePullsDrawer = !wide && !showTree
                model.setOnePane(!wide)
            }
        }
        // Under the status bar: the terminal's colour, or the sidebar's
        // beside an open drawer (the screen is clipped to its frame).
        .background((drawerOpen || showRight ? Color.chromeBackdrop : cardsAlpha > 0 ? OverviewScreen.backdrop : model.background).ignoresSafeArea())
        // The terminal is as dark as its colour scheme, whatever the
        // system's appearance: the bars and the sidebar beside it follow
        // it, as does the status bar over it. The overview is dark.
        .preferredColorScheme(model.dark || cardsAlpha > 0 ? .dark : .light)
        .onChange(of: drawerOpen) { _, open in
            model.edgePullsDrawer = !wide && !open
            if open {
                drawerShown = true
            } else {
                // The only field it can leave focused is its search.
                UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    if !drawerOpen { drawerShown = false }
                }
            }
        }
        .onChange(of: showRight) { _, open in
            // Made while it shows and until it has slid away, as the drawer.
            if open {
                rightShown = true
            } else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    if !showRight { rightShown = false }
                }
            }
        }
        .toolbar(.hidden, for: .navigationBar)
        .toolbar(.hidden, for: .tabBar)
    }

    /// Wide enough for the sidebar to stay beside the terminal.
    private static let wideWidth: CGFloat = 700
    /// How near the left edge a swipe must start to open the drawer.
    static let edgeWidth: CGFloat = 24
    /// The screen sliding off the drawer and back, and the column coming
    /// and going.
    private static let slide = Animation.spring(response: 0.34, dampingFraction: 0.88)
    private static let columnWidth: CGFloat = 280
    private static let barHeight: CGFloat = 54
    /// How far in from an end of the tab strip its tabs fade.
    private static let stripFade: CGFloat = 28
    private var drawerOpen: Bool { showTree && !wide }

    /// A drag that pulls the drawer out: from the screen's left edge and
    /// sideways, over the terminal (the bar's buttons are above it, the
    /// keys under it scroll sideways), and not a selection's handle being
    /// moved. Decided as it sets off and kept, so a thumb's arc turning up
    /// or down on the way does not let go of the screen.
    private func pullsDrawer(_ drag: DragGesture.Value) -> Bool {
        if edgeDrag.start != drag.startLocation {
            let move = drag.translation
            edgeDrag.start = drag.startLocation
            edgeDrag.pulls = drag.startLocation.x < Self.edgeWidth && move.width > abs(move.height)
                && terminalSpan.contains(drag.startLocation.y) && model.inputView?.holdsSelection != true
                && model.chatWith == nil
        }
        return edgeDrag.pulls
    }

    /// `pullsDrawer`, from the right edge and leftward: the right sidebar.
    /// `right` is where that edge is on screen -- in from the screen's own
    /// by the safe area, sideways.
    private func pullsRight(_ drag: DragGesture.Value, right: CGFloat) -> Bool {
        if rightDrag.start != drag.startLocation {
            let move = drag.translation
            rightDrag.start = drag.startLocation
            rightDrag.pulls = drag.startLocation.x > right - Self.edgeWidth && -move.width > abs(move.height)
                && terminalSpan.contains(drag.startLocation.y) && model.inputView?.holdsSelection != true
                && model.chatWith == nil
        }
        return rightDrag.pulls
    }

    /// The sidebar, opened (a drawer) or brought back (a column).
    private func showSidebar() {
        moves.waiting = false
        if wide { sidebarPinned = true } else { model.dismissKeyboard(); showTree = true }
    }

    /// A drawer covers the terminal and takes its keyboard down; a column
    /// leaves both be.
    private func toggleSidebar() {
        moves.waiting = false
        if wide { sidebarPinned.toggle() } else { model.dismissKeyboard(); showTree.toggle() }
    }

    /// The right sidebar, the screen sliding off it; it takes the
    /// keyboard down, as the drawer does.
    private func openRight() {
        moves.waiting = false
        model.dismissKeyboard()
        showRight = true
    }

    /// Back to the hosts, an overview waiting to open left shut: the
    /// screen is gone only once it has slid away.
    private func leave() {
        moves.waiting = false
        dismiss()
    }

    private func threadName(_ id: String) -> String {
        for space in model.tree?.spaces ?? [] {
            for project in space.projects {
                if let thread = project.threads.first(where: { $0.id == id }) { return thread.name }
            }
        }
        return ""
    }

    private func sidebar(live: Bool) -> some View {
        SidebarPanel(
            model: model,
            done: { if !wide { showTree = false } },
            onEditHost: { showTree = false; editingHost = model.host },
            onSettings: { showSettings = true },
            onOverview: { openOverview() },
            onLeave: { leave() },
            onHost: onHost,
            overviewOn: overviewOpen,
            live: live
        )
    }

    /// The terminal's page: the bar, then the strip, the terminal, the
    /// status line and the keys. Every piece of chrome takes the
    /// terminal's own background.
    private var screen: some View {
        VStack(spacing: 0) {
            topBar
            content
        }
        .barRoom(.top, height: Self.barHeight, $topRoom)
        .background((cardsAlpha > 0 ? OverviewScreen.backdrop : model.background).ignoresSafeArea())
        .ignoresSafeArea(.container, edges: .bottom)
        // Another keyboard (the sidebar's search, a name being asked for)
        // is not the terminal's: it does not resize it.
        .ignoresSafeArea(keyboardUp ? [] : .keyboard, edges: .bottom)
        .overlay { connectionPage }
        // Over the whole page, the bars at its foot included.
        .overlay {
            if showPanes {
                PaneMap(model: model) { showPanes = false }
            }
        }
        .animation(.easeOut(duration: 0.18), value: showPanes)
    }

    /// The connection page, over the whole screen, the bars included: the
    /// star sky, and on it how the connection is going (`ConnectionCard`).
    private var connectionPage: some View {
        ZStack {
            // The night the connection page sits on, from the moment it
            // covers the terminal; it streaks past as the pane comes up.
            if let start = skyStart {
                StarSky(start: start, rushAt: skyRushAt, travel: Travel())
                    .ignoresSafeArea()
                    // What it covers takes no touches, as under the launch's
                    // sky; as it rushes off, the pane already does.
                    .contentShape(Rectangle())
                    .onTapGesture {}
                    .allowsHitTesting(skyRushAt == nil)
                    .transition(.opacity)
            }
            if connectionCovers, ConnectionPhase(status: model.connection) == nil {
                // Asked not to redial: the card offers it instead.
                ConnectionCard(phase: .disconnected, hostName: hostName, endpoint: endpoint, canEdit: model.host != nil) { action in
                    switch action {
                    case .back: leave()
                    default: model.connect()
                    }
                }
            }
            if connectionCovers, let phase = ConnectionPhase(status: model.connection) {
                ConnectionCard(phase: phase, hostName: hostName, endpoint: endpoint, canEdit: model.host != nil) { action in
                    switch action {
                    case .retry: model.connect()
                    case .forgetKeyAndRetry:
                        if case .failed(let reason) = phase {
                            model.forgetKeyAndRetry(reason: reason, sessions: sessions)
                        } else {
                            if let host = model.host {
                                model.store?.forgetHostKey(for: host.id)
                                model.host?.knownHost = nil
                            }
                            model.connect()
                        }
                    case .edit: editingHost = model.host
                    case .back: leave()
                    }
                }
                .transition(.opacity)
            }
        }
        .animation(.easeOut(duration: 0.25), value: connectionCovers)
        .onAppear { if connectionCovers { skyStart = Date() } }
        .onChange(of: connectionCovers) { _, covers in skyFollows(covers) }
    }

    /// The sidebar, back to the hosts, and the strip in the rest of the
    /// bar; the round buttons in the system's glass.
    private var topBar: some View {
        HStack(spacing: 8) {
            Button { toggleSidebar() } label: { barIcon("sidebar.left") }
                .buttonStyle(.plain)
                .glassCircle()
                .accessibilityLabel(tr("sidebar"))
            if overviewOpen {
                // The overview's own bar: its name, and the way back.
                Text(tr("overview"))
                    .font(.headline)
                    .foregroundColor(.white)
                    .lineLimit(1)
                    .padding(.leading, 4)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button { closeOverview() } label: { barIcon("xmark") }
                    .buttonStyle(.plain)
                    .glassCircle()
                    .accessibilityLabel(tr("done"))
            } else {
                tabStrip.frame(maxWidth: .infinity, alignment: .leading)
                Button { openRight() } label: { barIcon("sidebar.right") }
                    .buttonStyle(.plain)
                    .glassCircle()
                    .accessibilityLabel(tr("panel.right"))
            }
        }
        // The screen's left side is the sidebar's while it stays there.
        .padding(.leading, max(10, wide && sidebarPinned ? 0 : topRoom.leading))
        .padding(.trailing, max(10, topRoom.trailing))
        .frame(height: Self.barHeight)
        .padding(.top, max(0, topRoom.top))
        .contentShape(Rectangle())
        // Pinching the bar zooms out to the overview.
        .gesture(MagnifyGesture().onEnded { value in
            if value.magnification < 0.8 { openOverview() }
        })
        // The text follows the system's size, as far as the bar holds it.
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
    }

    private func barIcon(_ name: String) -> some View {
        Image(systemName: name)
            .font(.body.weight(.semibold))
            .foregroundColor(cardsAlpha > 0 ? .white : model.ink)
            .frame(width: 42, height: 42)
            .contentShape(Circle())
    }

    private var content: some View {
        ZStack(alignment: .topLeading) {
            if overviewVisible {
                OverviewScreen(
                    model: model, stage: terminalFrame.size, liveThread: liveThread, liveShown: liveShown,
                    onThumb: { noteThumb($0, $1) }, onPick: { pick($0) }, onClose: { closeOverview() }
                )
                .opacity(cardsAlpha)
                .allowsHitTesting(overviewOpen)
            }
            // Above the cards, so the terminal shows on its way into its
            // card; untouchable meanwhile, so the cards get the taps.
            VStack(spacing: 0) {
                terminal
                    .opacity(terminalAlpha)
                    // Files on their way float over the terminal's foot:
                    // in the layout they would resize it as they come
                    // and go.
                    .overlay(alignment: .bottom) {
                        UploadStrip(model: model).padding(.bottom, 8)
                    }
                    // The connection's word floats over the terminal's top
                    // for the same reason.
                    .overlay(alignment: .top) {
                        statusCapsule
                            .padding(.top, 8)
                            .padding(.horizontal, 16)
                            .opacity(overviewOpen ? 0 : 1)
                    }
                    .onGeometryChange(for: CGRect.self) { $0.frame(in: .named("screen")) } action: { terminalFrame = $0 }
                    .onGeometryChange(for: ClosedRange<CGFloat>.self) { geo in
                        let frame = geo.frame(in: .global)
                        return frame.minY...max(frame.minY, frame.maxY)
                    } action: { terminalSpan = $0 }
                    .zIndex(2)
                if !overviewOpen {
                    // Under the terminal whether the keyboard is up or
                    // not, so the terminal is sized once as it comes and
                    // goes: the tab's panes, then the keys. With the
                    // keyboard down they sit on the home indicator's
                    // inset; with it up, on the keyboard.
                    VStack(spacing: 8) {
                        PaneSwitcher(model: model) { showPanes = true }
                        KeyBar(model: model, keyboardUp: keyboardUp)
                    }
                    .padding(.top, 6)
                    .padding(.bottom, keyboardUp ? 6 : max(Self.bottomInset, 8))
                    // Out of sight while the connection page covers the
                    // terminal, but still in the layout: the terminal keeps
                    // its size, and the pane is not resized twice a dial.
                    .opacity(connectionCovers ? 0 : (connectionHeld ? 0.4 : 1))
                    .allowsHitTesting(!connectionCovers && !connectionHeld)
                    .animation(.easeOut(duration: 0.2), value: connectionCovers)
                    .animation(.easeOut(duration: 0.2), value: connectionHeld)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
            .animation(.easeOut(duration: 0.2), value: keyboardUp)
            .animation(.easeOut(duration: 0.2), value: model.panelOpen)
            .animation(.easeOut(duration: 0.2), value: model.uploads.map(\.id))
            .zIndex(1)
            .allowsHitTesting(!overviewVisible)
            // A picture of the terminal on its way into a card or out of one.
            Flyway(flight: flight)
                .allowsHitTesting(false)
                .zIndex(3)
        }
        .coordinateSpace(name: "screen")
        .background(cardsAlpha > 0 ? OverviewScreen.backdrop : model.background)
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { note in
            // The sidebar's search has a keyboard too, and no key bar. A
            // floating keyboard (an iPad's) leaves the bottom as it was.
            keyboardUp = KeyboardCover.height(note, in: model.inputView?.window) > 0
                && model.inputView?.isFirstResponder == true
            // The terminal's back before the overview it was put away for.
            if keyboardUp { moves.waiting = false }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
            keyboardUp = false
        }
        .onChange(of: landingThumb) { _, thumb in
            // The cards are laid out (or the card moved, scrolled into
            // sight), even after the picture got where they were thought to
            // be: it goes on to the card, and the cards come out from under
            // the cover.
            guard let thumb, overviewOpen, !liveShown, !moves.closing else { return }
            flyIn(to: thumb)
            flight.fadeCover(to: 0, duration: 0.3)
        }
        .onChange(of: livePicture) { _, _ in fadeIntoCard() }
        .onChange(of: currentThreadId) { _, now in
            if overviewOpen, !moves.closing, let now, now != liveThread,
               let thread = model.threads?.threads.first(where: { $0.id == now }) {
                // Another thread picked elsewhere (the sidebar): it is the
                // one the overview closes into.
                pick(thread)
            } else if moves.picked != nil {
                // The picked thread is the terminal's now; its first frame
                // follows.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { revealPicked(force: false) }
            }
        }
        .sheet(item: $model.pendingHostKey) { challenge in
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    Text(tr(challenge.jump ? "hostkey.approve.jumptitle" : "hostkey.approve.title")).font(.headline)
                    if challenge.jump {
                        Text(tr("hostkey.approve.jumpwho", challenge.jumpName))
                    }
                    // Verbatim: an interpolated number is localised, "2,299".
                    Text(verbatim: "\(challenge.hostname):\(challenge.port)")
                    Text(challenge.fingerprint).font(.system(.body, design: .monospaced))
                        .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
                    Text(tr("hostkey.approve.body"))
                    HStack {
                        Button(tr("cancel"), role: .cancel) { model.disconnect() }
                        Spacer()
                        Button(tr("hostkey.approve.accept")) { model.approveHostKey(challenge) }
                    }
                }.padding(24)
            }.presentationDetents([.medium, .large])
        }
        .sheet(isPresented: $showSettings) {
            NavigationStack {
                SettingsView(showLog: $showLog)
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) { Button(tr("done")) { showSettings = false } }
                    }
            }
        }
        .onChange(of: showLog) { _, on in model.logShown = on }
        // Back from a chat opened on Threads: to Threads, the chat going
        // with the screen once it is gone.
        .onChange(of: model.backToThreads) { _, _ in
            leavingToThreads = true
            leave()
        }
        .onChange(of: model.sidebar?.editing) { _, editing in
            // A new project has no id yet: the same field, asking for the
            // name of what is about to exist.
            guard let editing, editing.kind != "none" else { return }
            if renaming == nil {
                // A rename starts from the current name, selected.
                renameText = editing.name ?? ""
                renaming = (editing.kind, editing.id ?? "")
            }
        }
        .onChange(of: model.nameAsks) { _, _ in
            // "+" asks at once, even while the core still asks from before.
            renameText = ""
            renaming = ("new-project", "")
        }
        .onChange(of: model.sidebar?.new_project_error) { _, error in
            // A folder the core refused: asked again, with why.
            if error != nil, model.sidebar?.editing.kind == "new-project", renaming == nil {
                renaming = ("new-project", "")
            }
        }
        .alert(
            renaming?.kind == "new-project" ? tr("project.new") : tr("rename"),
            isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })
        ) {
            // A new project is a folder on the host, ~/dir or /dir.
            TextField(renaming?.kind == "new-project" ? "~/" : tr("f.name"), text: $renameText)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
            Button(tr("save")) {
                model.sideKey("Enter", value: renameText)
                renaming = nil
            }
            Button(tr("cancel"), role: .cancel) {
                model.sideKey("Escape", value: "")
                renaming = nil
            }
        } message: {
            if renaming?.kind == "new-project", let error = model.sidebar?.new_project_error {
                Text(error)
            }
        }
        .selectsOnEditing(renaming == nil || renaming?.kind == "new-project" ? nil : renameText)
        .confirmationDialog(
            tr("thread.delete.title", threadName(model.confirming?.split(separator: ":", maxSplits: 1).last.map(String.init) ?? "")),
            isPresented: Binding(get: { model.confirming != nil }, set: { if !$0 { model.confirming = nil } }),
            titleVisibility: .visible
        ) {
            // Deleting ends every program in the thread: it is asked first.
            Button(tr("delete"), role: .destructive) {
                if let action = model.confirming { model.menuAction(action) }
                model.confirming = nil
            }
            Button(tr("cancel"), role: .cancel) { model.confirming = nil }
        } message: {
            Text(tr("thread.delete.body"))
        }
        .sheet(item: $menu) { sheet in
            MenuList(model: model, sheet: sheet) { menu = nil }
                .presentationDetents([.medium, .large])
        }
        .sheet(item: $editingHost) { host in
            HostEditView(host: host) { edited, secret, passphrase in
                if !secret.isEmpty { Keychain.save(secret, account: edited.id.uuidString) }
                if let passphrase { Keychain.save(passphrase, account: edited.id.uuidString + ".passphrase") }
                let before = model.store?.hosts.first { $0.id == edited.id }
                model.store?.upsert(edited)
                // As stored: a new address drops the old pin.
                let saved = model.store?.hosts.first { $0.id == edited.id } ?? edited
                let credentialsChanged = !secret.isEmpty || passphrase != nil
                model.applyHostEdit(saved, credentialsChanged: credentialsChanged, connectOnSave: true)
                if let store = model.store {
                    sessions.jumpHostKept(before: before, saved: saved, credentialsChanged: credentialsChanged, store: store)
                }
            }
        }
        .onAppear {
            moves.onScreen = true
            model.enterScreen()
            // The lists show this host's Space from now on.
            let shown = (model.host?.id ?? Sessions.probeId).uuidString
            if settings.spaceHost != shown { settings.spaceHost = shown }
            // The automated flows connect on their own schedule; a session
            // found open is simply shown. One lost and not redialing by
            // itself (auto-reconnect off) is dialled again: coming to it --
            // the host page's Connect, a thread of it -- is the tap that
            // asks for it.
            if !TerminalModel.automated, !model.isConnected || (model.reconnecting && !model.autoReconnect) {
                model.connect()
            }
            #if DEBUG
            let args = ProcessInfo.processInfo.arguments
            // When a flag fires: the seconds after it, if given.
            func when(_ flag: String, _ otherwise: Double) -> Double? {
                guard let at = args.firstIndex(of: flag) else { return nil }
                return at + 1 < args.count ? Double(args[at + 1]) ?? otherwise : otherwise
            }
            // On time: the system lets a long wait run late by up to a
            // tenth of it, which would run two flags a moment apart as one.
            // Not once the screen is gone.
            func after(_ seconds: Double, _ work: @escaping () -> Void) {
                let timer = DispatchSource.makeTimerSource(queue: .main)
                timer.schedule(deadline: .now() + seconds, leeway: .milliseconds(1))
                timer.setEventHandler {
                    if moves.onScreen { work() }
                    timer.cancel()
                }
                timer.resume()
            }
            // Screenshots of the sheets, which no script can tap open.
            if let t = when("--open-tree", 14) { after(t) { showSidebar() } }
            if let t = when("--open-overview", 14) { after(t) { openOverview() } }
            if let t = when("--open-panes", 14) { after(t) { showPanes = true } }
            if let t = when("--open-right", 14) { after(t) { openRight() } }
            // The overview's ways back: a running thread's card by its
            // place among them, or the close button.
            if let at = args.firstIndex(of: "--overview-pick"), at + 1 < args.count, let n = Int(args[at + 1]) {
                after(17) {
                    let running = (model.threads?.threads ?? []).filter(\.live)
                    if running.indices.contains(n) { pick(running[n]) }
                }
            }
            if let t = when("--overview-close", 17) { after(t) { closeOverview() } }
            #endif
        }
        .onDisappear {
            // The picture goes, the connection stays (see Sessions), and an
            // overview left open, or on its way, lets its screens go.
            moves.onScreen = false
            overviewClosed()
            model.leaveScreen()
            // A chat opened on Threads ends with the screen it was over --
            // only when going back there: a picture opened full screen
            // from the chat takes the screen out of sight too.
            if leavingToThreads {
                leavingToThreads = false
                model.closeChat(toTerminal: true)
            }
        }
    }

    // MARK: the bar under the terminal

    private var connected: Bool {
        !(model.connection.hasPrefix("disconnected") || model.connection.hasPrefix("failed") || model.connection.isEmpty)
    }

    /// Where the host is dialled, as the connection page names it under
    /// the host: the user, the address, and the port when it is not 22.
    private var endpoint: String {
        guard let host = model.host, !host.hostname.isEmpty else { return "" }
        let user = host.user.isEmpty ? "" : host.user + "@"
        let port = host.port == 22 ? "" : ":\(host.port)"
        return user + host.hostname + port
    }

    /// The sky under the connection page: up as the page comes, and, when
    /// the page goes because the pane is there, rushed off to show it, as
    /// the app comes out from behind the launch's. Otherwise it fades.
    private func skyFollows(_ covers: Bool) {
        if covers {
            skyRushAt = nil
            skyStart = Date()
            // Nothing typed reaches the terminal under the page, and the
            // keyboard would cover the page's buttons.
            model.dismissKeyboard()
            return
        }
        guard let start = skyStart, skyRushAt == nil else { return }
        guard model.connection.hasPrefix("pane ") else {
            withAnimation(.easeOut(duration: 0.25)) { skyStart = nil }
            return
        }
        let at = Date().timeIntervalSince(start)
        skyRushAt = at
        SkyHaptics.connected(shownFor: at)
        DispatchQueue.main.asyncAfter(deadline: .now() + LaunchSky.afterRush) {
            // Unless another page has come up meanwhile, with its own sky.
            guard skyStart == start, skyRushAt == at else { return }
            skyStart = nil
            skyRushAt = nil
        }
    }

    /// The connection page covers the terminal (`ConnectionCard`): the
    /// keys under it have nothing to type into.
    private var connectionCovers: Bool {
        // The overview's cards are what the screen shows then: the page
        // waits until it closes, as the card it replaced did.
        if model.pausedForBackground || overviewOpen { return false }
        if let phase = ConnectionPhase(status: model.connection) {
            return !model.cardHeld || phase.isFailure
        }
        return model.reconnecting && !model.autoReconnect
    }

    /// The first moments of a dial back from the background (`cardHeld`):
    /// the last frame shows, dimmed, and neither it nor the keys under it
    /// take a touch, the connection not being live yet.
    private var connectionHeld: Bool {
        model.cardHeld && (ConnectionPhase(status: model.connection).map { !$0.isFailure } ?? false)
    }

    private var hostName: String {
        model.host?.display ?? tr("thismac")
    }

    var statusColor: Color {
        // A redial in its first quiet moments shows as the terminal did.
        if model.reconnecting && !model.redialQuiet { return .orange }
        if connected { return .green }
        if model.connection.hasPrefix("connecting") || model.connection.contains("reconnect") { return .orange }
        return .red
    }

    /// The current thread's tabs, as the desktop's bar has them; the
    /// threads are the sidebar's.
    private var tabStrip: some View {
        let current = model.tabs?.tabs.first(where: { $0.current })?.tab
        return ScrollViewReader { reader in
            ScrollView(.horizontal, showsIndicators: false) {
                // As tall as the round buttons beside them, in their glass.
                HStack(spacing: 8) {
                    tabPills(height: 42)
                    newTabButton
                }
                // A scroll comes to rest with a whole pill at the start.
                .scrollTargetLayout()
                .glassGroup(spacing: 8)
            }
            .scrollTargetBehavior(.viewAligned)
            // The glass swells as it is pressed, above and below the strip;
            // at its ends the tabs scrolled off it are cut, not drawn over
            // the buttons before it, and fade out on their way there.
            .scrollClipDisabled()
            .clipShape(SideClip())
            .onScrollGeometryChange(for: StripEnds.self) { geo in
                StripEnds(before: geo.visibleRect.minX > 1, after: geo.visibleRect.maxX < geo.contentSize.width - 1)
            } action: { _, ends in
                stripEnds = ends
            }
            .overlay { stripFades }
            .onAppear {
                // First shown, or back from the overview: the current tab
                // at the start, once the strip is laid out.
                DispatchQueue.main.async {
                    if let current { reader.scrollTo("tab:\(current)", anchor: .leading) }
                }
            }
            .onChange(of: current) { _, tab in
                // A new tab's pill is laid out in this same pass: at the
                // start once it is.
                DispatchQueue.main.async {
                    if let tab { reader.scrollTo("tab:\(tab)", anchor: .leading) }
                }
            }
        }
        .foregroundColor(model.ink)
    }

    /// The bar's ground over the strip's ends where tabs lie past them, so
    /// they fade out there rather than end in a cut; an end with nothing
    /// past it stays sharp.
    private var stripFades: some View {
        let ground = cardsAlpha > 0 ? OverviewScreen.backdrop : model.background
        return HStack(spacing: 0) {
            LinearGradient(colors: [ground, ground.opacity(0)], startPoint: .leading, endPoint: .trailing)
                .frame(width: Self.stripFade)
                .opacity(stripEnds.before ? 1 : 0)
            Spacer(minLength: 0)
            LinearGradient(colors: [ground.opacity(0), ground], startPoint: .leading, endPoint: .trailing)
                .frame(width: Self.stripFade)
                .opacity(stripEnds.after ? 1 : 0)
        }
        .animation(.easeOut(duration: 0.15), value: stripEnds)
        .allowsHitTesting(false)
    }

    /// The tabs as the desktop's capsules: the card of what each runs at
    /// the left, round within the capsule's round end, then its name.
    private func tabPills(height: CGFloat) -> some View {
        ForEach(model.tabs?.tabs ?? []) { tab in
            Button {
                model.chromeClick("pane", pane: tab.target)
            } label: {
                // A window tab wears the terminal's card, as on the
                // desktop; what each pane runs is on its capsule.
                let icon = model.tabs?.icon
                AtMost(width: 180) {
                    HStack(spacing: 8) {
                        TabIconCard(core: model.core, id: icon, size: height - 12, revision: model.iconsRevision)
                        Text(tab.label.isEmpty ? tr("tab.num", tab.tab) : tab.label)
                            .font(.subheadline.weight(tab.current ? .semibold : .regular))
                            .lineLimit(1)
                        if tab.current {
                            closeX { model.chromeClick("close-tab", tab: tab.tab) }
                        }
                    }
                    .padding(.leading, icon == nil ? 14 : 6)
                    .padding(.trailing, tab.current ? 6 : 14)
                }
                .frame(minWidth: 76, minHeight: height)
                .foregroundColor(tab.current ? model.ink : model.ink.opacity(0.62))
                .contentShape(Capsule())
            }
            .buttonStyle(.plain)
            .glassPill(lit: tab.current, ink: model.ink)
            .contextMenu {
                AppMenuItems(model: model, kind: "tab", id: String(tab.tab))
            }
            .id("tab:\(tab.tab)")
        }
    }

    /// The × at the end of the current pill: what the desktop's tabs carry.
    private func closeX(_ action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: "xmark")
                .font(.system(size: 11, weight: .bold))
                .foregroundColor(model.ink.opacity(0.7))
                .frame(width: 30, height: 30)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
    }

    /// "+" opens a tab; held, it offers the splits too.
    private var newTabButton: some View {
        Button { model.chromeClick("new-tab") } label: { barIcon("plus") }
            .buttonStyle(.plain)
            .glassCircle()
            .accessibilityLabel(tr("newtab"))
            .contextMenu {
                Button(tr("newtab")) { model.chromeClick("new-tab") }
                Button(tr("split.right")) { model.chromeClick("split-right") }
                Button(tr("split.below")) { model.chromeClick("split-below") }
            }
    }

    // MARK: the overview

    private var currentThreadId: String? { model.threads?.threads.first(where: { $0.current })?.id }

    /// Where the live card's picture is, its pane, and whether its
    /// picture is drawn.
    private var liveThumb: CGRect? { liveThread.flatMap { moves.thumbs[$0] } }
    private var livePane: Int? {
        liveThread.flatMap { id in model.threads?.threads.first(where: { $0.id == id }) }.flatMap(OverviewScreen.pane(of:))
    }
    private var livePicture: Bool { livePane.flatMap { model.pictures[$0] } != nil }

    /// A card's picture moved (laid out, scrolled): kept for the flights,
    /// and the live one's followed while the terminal is on its way in.
    private func noteThumb(_ id: String, _ rect: CGRect) {
        moves.thumbs[id] = rect
        if id == liveThread, overviewOpen, !liveShown, !moves.closing { landingThumb = rect }
    }

    /// The home indicator's inset, for a bar that sits at the bottom
    /// while the keyboard is down.
    static var bottomInset: CGFloat {
        UIApplication.shared.connectedScenes
            .compactMap { ($0 as? UIWindowScene)?.keyWindow }
            .first?.safeAreaInsets.bottom ?? 0
    }

    /// Into the overview. The terminal as it is flies off at once, for
    /// where its card is bound to be, over a cover under which the cards
    /// are made; Core Animation keeps it going while they are, and the
    /// cards come out from under the cover once they are laid out.
    private func openOverview() {
        guard !overviewVisible, !flight.flying, !moves.waiting else {
            // Asked again from the drawer, or while it slides away: only
            // the drawer goes.
            if drawerOpen { showTree = false }
            return
        }
        guard drawerOpen || keyboardUp || model.panelOpen else { return zoomOut() }
        // The terminal back in its place, uncovered and at its full height,
        // first: the cards have its shape, and it flies from where it shows.
        model.dismissKeyboard()
        model.panelOpen = false
        moves.waiting = true
        let turn = moves.turn
        // Once; not if the drawer, the keyboard or the keys came back
        // meanwhile, or the screen went.
        let go = {
            guard turn == moves.turn, moves.waiting else { return }
            moves.waiting = false
            guard moves.onScreen, !drawerOpen, edgePull == 0, model.inputView?.isFirstResponder != true, !model.panelOpen else { return }
            zoomOut()
        }
        if drawerOpen {
            // Once the screen has slid back over the drawer, not while it
            // shows short of where it is laid out (a busy main thread
            // starts the slide late); a slide that never says it is done
            // waits no longer than a second.
            var back = Transaction(animation: Self.slide)
            back.addAnimationCompletion(criteria: .logicallyComplete, go)
            withTransaction(back) { showTree = false }
            DispatchQueue.main.asyncAfter(deadline: .now() + 1, execute: go)
        } else {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: go)
        }
    }

    /// The terminal, where it shows uncovered, off into its card.
    private func zoomOut() {
        let running = (model.threads?.threads ?? []).filter(\.live)
        let live = running.first(where: \.current)
        moves.turn += 1
        moves.landed = false
        let turn = moves.turn
        if let live, let index = running.firstIndex(of: live), terminalFrame.height > 0,
           let shot = flight.snapshot(terminalFrame) {
            flight.cover(UIColor(OverviewScreen.backdrop), alpha: 1)
            flight.begin(with: shot, frame: terminalFrame, at: terminalFrame, radius: 0)
            flyIn(to: OverviewScreen.estimatedThumb(
                index: index, count: running.count, width: terminalFrame.width, aspect: terminalFrame.width / terminalFrame.height
            ))
            CATransaction.flush()
        }
        // The cards, after the flight is under way.
        DispatchQueue.main.async {
            guard turn == moves.turn else { return }
            model.refreshViews()
            model.keepScreens()
            liveThread = live?.id
            if let pane = live.flatMap(OverviewScreen.pane(of:)) { model.requestScreen(pane: pane) }
            moves.thumbs = [:]
            landingThumb = nil
            liveShown = false
            overviewOpen = true
            overviewVisible = true
            if flight.flying {
                // Under the cover already: gone at once, so the cover
                // fading off the cards shows no trace of it.
                terminalAlpha = 0
                withAnimation(.easeOut(duration: 0.3)) { cardsAlpha = 1 }
            } else {
                // Nothing flies over the live card: its picture shows.
                withAnimation(.easeOut(duration: 0.3)) {
                    terminalAlpha = 0
                    cardsAlpha = 1
                }
                moves.landed = true
                liveShown = true
            }
        }
        // Cards that never say where they are show all the same.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) {
            guard turn == moves.turn, overviewOpen, !liveShown, landingThumb == nil else { return }
            moves.landed = true
            liveShown = true
            flight.fadeOut(duration: 0.2) {}
        }
    }

    private func flyIn(to thumb: CGRect) {
        let turn = moves.turn
        flight.move(to: thumb, radius: OverviewScreen.thumbRadius, duration: 0.45) {
            guard turn == moves.turn else { return }
            moves.landed = true
            fadeIntoCard()
            // A picture that never comes (the server could not say) leaves
            // the card's own background there.
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
                if turn == moves.turn { fadeIntoCard(force: true) }
            }
        }
    }

    /// The card's picture under the terminal's, which fades off it.
    private func fadeIntoCard(force: Bool = false) {
        guard overviewOpen, moves.landed, !moves.closing, !liveShown, livePicture || force else { return }
        liveShown = true
        let turn = moves.turn
        DispatchQueue.main.async {
            if turn == moves.turn { flight.fadeOut(duration: 0.2) {} }
        }
    }

    /// A card's picture as a view to fly out of it, at once; drawn again
    /// at the terminal's size off the main thread and put in when it is,
    /// on the way.
    private func flyingPicture(of pane: Int) -> UIView? {
        guard let card = model.pictures[pane], terminalFrame.width > 0 else { return nil }
        let scale = displayScale
        let view = UIImageView(image: UIImage(cgImage: card, scale: scale, orientation: .up))
        view.contentMode = .scaleToFill
        view.backgroundColor = UIColor(model.screens[pane].flatMap { Color(hex: $0.background) } ?? model.background)
        model.paint(pane: pane, pixels: CGSize(width: terminalFrame.width * scale, height: terminalFrame.height * scale)) {
            [weak view] full in view?.image = UIImage(cgImage: full, scale: scale, orientation: .up)
        }
        return view
    }

    /// Back into the thread on show: its picture lifts off its card and
    /// grows to the screen as the cards go under the cover, and the
    /// terminal takes over (`flyBack`).
    private func closeOverview() {
        guard overviewOpen, !moves.closing else { return }
        moves.turn += 1
        moves.closing = true
        let turn = moves.turn
        let backdrop = UIColor(OverviewScreen.backdrop)
        if !moves.landed && flight.flying {
            // Still on its way in: it turns back.
            flight.fadeCover(to: 1, duration: 0.2)
            flight.move(to: terminalFrame, radius: 0, duration: 0.35) { flyBack(turn) }
        } else if let thumb = liveThumb, let pane = livePane, let picture = flyingPicture(of: pane) {
            flight.cover(backdrop, alpha: 0)
            flight.begin(with: picture, frame: terminalFrame, at: thumb, radius: OverviewScreen.thumbRadius)
            flight.fadeCover(to: 1, duration: 0.3)
            flight.move(to: terminalFrame, radius: 0, duration: 0.4) { flyBack(turn) }
        } else {
            fadeBack(turn)
            return
        }
        CATransaction.flush()
    }

    /// Nothing to fly: the terminal fades back over the cards, its bar
    /// back at once.
    private func fadeBack(_ turn: Int) {
        flight.end()
        overviewOpen = false
        withAnimation(.easeOut(duration: 0.25)) {
            terminalAlpha = 1
            cardsAlpha = 0
        } completion: {
            if turn == moves.turn { overviewClosed() }
        }
    }

    /// A card tapped. The thread on show comes back as `closeOverview`
    /// has it; another's picture leaves its card and grows to the screen,
    /// and the terminal takes over from it once it shows that thread
    /// (`revealPicked`).
    private func pick(_ thread: ThreadView) {
        guard overviewOpen, !moves.closing else { return }
        if thread.id == liveThread {
            closeOverview()
            return
        }
        if currentThreadId != thread.id { model.sideClick("thread", id: thread.id) }
        moves.turn += 1
        moves.closing = true
        moves.grown = false
        moves.picked = thread.id
        let turn = moves.turn
        guard let thumb = moves.thumbs[thread.id], let pane = OverviewScreen.pane(of: thread),
              let picture = flyingPicture(of: pane) else {
            // Its picture not drawn yet: none flies, not even the thread
            // on show's, and the terminal fades back once it shows this one.
            flight.end()
            moves.grown = true
            if currentThreadId == thread.id {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { revealPicked(force: false) }
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
                if turn == moves.turn { revealPicked(force: true) }
            }
            return
        }
        flight.cover(UIColor(OverviewScreen.backdrop), alpha: 0)
        flight.begin(with: picture, frame: terminalFrame, at: thumb, radius: OverviewScreen.thumbRadius)
        flight.fadeCover(to: 1, duration: 0.3)
        flight.move(to: terminalFrame, radius: 0, duration: 0.42) {
            guard turn == moves.turn else { return }
            moves.grown = true
            revealPicked(force: false)
        }
        CATransaction.flush()
        // A switch that never shows ends the wait all the same.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            if turn == moves.turn { revealPicked(force: true) }
        }
    }

    private func revealPicked(force: Bool) {
        guard let picked = moves.picked else { return }
        guard force || (moves.grown && currentThreadId == picked) else { return }
        moves.picked = nil
        flyBack(moves.turn)
    }

    /// The picture fills the screen: the terminal and its bar come back
    /// under it, the picture fades off it, and the cards are taken down
    /// after, out of sight under the terminal.
    private func flyBack(_ turn: Int) {
        guard turn == moves.turn else { return }
        guard flight.flying else { return fadeBack(turn) }
        terminalAlpha = 1
        cardsAlpha = 0
        overviewOpen = false
        DispatchQueue.main.async {
            guard turn == moves.turn else { return }
            flight.fadeOut(duration: 0.15) {
                if turn == moves.turn { overviewClosed() }
            }
        }
    }

    /// The cards are gone: the overview lets go of what it held, and what
    /// was set to run on its way in or out finds itself overtaken.
    private func overviewClosed() {
        moves.turn += 1
        moves.waiting = false
        flight.end()
        overviewOpen = false
        overviewVisible = false
        liveThread = nil
        liveShown = false
        moves.thumbs = [:]
        landingThumb = nil
        terminalAlpha = 1
        cardsAlpha = 0
        moves.landed = false
        moves.closing = false
        moves.grown = false
        moves.picked = nil
        model.releaseScreens()
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
            // Back from the background, the first moments of the dial back
            // show the last frame and a line over it (`statusCapsule`). It
            // is not live yet, so it is dimmed, and a tap on it goes nowhere,
            // as the card would have it.
            if connectionHeld {
                Color.black.opacity(0.3)
                    .contentShape(Rectangle())
                    .onTapGesture {}
                    .transition(.opacity)
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
                        if member.icon != nil {
                            TabIconCard(
                                core: model.core, id: member.icon, size: 15, busy: member.busy,
                                revision: model.iconsRevision
                            )
                        } else if member.busy {
                            ProgressView().controlSize(.mini)
                        }
                        Text(member.title.isEmpty ? "shell" : member.title)
                            .lineLimit(1)
                            .font(.caption2.weight(member.current ? .semibold : .regular))
                    }
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(member.current ? model.ink.opacity(nav.focused ? 0.22 : 0.12) : Color.clear)
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
        .foregroundColor(nav.focused ? model.ink : .gray)
        .padding(.horizontal, 6)
        .frame(width: min(nav.rect.width, maxWidth), height: nav.rect.height, alignment: .leading)
        // The bar's height is the core's: its text grows only so far.
        .dynamicTypeSize(...DynamicTypeSize.large)
        .background(model.background)
        .background(model.ink.opacity(nav.focused ? 0.1 : 0.04))
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

    /// Another device holds the terminal, or none does: a card of glass at
    /// the terminal's foot, out of the way of what it shows -- what holds
    /// it and what a tap does, then the button that takes it, the card's
    /// width. While the server is asked the button waits; refused, the card
    /// says so and offers to try again. The Android app draws the same.
    private func takeOverCard(_ card: Card) -> some View {
        let taking = card.state == "taking"
        return VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 12) {
                ZStack {
                    Circle().fill(model.ink.opacity(0.1))
                    switch card.state {
                    case "taking": Spinner(color: .accentColor).frame(width: 18, height: 18)
                    case "refused": DesktopIcon(name: "triangle-alert").foregroundColor(.orange)
                    case "free": DesktopIcon(name: "hand")
                    default: DesktopIcon(name: "monitor-smartphone")
                    }
                }
                .frame(width: 40, height: 40)
                VStack(alignment: .leading, spacing: 2) {
                    Text(card.title)
                        .font(.subheadline.weight(.semibold))
                        .lineLimit(2)
                    Text(card.hint)
                        .font(.footnote)
                        .foregroundColor(model.ink.opacity(0.62))
                        .lineLimit(2)
                }
                Spacer(minLength: 0)
            }
            Button { model.takeOver() } label: {
                Text(card.action)
                    .font(.body.weight(.semibold))
                    .foregroundColor(.white)
                    .frame(maxWidth: .infinity)
                    .frame(height: 44)
                    .background(Color.accentColor.opacity(taking ? 0.45 : 1), in: Capsule())
                    .contentShape(Capsule())
            }
            .buttonStyle(.plain)
            .disabled(taking)
        }
        .foregroundColor(model.ink)
        .padding(14)
        .frame(maxWidth: 520)
        .glassPanel(radius: 26)
        .padding(.horizontal, 12)
        .padding(.bottom, 12)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottom)
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

    /// The App's remark, "copied", or the first moments of a dial back from
    /// the background: a capsule of glass over the terminal's top, so the
    /// terminal keeps its size as it comes and goes. It turns while the
    /// connection is on its way back. Where the connection card is up, the
    /// card says how the connection is going: the App's sticky word on it
    /// is left to the card, and only passing remarks show here.
    private var statusCapsule: some View {
        let phase = ConnectionPhase(status: model.connection)
        let held = model.cardHeld && phase.map { !$0.isFailure } ?? false
        let carded = !held && (phase != nil || (model.reconnecting && !model.autoReconnect))
        let text = model.copied ? tr("copied")
            : held ? tr("card.reconnecting", hostName)
            : carded && model.toastSticky ? "" : (model.toastText ?? "")
        // A redial back within a moment says nothing (`redialQuiet`).
        let showing = !text.isEmpty && (model.copied || held || !model.redialQuiet)
        // It turns while the connection is on its way back by itself; over
        // a card, what it says is a remark, not the connection.
        let redialing = model.reconnecting && model.autoReconnect && !carded
        let turning = !model.copied && (held || redialing)
        return ZStack {
            if showing {
                HStack(spacing: 8) {
                    if model.copied {
                        Image(systemName: "checkmark").font(.footnote.weight(.semibold))
                    } else if turning {
                        Spinner(color: model.ink).frame(width: 14, height: 14)
                    }
                    Text(text)
                        .font(.footnote)
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                }
                .foregroundColor(model.ink)
                .padding(.horizontal, 14)
                .padding(.vertical, 9)
                .glassCapsule()
                .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
        .animation(.easeOut(duration: 0.2), value: showing)
        .allowsHitTesting(false)
    }
}

/// What a menu is about, for the sheet.
struct MenuSheet: Identifiable {
    var kind: String
    var id: String
    var title: String
    var key: String { kind + ":" + id }
}

/// A thread's mark as the desktop's sidebar draws it: the core's picture
/// (`threadMark`, from the look every client shares), and for a mark that
/// moves the picture of the step the wall clock is at, so every mark on
/// every screen steps together. Every state takes the same square, `size`
/// × 2, so a row keeps its width as the status changes; the tick of a
/// finished thread is the larger and reaches past it, as on the desktop.
struct ThreadMark: View {
    var status: String
    var dot: String
    /// Whether the thread has terminals (`ThreadRow.live`).
    var live = true
    var size: CGFloat
    @Environment(\.displayScale) private var scale
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let side = size * 2
        let mark = ThreadMarkImages.shared.mark(
            status: status, dot: dot, live: live, points: side, scale: scale, dark: scheme == .dark
        )
        Color.clear
            .frame(width: side, height: side)
            .overlay {
                if let mark {
                    if mark.frames.count > 1 {
                        TimelineView(.periodic(from: .now, by: mark.step)) { moment in
                            let step = Int(moment.date.timeIntervalSince1970 / mark.step)
                            Image(uiImage: mark.frames[step % mark.frames.count]).fixedSize()
                        }
                    } else {
                        Image(uiImage: mark.frames[0]).fixedSize()
                    }
                }
            }
    }
}

/// The desktop's and the page's busy mark: an open ring turning once in
/// 0.96 s, drawn to the size it is given.
struct Spinner: View {
    var color: Color
    @State private var turning = false

    var body: some View {
        Circle()
            .trim(from: 0, to: 0.75)
            .stroke(color, style: StrokeStyle(lineWidth: 1.6, lineCap: .round))
            .rotationEffect(.degrees(turning ? 360 : 0))
            // Started again each time it shows: a lazy list's row comes
            // back with its state, and an animation that ran out.
            .onAppear {
                turning = false
                withAnimation(.linear(duration: 0.96).repeatForever(autoreverses: false)) { turning = true }
            }
            .onDisappear { turning = false }
    }
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
            // Deleting a thread ends its programs: it is asked first.
            let deletes = item.id.hasPrefix("delete-thread:")
            return AnyView(
                Button(role: deletes ? .destructive : nil) {
                    if deletes { model.confirming = item.id } else { model.menuAction(item.id) }
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

    /// Stopped, with a reason to show: the card says so at once.
    var isFailure: Bool {
        if case .failed = self { return true }
        return false
    }
}

/// What a connection on its way says over the star sky (`StarSky`, under
/// it in `TerminalScreen`): the host, how far the connection has got, or
/// what stopped it and what to do about it, with the buttons at the foot
/// where a thumb is. The sky is night whatever the terminal's colours, so
/// the words are white.
struct ConnectionCard: View {
    enum Action { case retry, forgetKeyAndRetry, edit, back }

    @ObservedObject private var lang = AppLanguage.shared

    var phase: ConnectionPhase
    var hostName: String
    /// Where it is dialled: `user@host`, and the port when it is not 22.
    var endpoint: String
    var canEdit: Bool
    var act: (Action) -> Void

    var body: some View {
        // Upright there is room to spare and the page spreads out; on its
        // side a phone has not the height for all of it, and the page
        // scrolls rather than push its buttons off the screen.
        ViewThatFits(in: .vertical) {
            page(spread: true)
            ScrollView(.vertical, showsIndicators: false) {
                page(spread: false).padding(.top, 16)
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // Every touch is the page's: none reaches the terminal under it.
        .contentShape(Rectangle())
    }

    /// The words a little above the middle and the buttons at the foot
    /// (`spread`), or one after the other.
    private func page(spread: Bool) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            if spread { Spacer(minLength: 24) }
            words
            if spread {
                Spacer(minLength: 24)
                Spacer(minLength: 0)
            } else {
                Color.clear.frame(height: 28)
            }
            buttons
                .padding(.bottom, 20)
        }
        .multilineTextAlignment(.leading)
        .foregroundColor(.white)
        .frame(maxWidth: 520, alignment: .leading)
        .padding(.horizontal, 28)
        .frame(maxWidth: .infinity)
    }

    private var words: some View {
        VStack(alignment: .leading, spacing: 22) {
            mark
            VStack(alignment: .leading, spacing: 6) {
                Text(label)
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundColor(phase.isFailure ? .orange : .white.opacity(0.7))
                // One line, a long name made smaller rather than broken at
                // a hyphen.
                Text(hostName)
                    .font(.system(size: 34, weight: .bold))
                    .lineLimit(1)
                    .minimumScaleFactor(0.5)
                if !shownEndpoint.isEmpty {
                    Text(shownEndpoint)
                        .font(.system(size: 15, design: .monospaced))
                        .foregroundColor(.white.opacity(0.55))
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            details
        }
        // Stars pass behind the words: a soft shadow keeps them clear.
        .shadow(color: .black.opacity(0.6), radius: 8)
    }

    /// Where the connection is, over the host's name: the words of the
    /// card's sentence without the name (`conn.connecting`, "Connecting
    /// to"), so the name can stand large under them.
    private var label: String {
        switch phase {
        case .connecting: return tr("conn.connecting")
        case .attaching: return tr("conn.attaching")
        case .reconnecting: return tr("conn.reconnecting")
        case .failed: return tr("conn.failed")
        case .disconnected: return tr("conn.disconnected")
        }
    }

    /// The host's own address, or, for a dial with no saved host behind
    /// it, the one the status names.
    private var shownEndpoint: String {
        if endpoint.isEmpty, case .connecting(let target) = phase { return target }
        return endpoint
    }

    /// On the way the sky is the picture; stopped, a mark says why.
    @ViewBuilder private var mark: some View {
        switch phase {
        case .failed:
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.system(size: 34, weight: .semibold))
                .foregroundColor(.orange)
        case .disconnected:
            Image(systemName: "bolt.slash.fill")
                .font(.system(size: 32, weight: .semibold))
                .foregroundColor(.white.opacity(0.75))
        default:
            EmptyView()
        }
    }

    @ViewBuilder private var details: some View {
        switch phase {
        case .connecting, .reconnecting:
            steps(at: 0)
        case .attaching:
            steps(at: 1)
        case .failed(let reason):
            VStack(alignment: .leading, spacing: 14) {
                if let hint = Self.hint(for: reason) {
                    Text(hint)
                        .font(.system(size: 17))
                        .foregroundColor(.white.opacity(0.85))
                        .fixedSize(horizontal: false, vertical: true)
                }
                Text(reason)
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundColor(.white.opacity(0.6))
                    .lineLimit(4)
                    .textSelection(.enabled)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 11)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.white.opacity(0.08), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }
        case .disconnected:
            EmptyView()
        }
    }

    /// The two steps of a connection, one under the other: done, on its
    /// way (turning), or still to come.
    private func steps(at current: Int) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            step(tr("conn.step1"), state: current > 0 ? 2 : 1)
            step(tr("conn.step2"), state: current > 1 ? 2 : (current == 1 ? 1 : 0))
        }
        .padding(.top, 4)
    }

    /// `state`: 0 still to come, 1 on its way, 2 done.
    private func step(_ label: String, state: Int) -> some View {
        HStack(spacing: 12) {
            ZStack {
                switch state {
                case 2:
                    Image(systemName: "checkmark.circle.fill")
                        .font(.system(size: 20))
                        .foregroundColor(.green)
                case 1:
                    Spinner(color: .white).frame(width: 18, height: 18)
                default:
                    Circle().strokeBorder(.white.opacity(0.35), lineWidth: 1.5).frame(width: 18, height: 18)
                }
            }
            .frame(width: 22, height: 22)
            Text(label)
                .font(.system(size: 17))
                .foregroundColor(.white.opacity(state == 0 ? 0.4 : (state == 1 ? 1 : 0.7)))
                .lineLimit(1)
        }
    }

    @ViewBuilder private var buttons: some View {
        switch phase {
        case .failed(let reason):
            VStack(spacing: 10) {
                if reason.contains("key changed") && canEdit {
                    lead(tr("forgetretry")) { act(.forgetKeyAndRetry) }
                } else {
                    lead(tr("retry")) { act(.retry) }
                }
                HStack(spacing: 10) {
                    if canEdit { plain(tr("edithost")) { act(.edit) } }
                    plain(tr("back")) { act(.back) }
                }
            }
        case .disconnected:
            VStack(spacing: 10) {
                lead(tr("reconnect")) { act(.retry) }
                plain(tr("back")) { act(.back) }
            }
        default:
            plain(tr("back")) { act(.back) }
        }
    }

    /// The one that leads: glass in the desktop's blue.
    private func lead(_ label: String, _ action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(label)
                .font(.system(size: 17, weight: .semibold))
                .foregroundColor(.white)
                .frame(maxWidth: .infinity)
                .frame(height: 54)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .glassCapsule(tint: .accentColor)
    }

    private func plain(_ label: String, _ action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(label)
                .font(.system(size: 17, weight: .medium))
                .foregroundColor(.white)
                .frame(maxWidth: .infinity)
                .frame(height: 50)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .glassCapsule()
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

/// The tab strip's clip: its own width, with room above and below for the
/// glass to swell into as it is pressed.
private struct SideClip: Shape {
    func path(in rect: CGRect) -> Path { Path(rect.insetBy(dx: 0, dy: -16)) }
}

/// Its content at most `width` across, however much is offered: in a
/// scroll view, which offers none, a frame's limit alone sizes the frame
/// but leaves a long name whole and spilling out of it.
private struct AtMost: Layout {
    var width: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        guard let content = subviews.first else { return .zero }
        let ideal = content.sizeThatFits(.unspecified).width
        return content.sizeThatFits(ProposedViewSize(width: min(proposal.width ?? ideal, ideal, width), height: proposal.height))
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        subviews.first?.place(at: bounds.origin, proposal: ProposedViewSize(bounds.size))
    }
}

/// What the overview's moves keep to themselves, out of SwiftUI's state so
/// that keeping it redraws nothing: which move is the latest (a late end
/// belongs to an older one), whether the terminal's picture is in its
/// card, and the thread a tapped card is bringing back.
final class OverviewMoves {
    var turn = 0
    /// Each card's picture, by thread, in the screen's space: read as a
    /// flight sets off, not drawn from.
    var thumbs: [String: CGRect] = [:]
    var landed = false
    /// On the way back (`closeOverview`, `pick`): taps wait.
    var closing = false
    var grown = false
    var picked: String?
    /// An open waits for the drawer or the keyboard to go first.
    var waiting = false
    /// The screen is up: what was set to run on it runs only then.
    var onScreen = false
}

/// Whether tabs are scrolled off the strip's start, and lie past its end.
private struct StripEnds: Equatable {
    var before = false
    var after = false
}

/// Where the drag from the left edge under way set off, and whether it
/// pulls the drawer: decided once, as it sets off.
final class EdgeDrag {
    var start: CGPoint?
    var pulls = false
}
