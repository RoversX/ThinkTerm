import SwiftUI

/// The desktop's Live Overview as an overlay under the terminal: a card
/// per thread, grouped by project, two to a row. Each card is the
/// thread's name and state with its current tab's title,
/// an offline badge, and a thumbnail of its terminal in its colours: the
/// last rows of its current tab's pane, fetched from the server and
/// refreshed while the overview is up. The thread on show leaves its
/// thumbnail empty and reports the box's frame: the live terminal is
/// zoomed into it by the screen above.
struct OverviewScreen: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    @Binding var isPresented: Bool
    /// The thread on show's card and its thumbnail box, in the screen's space.
    @Binding var cardFrame: CardFrames?
    var liveThread: String?
    /// The live card's rows wait until the terminal has faded out of it.
    var livePreview: Bool = true
    /// A tapped card, with its frames: the terminal grows back out of it.
    var onPick: (ThreadView, CardFrames?) -> Void = { _, _ in }
    private let rows = 9
    private let columns = [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)]

    /// Every card's two frames, by thread; the live card's are reported.
    @State private var cardFrames: [String: CGRect] = [:]
    @State private var thumbFrames: [String: CGRect] = [:]

    private func frames(of id: String) -> CardFrames? {
        guard let card = cardFrames[id], let thumb = thumbFrames[id] else { return nil }
        return CardFrames(card: card, thumb: thumb)
    }

    private func report() {
        guard let live = liveThread, let frames = frames(of: live) else { return }
        if cardFrame != frames { cardFrame = frames }
    }

    private var threads: [ThreadView] { model.threads?.threads ?? [] }

    private var groups: [(project: String, threads: [ThreadView])] {
        var order: [String] = []
        var byProject: [String: [ThreadView]] = [:]
        for t in threads {
            if byProject[t.project] == nil { order.append(t.project) }
            byProject[t.project, default: []].append(t)
        }
        return order.map { ($0, byProject[$0] ?? []) }
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                if threads.isEmpty {
                    Text(tr("overview.none"))
                        .foregroundColor(.secondary)
                        .padding(.top, 60)
                } else {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        ForEach(groups, id: \.project) { group in
                            HStack(spacing: 6) {
                                Text(model.threads?.space ?? "")
                                Text("·").foregroundColor(.secondary)
                                Text(group.project)
                                Rectangle().fill(Color.white.opacity(0.08)).frame(height: 1)
                                Text("\(group.threads.count)").foregroundColor(.secondary)
                            }
                            .font(.caption)
                            .padding(.top, 10)
                            LazyVGrid(columns: columns, spacing: 10) {
                                ForEach(group.threads) { thread in
                                    card(thread)
                                }
                            }
                        }
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                }
            }
            // Pinching back in returns to the terminal.
            .gesture(MagnifyGesture().onEnded { value in
                if value.magnification > 1.25 { isPresented = false }
            })
        }
        .foregroundColor(.white)
        .background(Color(white: 0.06))
        .onAppear {
            model.refreshViews()
            refreshPreviews()
        }
        .onReceive(Timer.publish(every: 2, on: .main, in: .common).autoconnect()) { _ in
            refreshPreviews()
        }
    }

    /// Every live thread's current tab, asked for its last rows.
    private func refreshPreviews() {
        for thread in threads where thread.live {
            if let pane = previewPane(thread) {
                model.requestPreview(pane: pane, rows: rows)
            }
        }
    }

    private func previewPane(_ thread: ThreadView) -> Int? {
        (thread.tabs.first(where: { $0.current }) ?? thread.tabs.first)?.target
    }

    private func card(_ thread: ThreadView) -> some View {
        let live = thread.id == liveThread
        return Button {
            if !live { model.sideClick("thread", id: thread.id) }
            onPick(thread, frames(of: thread.id))
            isPresented = false
        } label: {
            VStack(alignment: .leading, spacing: 0) {
                ThreadCardHeader(thread: thread)
                ZStack(alignment: .topLeading) {
                    Text(preview(thread))
                        .font(.system(size: 7, design: .monospaced))
                        .lineSpacing(1)
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 7)
                        .opacity(!live || livePreview ? 1 : 0)
                    // The terminal itself lands over this, then fades into the
                    // same preview as every other card; the frame is kept so
                    // the screen above knows where.
                    Color.clear
                        .background(GeometryReader { geo in
                            Color.clear
                                .onAppear { thumbFrames[thread.id] = geo.frame(in: .named("screen")); report() }
                                .onChange(of: geo.frame(in: .named("screen"))) { _, f in thumbFrames[thread.id] = f; report() }
                        })
                }
                .frame(height: 92, alignment: .topLeading)
                .background(thread.live ? model.background : Color.black.opacity(0.4))
                .clipped()
            }
            .background(GeometryReader { geo in
                Color.clear
                    .onAppear { cardFrames[thread.id] = geo.frame(in: .named("screen")); report() }
                    .onChange(of: geo.frame(in: .named("screen"))) { _, f in cardFrames[thread.id] = f; report() }
            })
            .background(Color(red: 0.10, green: 0.11, blue: 0.13))
            .clipShape(RoundedRectangle(cornerRadius: 13))
            // The moving card draws its own border until it has landed.
            .overlay(RoundedRectangle(cornerRadius: 13).stroke(
                live ? (livePreview ? Color.accentColor : Color.clear) : Color.white.opacity(0.07),
                lineWidth: live ? 2 : 1
            ))
        }
        .buttonStyle(.plain)
    }

    /// The rows fetched for the thread's pane in their colours, trailing
    /// blank rows dropped, each cut at about a half-width card's worth.
    private func preview(_ thread: ThreadView) -> AttributedString {
        guard let pane = previewPane(thread), let fetched = model.previews[pane] else { return AttributedString("") }
        let kept = Array(fetched.reversed().drop(while: { $0.runs.isEmpty }).reversed().suffix(rows))
        var out = AttributedString()
        for (i, row) in kept.enumerated() {
            var used = 0
            for run in row.runs {
                let room = 38 - used
                if room <= 0 { break }
                var piece = AttributedString(String(run.text.prefix(room)))
                piece.foregroundColor = Color(hex: run.fg) ?? .white
                out += piece
                used += min(run.text.count, room)
            }
            if i < kept.count - 1 { out += AttributedString("\n") }
        }
        return out
    }
}

/// Where the live card is and where its thumbnail is, in the screen's space.
struct CardFrames: Equatable {
    var card: CGRect
    var thumb: CGRect
}

/// A card's top: the thread's dot and name, its tabs as dots, the current
/// tab's title, and an offline badge. Shared with the screen's zoom, which
/// carries the same header while the terminal shrinks.
struct ThreadCardHeader: View {
    let thread: ThreadView
    @ObservedObject private var lang = AppLanguage.shared

    /// One line: the dot and name, the current tab's title after them, and
    /// an offline badge at the end; the tabs as dots took a row of their own.
    var body: some View {
        let currentTab = thread.tabs.first(where: { $0.current }) ?? thread.tabs.first
        HStack(spacing: 7) {
            Circle().fill(TerminalScreen.threadColor(status: thread.status, live: thread.live)).frame(width: 7, height: 7)
            Text(thread.name).font(.system(size: 13.5, weight: .semibold)).lineLimit(1)
            Text(currentTab?.title ?? "")
                .font(.system(size: 10, design: .monospaced))
                .foregroundColor(.secondary)
                .lineLimit(1)
            Spacer(minLength: 0)
            if !thread.live {
                Text(tr("offline"))
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundColor(.secondary)
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .background(Color.gray.opacity(0.22))
                    .clipShape(Capsule())
            }
        }
        .padding(.horizontal, 10)
        .padding(.top, 9)
        .padding(.bottom, 8)
        .foregroundColor(.white)
    }
}
