import PhotosUI
import SwiftUI

// MARK: the chat as the core makes it (thinkterm_agent_chat::view::ChatView)

struct ChatState: Decodable {
    /// The pane the chat is with: a view of a chat shown before is told
    /// from one of the chat on show by it.
    var pane: Int
    var agent: String
    var title: String
    /// "connecting", "working", "idle", "blocked" or "problem".
    var state: String
    var state_label: String
    var since: Int64?
    var busy: String?
    var guessed: String?
    var rows: [ChatRow]
    var more: Bool
    var more_label: String
    var loading_older: Bool
    var ask: ChatAsk?
    var problem: ChatProblem?
    var notice: ChatNotice?
    var placeholder: String
    var can_send: Bool
    var can_stop: Bool
    /// Files and pictures can be sent along.
    var can_attach: Bool
    /// The model in use and the others; nil when the agent does not say.
    var model: ChatModelMenu?
    /// What a `/` (or `$`) typed in the field offers.
    var commands: [ChatCommand]?
    /// How full the model's context is: the ring in the bar and its sheet;
    /// nil when the agent does not say.
    var context: ChatContext?
    /// The agent runs an older ThinkTerm extension than the one in its
    /// folder: said over the field, with the button that loads the new one.
    var outdated: ChatOutdated?
    /// When each row came about (unix seconds), by row id, where known.
    var times: [String: Int64]?
    /// The replies still being written, by row id.
    var writing: [String]?
}

/// The agent runs an older ThinkTerm extension than the one in its
/// folder: what is said, and the button that loads the new one.
struct ChatOutdated: Decodable, Equatable {
    var text: String
    var action: ChatOutdatedAction
}

struct ChatOutdatedAction: Decodable, Equatable {
    var id: String
    var label: String
}

/// How full the context is (`fill` 0 to 1, nil while not known; `level`
/// "ok", "high" or "full"), what fills it, and the session's totals.
struct ChatContext: Decodable, Equatable {
    var fill: Double?
    var level: String
    var percent: String
    var heading: String
    var amount: String
    var parts: [ChatContextPart]
    var breakdown: String
    var compacts_at: String?
    var note: String?
    var totals_heading: String
    var totals: [ChatStat]
    var compact: String?
}

/// One part of the context: what it is ("system", "user", ... "free"), as
/// said, how much, and its share of the window.
struct ChatContextPart: Decodable, Equatable, Identifiable {
    var kind: String
    var label: String
    var amount: String
    var share: Double
    var percent: String
    var id: String { kind }
}

struct ChatStat: Decodable, Equatable, Identifiable {
    var label: String
    var value: String
    var id: String { label }
}

/// A command the field offers: as typed (`/compact`), what it does, what
/// may follow it (a pick then puts it in the field rather than run it),
/// `here` for one the app does itself ("model": the model menu), what it
/// is ("skill", "command", "prompt") and the heading of its group.
struct ChatCommand: Decodable, Hashable, Identifiable {
    var name: String
    var description: String
    var args: String?
    var here: String?
    var kind: String
    var group: String
    var id: String { name }

    private enum Key: String, CodingKey { case name, description, args, here, kind, group }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Key.self)
        name = try c.decode(String.self, forKey: .name)
        description = (try? c.decode(String.self, forKey: .description)) ?? ""
        args = try? c.decode(String.self, forKey: .args)
        here = try? c.decode(String.self, forKey: .here)
        // An older core says no kind: the name tells a skill.
        kind = (try? c.decode(String.self, forKey: .kind)) ?? (name.hasPrefix("$") || name.hasPrefix("/skill:") ? "skill" : "command")
        group = (try? c.decode(String.self, forKey: .group)) ?? ""
    }
}

/// The model button: what it says, and what it opens on -- the models,
/// each opening on how hard it can think.
struct ChatModelMenu: Decodable, Equatable {
    var label: String
    var models_heading: String
    var models: [ChatModelChoice]
    var efforts_heading: String
}

/// A model; `efforts` empty for one picked as it is.
struct ChatModelChoice: Decodable, Equatable, Identifiable {
    var id: String
    var label: String
    var current: Bool
    var efforts: [ChatModelOption]
}

struct ChatModelOption: Decodable, Equatable, Identifiable {
    var id: String
    var label: String
    var current: Bool
}

/// A picture or file sent with what was said: the key of its small
/// picture (`TerminalModel.chatThumbs`) when the host made one, and where
/// it is on the host.
struct ChatSent: Decodable, Hashable {
    var name: String
    var image: Bool
    var picture: String?
    var path: String?
}

enum ChatRow: Decodable, Identifiable {
    case user(id: String, text: String, attachments: [ChatSent])
    /// `copy`: the words as written, for the last reply of a turn.
    case text(id: String, blocks: [MdBlock], copy: String?)
    case fold(ChatFold)
    case note(id: String, text: String)
    case group(ChatGroup)

    var id: String {
        switch self {
        case let .user(id, _, _): return id
        case let .text(id, _, _): return id
        case let .note(id, _): return id
        case let .fold(fold): return fold.id
        case let .group(group): return group.id
        }
    }

    private enum Key: String, CodingKey { case kind, id, text, blocks, attachments, copy }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Key.self)
        let id = try c.decode(String.self, forKey: .id)
        switch try c.decode(String.self, forKey: .kind) {
        case "user":
            self = .user(
                id: id,
                text: try c.decode(String.self, forKey: .text),
                attachments: (try? c.decode([ChatSent].self, forKey: .attachments)) ?? []
            )
        case "text": self = .text(id: id, blocks: try c.decode([MdBlock].self, forKey: .blocks), copy: try? c.decode(String.self, forKey: .copy))
        case "fold": self = .fold(try ChatFold(from: decoder))
        case "group": self = .group(try ChatGroup(from: decoder))
        default: self = .note(id: id, text: (try? c.decode(String.self, forKey: .text)) ?? "")
        }
    }
}

/// What the agent did between two things said, as one line ("Ran 3
/// commands") that opens on its folds; `rows` only while open.
struct ChatGroup: Decodable {
    var id: String
    var icon: String
    var label: String
    /// For one change done, the lines it added and took out.
    var added: Int?
    var removed: Int?
    /// "running", "stopped", "done".
    var state: String
    var badge: String?
    var open: Bool
    var rows: [ChatRow]
}

struct ChatFold: Decodable {
    var id: String
    /// "think", "look", "run", "edit", "tool".
    var icon: String
    var label: String
    /// "running", "done", "failed", "declined", "stopped".
    var state: String
    var badge: String?
    var added: Int?
    var removed: Int?
    var open: Bool
    /// There is something to show when it opens; `body` is that, sent only
    /// while it is open.
    var can_open: Bool
    var body: FoldBody?
}

enum FoldBody: Decodable {
    case blocks([MdBlock])
    case output(String)
    case list([String])
    case diff([DiffFile])

    private enum Key: String, CodingKey { case t, blocks, text, lines, files }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Key.self)
        switch try c.decode(String.self, forKey: .t) {
        case "text": self = .blocks(try c.decode([MdBlock].self, forKey: .blocks))
        case "list": self = .list(try c.decode([String].self, forKey: .lines))
        case "diff": self = .diff(try c.decode([DiffFile].self, forKey: .files))
        default: self = .output((try? c.decode(String.self, forKey: .text)) ?? "")
        }
    }
}

struct DiffFile: Decodable, Hashable {
    var path: String
    var lines: [DiffLine]
}

/// `t`: "add", "del", "ctx" or "hunk".
struct DiffLine: Decodable, Hashable {
    var t: String
    var s: String
}

enum MdBlock: Decodable, Hashable {
    case para([MdSpan])
    case heading(Int, [MdSpan])
    case item(depth: Int, mark: String, [MdSpan])
    case quote([MdSpan])
    case code(lang: String, text: String)
    case rule
    /// How each column lines up ("left", "center", "right", "none"), the
    /// header's cells, then the rows'.
    case table(align: [String], head: [[MdSpan]], rows: [[[MdSpan]]])

    private enum Key: String, CodingKey { case t, spans, level, depth, mark, lang, text, align, head, rows }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Key.self)
        let spans = { (try? c.decode([MdSpan].self, forKey: .spans)) ?? [] }
        switch try c.decode(String.self, forKey: .t) {
        case "heading": self = .heading((try? c.decode(Int.self, forKey: .level)) ?? 1, spans())
        case "item":
            self = .item(
                depth: (try? c.decode(Int.self, forKey: .depth)) ?? 0,
                mark: (try? c.decode(String.self, forKey: .mark)) ?? "•",
                spans()
            )
        case "quote": self = .quote(spans())
        case "code":
            self = .code(lang: (try? c.decode(String.self, forKey: .lang)) ?? "", text: (try? c.decode(String.self, forKey: .text)) ?? "")
        case "rule": self = .rule
        case "table":
            self = .table(
                align: (try? c.decode([String].self, forKey: .align)) ?? [],
                head: (try? c.decode([[MdSpan]].self, forKey: .head)) ?? [],
                rows: (try? c.decode([[[MdSpan]]].self, forKey: .rows)) ?? []
            )
        default: self = .para(spans())
        }
    }
}

struct MdSpan: Decodable, Hashable {
    var text: String
    var bold: Bool?
    var italic: Bool?
    var code: Bool?
    var strike: Bool?
    var link: String?
    /// How much of its colour it shows, for words just come in.
    var fade: Double?
}

struct ChatAsk: Decodable {
    var id: String
    var label: String
    var question: String
    var body: String
    var reason: String?
    var place: String?
    var choices: [ChatChoice]
    /// With no choices: the label of the way to the terminal, where the
    /// agent takes its answer.
    var terminal: String?
}

struct ChatChoice: Decodable, Hashable {
    var id: String
    var label: String
    var lead: Bool
}

struct ChatProblem: Decodable {
    var text: String
    var detail: String
    var action: ChatAction?
}

struct ChatAction: Decodable {
    var id: String
    var label: String
}

struct ChatNotice: Decodable, Equatable {
    var seq: Int64
    var text: String
    /// What was typed and not sent: back in the field, to be put right.
    var kept: String?
}

// MARK: the page

/// How large the chat's text is drawn, as its setting says.
func chatTextScale(_ setting: String) -> CGFloat {
    switch setting {
    case "s": return 0.9
    case "l": return 1.12
    case "xl": return 1.25
    default: return 1
    }
}

/// The chat's text scale: its own setting, times what the system's text
/// size (Dynamic Type) makes of body text -- kept within twice, past which
/// a phone's width holds too few words.
func chatTextScale(_ setting: String, _ size: DynamicTypeSize) -> CGFloat {
    let traits = UITraitCollection(preferredContentSizeCategory: UIContentSizeCategory(size))
    let system = UIFontMetrics(forTextStyle: .body).scaledValue(for: 17, compatibleWith: traits) / 17
    return chatTextScale(setting) * min(max(system, 0.85), 2)
}

private struct ChatTextScaleKey: EnvironmentKey {
    static let defaultValue: CGFloat = 1
}

extension EnvironmentValues {
    /// How large the chat's text is drawn (`chatTextScale`).
    var chatTextScale: CGFloat {
        get { self[ChatTextScaleKey.self] }
        set { self[ChatTextScaleKey.self] = newValue }
    }
}

/// Whether a code block is being scrolled sideways: the swipe for the
/// times leaves the rows where they are meanwhile.
final class CodeScroll: @unchecked Sendable {
    var active = false
}

private struct CodeScrollKey: EnvironmentKey {
    static let defaultValue = CodeScroll()
}

extension EnvironmentValues {
    var codeScroll: CodeScroll {
        get { self[CodeScrollKey.self] }
        set { self[CodeScrollKey.self] = newValue }
    }
}

private let working = Color(red: 0.20, green: 0.78, blue: 0.35)
private let waiting = Color(red: 1.0, green: 0.62, blue: 0.04)

/// The chat with the agent of a pane, over that pane's terminal: what it
/// said and did, what it asks, and a field to say something. Going back --
/// the button, or a swipe in from the left edge -- is going to its terminal.
///
/// It watches the chat's own state (`feed`), not the whole model, which
/// changes with every refresh of the terminal under it: the page is drawn
/// again when the chat changes, and the field only when what it shows does.
struct ChatPage: View {
    /// What is asked of the chat goes here; what is shown comes from `feed`.
    let model: TerminalModel
    @ObservedObject var feed: TerminalModel.ChatFeed
    @State private var notice: ChatNotice?
    @State private var noticeSeen: Int64 = 0
    /// The end of the list is in view: what comes next is shown as it
    /// comes. Scrolled up to read, the list stays where it is.
    @State private var atEnd = true
    /// Just said something: what comes of it is shown, wherever the
    /// person was.
    @State private var follow = false
    /// A picture sent, shown big.
    @State private var shownPicture: ChatSent?
    /// The rows on show already: one not among them comes in as it is
    /// added, the first ones as they are.
    @State private var known: Set<String> = []
    /// The context's sheet, risen from the bottom.
    @State private var showContext = false
    /// The chat's text size is one of its settings.
    @ObservedObject private var settings = AppSettings.shared
    @Environment(\.colorScheme) private var scheme
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.accessibilityReduceMotion) private var still
    /// How much of each reply on its way is on show.
    @StateObject private var reveal = ChatReveal()
    /// Swiped left: the rows move aside for when each came about, and back
    /// once let go (or the touch is taken for something else).
    @GestureState(resetTransaction: Transaction(animation: .snappy(duration: 0.28))) private var swipe: CGFloat = 0
    @State private var codeScroll = CodeScroll()

    var body: some View {
        let chat = feed.chat
        let scale = chatTextScale(settings.chatText, typeSize)
        // A reply on its way shows as it is written, its words coming in at
        // a steady pace; or, set to blocks, only once written whole.
        let streaming = settings.chatReveal != "blocks"
        let writing = Set(chat?.writing ?? [])
        let rows = (chat?.rows ?? []).filter { row in
            if streaming { return true }
            if case .text = row { return !writing.contains(row.id) }
            return true
        }
        let texts: [(String, [MdBlock])] = rows.compactMap { row in
            if case let .text(id, blocks, _) = row { return (id, blocks) }
            return nil
        }
        // What a reply on its way has come to, counted for those only (the
        // last may just have been written whole): more of it, and the
        // reveal goes on.
        let lastText = texts.last?.0
        let wanted = texts.reduce(0) { sum, text in
            writing.contains(text.0) || text.0 == lastText || reveal.moving(text.0) ? sum + mdLength(text.1) : sum
        }
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    if let chat, chat.more {
                        HStack {
                            Spacer()
                            if chat.loading_older {
                                ProgressView().controlSize(.small)
                            } else {
                                Button(chat.more_label) { model.chatOlder() }
                                    .font(.footnote)
                                    .buttonStyle(.glass)
                            }
                            Spacer()
                        }
                    }
                    if let guessed = chat?.guessed {
                        Text(guessed)
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                            .frame(maxWidth: .infinity)
                    }
                    if chat == nil || (chat?.rows.isEmpty == true && chat?.state == "connecting") {
                        ProgressView()
                            .frame(maxWidth: .infinity)
                            .padding(.top, 80)
                    }
                    // Nothing said yet: whom the chat is with, where the words
                    // will be.
                    if let chat, chat.rows.isEmpty, chat.state != "connecting", chat.problem == nil, !chat.more {
                        VStack(spacing: 4) {
                            Image(systemName: "bubble.left.and.bubble.right")
                                .font(.system(size: 22))
                                .foregroundStyle(.secondary)
                                .frame(width: 52, height: 52)
                                .background(Circle().fill(Color.primary.opacity(0.06)))
                                .padding(.bottom, 8)
                            Text(chat.agent.isEmpty ? (feed.with?.agent ?? "") : chat.agent)
                                .font(.system(size: 18, weight: .semibold))
                            Text(feed.with?.place ?? "")
                                .font(.system(size: 13))
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                        }
                        .frame(maxWidth: .infinity)
                        .padding(.top, 180)
                        .transition(.opacity)
                    }
                    ForEach(rows) { row in
                        let shown = streaming && !still ? reveal.shownOf(row, writing: writing) : nil
                        rowView(row, shown: shown)
                            // The full width: its time sits at the right edge
                            // however short the row.
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .offset(x: swipe)
                            .overlay(alignment: .topTrailing) {
                                // When it came about, out of sight to its right until
                                // the rows are swiped aside.
                                if swipe < 0, let at = chat?.times?[row.id] {
                                    Text(chatTimeWords(at))
                                        .font(.system(size: 11))
                                        .foregroundStyle(.secondary)
                                        .multilineTextAlignment(.trailing)
                                        .frame(width: timeGutter - 8, alignment: .trailing)
                                        .padding(.top, 2)
                                        .offset(x: swipe + timeGutter)
                                        .opacity(Double(min(1, -swipe / timeGutter)))
                                        .allowsHitTesting(false)
                                }
                            }
                            .modifier(Entrance(fresh: !known.isEmpty && !known.contains(row.id)))
                            .id(row.id)
                    }
                    if let busy = chat?.busy {
                        BusyLine(text: busy, since: chat?.since).id("busy")
                    }
                    Color.clear.frame(height: 1).id("end")
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
            // A swipe to the left over the rows moves them aside for their
            // times; not one that scrolls a code block, nor one from the
            // left edge, which goes back.
            .simultaneousGesture(
                DragGesture(minimumDistance: 20, coordinateSpace: .global)
                    .updating($swipe) { drag, state, _ in
                        guard drag.startLocation.x >= 28, !codeScroll.active,
                              abs(drag.translation.width) > abs(drag.translation.height) else { return }
                        let dx = min(0, drag.translation.width)
                        // Past the times, it gives less and less.
                        state = dx < -timeGutter ? -timeGutter + (dx + timeGutter) / 3 : dx
                    }
            )
            .environment(\.codeScroll, codeScroll)
            .scrollDismissesKeyboard(.interactively)
            // What scrolls under the bars blurs and fades into them, softly:
            // a hard edge drew a line across and a band of frost over the
            // field.
            .scrollEdgeEffectStyle(.soft, for: .all)
            .defaultScrollAnchor(.bottom)
            .safeAreaBar(edge: .top, spacing: 0) { header(chat) }
            .safeAreaBar(edge: .bottom, spacing: 0) {
                foot(chat).overlay(alignment: .top) {
                    // Scrolled up to read: a way back down to what comes now.
                    if !atEnd && chat?.rows.isEmpty == false {
                        Button {
                            withAnimation(.smooth(duration: 0.3)) { proxy.scrollTo("end", anchor: .bottom) }
                        } label: {
                            Image(systemName: "arrow.down")
                                .font(.system(size: 15, weight: .semibold))
                                .foregroundStyle(.primary)
                                .frame(width: 40, height: 40)
                                .glassEffect(.regular.interactive(), in: .circle)
                                .frame(width: 46, height: 46)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel(tr("chat.latest"))
                        .offset(y: -50)
                        .transition(.scale(scale: 0.8).combined(with: .opacity))
                    }
                }
                .animation(.snappy(duration: 0.16), value: atEnd)
            }
            // Whether the person reads the end: as their last scroll (or
            // the last one made here) left the list, once it came to rest.
            // What comes in below does not change it: rows that came in a
            // burst once read as a scroll away from it.
            .onScrollPhaseChange { _, phase, context in
                switch phase {
                // The person's own scroll (a fling up from the end): what
                // comes in meanwhile does not pull the list back.
                case .interacting:
                    atEnd = false
                case .idle:
                    let geometry = context.geometry
                    atEnd = geometry.visibleRect.maxY >= geometry.contentSize.height - 120
                default:
                    break
                }
            }
            .onChange(of: chat?.rows.map(\.id) ?? []) { _, ids in
                known.formUnion(ids)
                reveal.prime(chat?.rows ?? [])
                reveal.prune(Set(ids))
            }
            // Replies on their way catch up with what has come of them, a
            // little each frame, the list following the end as they grow
            // when the person reads there. It rests once all have caught up,
            // until more comes.
            .task(id: streaming && !still ? wanted : -1) {
                guard streaming && !still else { return }
                var last = Date()
                while !Task.isCancelled {
                    try? await Task.sleep(for: .milliseconds(16))
                    let now = Date()
                    let seconds = min(0.1, now.timeIntervalSince(last))
                    last = now
                    guard reveal.step(texts, writing: writing, seconds: seconds) else { break }
                    if atEnd && !follow { proxy.scrollTo("end", anchor: .bottom) }
                }
            }
            .onChange(of: chat?.rows.last?.id) {
                guard atEnd || follow else { return }
                follow = false
                withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("end", anchor: .bottom) }
            }
            .onChange(of: chat?.busy) {
                guard atEnd || follow else { return }
                proxy.scrollTo("end", anchor: .bottom)
            }
        }
        // The chat's text, and the field's, as large as its setting and the
        // system's text size make it; the bars keep theirs.
        .environment(\.chatTextScale, scale)
        .background(Color.chromeBackdrop.ignoresSafeArea())
        .onAppear {
            known.formUnion(chat?.rows.map(\.id) ?? [])
            reveal.prime(chat?.rows ?? [])
        }
        .fullScreenCover(item: $shownPicture) { picture in
            PictureView(
                file: picture,
                thumb: picture.picture.flatMap { feed.thumbs[$0] },
                local: picture.path.flatMap { feed.pictures[$0] }
            ) { shownPicture = nil }
        }
        .onChange(of: chat?.notice) { _, new in
            guard let new, new.seq > noticeSeen else { return }
            noticeSeen = new.seq
            withAnimation { notice = new }
            Task {
                try? await Task.sleep(for: .seconds(4))
                withAnimation { if notice == new { notice = nil } }
            }
        }
        // A swipe in from the left edge goes back, as a pushed page's does.
        .simultaneousGesture(
            DragGesture(minimumDistance: 20, coordinateSpace: .global)
                .onEnded { drag in
                    if drag.startLocation.x < 28 && drag.translation.width > 80 && abs(drag.translation.height) < 80 {
                        model.closeChat()
                    }
                }
        )
    }

    // MARK: the bar

    private func header(_ chat: ChatState?) -> some View {
        let target = feed.with
        let dot: Color = switch chat?.state {
        case "working": working
        case "blocked": waiting
        default: .gray
        }
        let who = (chat?.agent).flatMap { $0.isEmpty ? nil : $0 } ?? target?.agent ?? ""
        let said = [who, chat?.state_label ?? "", chat?.title ?? ""].filter { !$0.isEmpty }.joined(separator: " · ")
        return HStack(spacing: 8) {
            // Drawn small; a finger's room around it all the same.
            Button {
                model.closeChat()
            } label: {
                Image(systemName: "chevron.left")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(.primary)
                    .frame(width: 40, height: 40)
                    .glassEffect(.regular.interactive(), in: .circle)
                    .frame(width: 46, height: 46)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(tr("back"))
            VStack(alignment: .leading, spacing: 1) {
                Text(target?.place ?? "")
                    .font(.system(size: 16, weight: .semibold))
                    .lineLimit(1)
                HStack(spacing: 5) {
                    Circle().fill(dot).frame(width: 6, height: 6)
                    Text(said)
                        .font(.system(size: 12.5))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 0)
            if let context = chat?.context {
                contextRing(context)
            }
        }
        .padding(.horizontal, 8)
        .padding(.bottom, 6)
    }

    /// How full the context is: a ring in a glass circle. A tap raises
    /// what it is made of.
    private func contextRing(_ context: ChatContext) -> some View {
        let fill = context.fill ?? 0
        return Button {
            showContext = true
        } label: {
            // A whole ring under the part filled, and how full in its
            // middle: a gauge, not a spinner.
            ZStack {
                Circle().stroke(Color.primary.opacity(0.26), lineWidth: 2.4)
                Circle()
                    .trim(from: 0, to: fill > 0 ? max(fill, 0.02) : 0)
                    .stroke(contextLevelColor(context.level), style: StrokeStyle(lineWidth: 2.4, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                Text({ let digits = context.percent.filter(\.isNumber); return digits.isEmpty ? "–" : digits }())
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(.primary.opacity(0.85))
                    .lineLimit(1)
                    .minimumScaleFactor(0.7)
            }
            .frame(width: 28, height: 28)
            .animation(.smooth(duration: 0.5), value: fill)
            .frame(width: 40, height: 40)
            .glassEffect(.regular.interactive(), in: .circle)
            .frame(width: 46, height: 46)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(context.heading): \(context.percent)")
        .sheet(isPresented: $showContext) {
            ContextSheet(feed: feed) {
                showContext = false
                model.chatCommand("/compact")
            }
            .presentationDetents([.medium, .large])
            .presentationDragIndicator(.visible)
        }
    }

    // MARK: what it said and did

    @ViewBuilder
    private func rowView(_ row: ChatRow, shown: Int? = nil) -> some View {
        switch row {
        case let .user(_, text, attachments):
            VStack(alignment: .trailing, spacing: 6) {
                if !attachments.isEmpty {
                    SentFiles(files: attachments, pictures: feed.pictures, thumbs: feed.thumbs) { shownPicture = $0 }
                }
                if !text.isEmpty {
                    HStack {
                        Spacer(minLength: 48)
                        Text(text)
                            .font(.system(size: 16 * chatTextScale(settings.chatText, typeSize)))
                            .lineSpacing(3)
                            .textSelection(.enabled)
                            .padding(.horizontal, 14)
                            .padding(.vertical, 9)
                            .background(
                                UnevenRoundedRectangle(topLeadingRadius: 20, bottomLeadingRadius: 20, bottomTrailingRadius: 6, topTrailingRadius: 20, style: .continuous)
                                    .fill(Color(uiColor: .secondarySystemFill))
                            )
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .trailing)
        case let .text(_, blocks, copy):
            // Grows as more of what it says comes, at once: an animated
            // growth a few times a second kept the list moving under the
            // words.
            VStack(alignment: .leading, spacing: 2) {
                // As far as it has come in: the newest words fading in.
                MarkdownBlocks(blocks: shown.map { mdHead(blocks, $0) } ?? blocks, whole: shown == nil ? nil : blocks)
                    .textSelection(.enabled)
                // The answer whole, as written.
                if let copy {
                    CopyButton(size: 14) { UIPasteboard.general.string = copy }
                }
            }
        case let .fold(fold):
            FoldView(fold: fold) { model.chatToggle(fold.id) }
        case let .group(group):
            GroupView(group: group, toggle: { model.chatToggle(group.id) }, toggleFold: { model.chatToggle($0) })
        case let .note(_, text):
            Text(text)
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: .infinity)
        }
    }

    // MARK: what it asks, and the field

    private func foot(_ chat: ChatState?) -> some View {
        // Glass pieces sample together, each keeping its own shape: the
        // commands over the field are two pieces, not one blob.
        GlassEffectContainer(spacing: 2) {
            VStack(spacing: 10) {
                if let notice {
                    Text(notice.text)
                        .font(.footnote)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .glassEffect(.regular, in: .capsule)
                        .transition(.opacity)
                }
                // Cards over the field rise in and drop away.
                if let outdated = chat?.outdated {
                    outdatedCard(outdated)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
                if let problem = chat?.problem {
                    problemCard(problem)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                } else if let ask = chat?.ask {
                    askCard(ask)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
                // Drawn again only when what it shows changes: not as rows
                // come, nor as the list scrolls.
                ChatComposer(
                    model: model,
                    pane: feed.with?.pane ?? -1,
                    placeholder: chat?.placeholder ?? "",
                    canSend: chat?.can_send == true,
                    canStop: chat?.can_stop == true,
                    canAttach: chat?.can_attach == true,
                    menu: chat?.model,
                    commands: chat?.commands ?? [],
                    files: feed.files,
                    notice: chat?.notice,
                    onSent: { follow = true }
                )
                .equatable()
            }
            .animation(.snappy(duration: 0.24), value: chat?.ask?.id)
            .animation(.snappy(duration: 0.24), value: chat?.problem?.text)
            .animation(.snappy(duration: 0.24), value: chat?.outdated != nil)
        }
        .padding(.horizontal, 10)
        .padding(.top, 8)
        .padding(.bottom, 6)
    }

    private func askCard(_ ask: ChatAsk) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(ask.label, systemImage: "exclamationmark.triangle")
                .font(.caption.weight(.semibold))
                .foregroundStyle(waiting)
            Text(ask.question)
                .font(.headline)
            if !ask.body.isEmpty {
                ScrollView {
                    Text(ask.body)
                        .font(.system(size: 13, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .textSelection(.enabled)
                }
                .frame(maxHeight: 140)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
                .background(Color.black.opacity(scheme == .dark ? 0.36 : 0.06), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }
            if let reason = ask.reason {
                Text(reason).font(.footnote).foregroundStyle(.secondary)
            }
            if let place = ask.place {
                Text(place).font(.caption).foregroundStyle(.secondary)
            }
            // Answered only in the agent's terminal, which is under the chat.
            if let terminal = ask.terminal {
                Button {
                    model.closeChat(toTerminal: true)
                } label: {
                    Text(terminal)
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .frame(height: 36)
                }
                .buttonStyle(.glassProminent)
                .padding(.top, 2)
            }
            if let lead = ask.choices.first(where: { $0.lead }) {
                Button {
                    model.chatAnswer(ask: ask.id, choice: lead.id)
                } label: {
                    Text(lead.label)
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .frame(height: 36)
                }
                .buttonStyle(.glassProminent)
                .padding(.top, 2)
            }
            let others = ask.choices.filter { !$0.lead }
            if !others.isEmpty {
                HStack(spacing: 10) {
                    ForEach(others, id: \.id) { choice in
                        Button {
                            model.chatAnswer(ask: ask.id, choice: choice.id)
                        } label: {
                            Text(choice.label)
                                .lineLimit(1)
                                .minimumScaleFactor(0.8)
                                .frame(maxWidth: .infinity)
                                .frame(height: 32)
                        }
                        .buttonStyle(.glass)
                    }
                }
            }
        }
        .padding(16)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 28, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 28, style: .continuous)
                .strokeBorder(waiting.opacity(0.4), lineWidth: 1)
        }
    }

    /// The agent runs an older extension than the one in its folder: a
    /// line over the field, and the button that has it take the new one in.
    private func outdatedCard(_ outdated: ChatOutdated) -> some View {
        HStack(spacing: 10) {
            Text(outdated.text)
                .font(.system(size: 13))
                .foregroundStyle(.primary.opacity(0.85))
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            Button(outdated.action.label) { model.chatAction(outdated.action.id) }
                .font(.system(size: 13.5, weight: .semibold))
                .buttonStyle(.glassProminent)
        }
        .padding(.leading, 14)
        .padding(.trailing, 6)
        .padding(.vertical, 6)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
    }

    private func problemCard(_ problem: ChatProblem) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(problem.text)
                .font(.subheadline)
                .fixedSize(horizontal: false, vertical: true)
            if !problem.detail.isEmpty {
                // Whoever wants the detail can copy it.
                ScrollView {
                    Text(problem.detail)
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .textSelection(.enabled)
                }
                .frame(maxHeight: 96)
                .fixedSize(horizontal: false, vertical: true)
            }
            if let action = problem.action {
                Button {
                    // The terminal is the app's to show; the rest the core's.
                    if action.id == "terminal" {
                        model.closeChat(toTerminal: true)
                    } else {
                        model.chatAction(action.id)
                    }
                } label: {
                    Text(action.label)
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .frame(height: 32)
                }
                .buttonStyle(.glassProminent)
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 24, style: .continuous))
    }
}

/// What each part of the context is drawn in.
private func contextPartColor(_ kind: String) -> Color {
    switch kind {
    case "system": return Color(red: 0.69, green: 0.32, blue: 0.87)
    case "tools": return Color(red: 0.37, green: 0.36, blue: 0.90)
    case "instructions": return Color(red: 0.39, green: 0.82, blue: 1.0)
    case "user": return Color(red: 0.04, green: 0.52, blue: 1.0)
    case "assistant": return Color(red: 0.19, green: 0.82, blue: 0.35)
    case "thinking": return Color(red: 1.0, green: 0.62, blue: 0.04)
    case "tool_use": return Color(red: 1.0, green: 0.22, blue: 0.37)
    case "summary": return Color(red: 1.0, green: 0.84, blue: 0.04)
    case "free": return Color.primary.opacity(0.1)
    default: return Color(red: 0.56, green: 0.56, blue: 0.58)
    }
}

/// The ring's colour: the ink while there is room, amber nearly full,
/// red full.
private func contextLevelColor(_ level: String) -> Color {
    switch level {
    case "full": return ChatColors.removed
    case "high": return ChatColors.waiting
    default: return Color.primary.opacity(0.85)
    }
}

/// How full the context is, risen from the bottom: a ring of its parts with
/// how full in its middle, when it compacts by itself, what the session has
/// used, and a way to compact now. Each part with how much opens on a tap
/// (of the ring, or the row that says so). It follows the chat while open.
private struct ContextSheet: View {
    @ObservedObject var feed: TerminalModel.ChatFeed
    let compact: () -> Void
    @State private var grown = false
    @State private var open = false

    var body: some View {
        if let context = feed.chat?.context {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    HStack(spacing: 18) {
                        ZStack {
                            Circle().stroke(Color.primary.opacity(0.08), lineWidth: 14)
                            ForEach(Array(segments(context).enumerated()), id: \.offset) { _, segment in
                                Circle()
                                    .trim(from: segment.from * (grown ? 1 : 0), to: segment.to * (grown ? 1 : 0))
                                    .stroke(contextPartColor(segment.kind), style: StrokeStyle(lineWidth: 14, lineCap: .butt))
                                    .rotationEffect(.degrees(-90))
                            }
                            Text(context.percent)
                                .font(.system(size: 24, weight: .semibold))
                                .foregroundStyle(contextLevelColor(context.level))
                        }
                        .frame(width: 112, height: 112)
                        .contentShape(Circle())
                        .onTapGesture {
                            guard !context.parts.isEmpty else { return }
                            withAnimation(.snappy(duration: 0.22)) { open.toggle() }
                        }
                        VStack(alignment: .leading, spacing: 3) {
                            Text(context.heading).font(.system(size: 20, weight: .semibold))
                            Text(context.amount).font(.system(size: 14)).foregroundStyle(.secondary)
                            if let at = context.compacts_at {
                                Text(at).font(.system(size: 13)).foregroundStyle(.secondary)
                            }
                        }
                        Spacer(minLength: 0)
                    }
                    if !context.parts.isEmpty {
                        VStack(spacing: 0) {
                            Button {
                                withAnimation(.snappy(duration: 0.22)) { open.toggle() }
                            } label: {
                                HStack {
                                    Text(context.breakdown).font(.system(size: 15))
                                    Spacer(minLength: 8)
                                    Image(systemName: "chevron.right")
                                        .font(.system(size: 13, weight: .semibold))
                                        .foregroundStyle(.secondary)
                                        .rotationEffect(.degrees(open ? 90 : 0))
                                }
                                .padding(.horizontal, 16)
                                .frame(minHeight: 48)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            if open {
                                ForEach(Array(context.parts.enumerated()), id: \.element.id) { at, part in
                                Divider().padding(.leading, at > 0 ? 40 : 16)
                                HStack(spacing: 12) {
                                    Circle().fill(contextPartColor(part.kind)).frame(width: 10, height: 10)
                                    Text(part.label).font(.system(size: 15)).lineLimit(1)
                                    Spacer(minLength: 8)
                                    Text(part.amount).font(.system(size: 14)).foregroundStyle(.secondary)
                                    Text(part.percent)
                                        .font(.system(size: 14).monospacedDigit())
                                        .frame(width: 44, alignment: .trailing)
                                }
                                .padding(.horizontal, 16)
                                .frame(minHeight: 44)
                                .transition(.opacity)
                                }
                            }
                        }
                        .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                    }
                    if open, let note = context.note {
                        Text(note).font(.system(size: 12.5)).foregroundStyle(.secondary)
                    }
                    if !context.totals.isEmpty {
                        VStack(alignment: .leading, spacing: 8) {
                            Text(context.totals_heading)
                                .font(.system(size: 13, weight: .semibold))
                                .foregroundStyle(.secondary)
                            LazyVGrid(columns: [GridItem(.flexible(), spacing: 8), GridItem(.flexible(), spacing: 8)], spacing: 8) {
                                ForEach(context.totals) { stat in
                                    VStack(alignment: .leading, spacing: 2) {
                                        Text(stat.label).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
                                        Text(stat.value).font(.system(size: 18, weight: .semibold))
                                    }
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .padding(.horizontal, 14)
                                    .padding(.vertical, 10)
                                    .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
                                }
                            }
                        }
                    }
                    if let label = context.compact {
                        Button(action: compact) {
                            Text(label)
                                .font(.system(size: 16, weight: .semibold))
                                .frame(maxWidth: .infinity)
                                .frame(height: 44)
                        }
                        .buttonStyle(.glass)
                    }
                }
                .padding(.horizontal, 20)
                .padding(.top, 24)
                .padding(.bottom, 28)
            }
            .onAppear {
                withAnimation(.smooth(duration: 0.7)) { grown = true }
            }
        }
    }

    /// Where each part (not what is free) starts and ends round the ring.
    private func segments(_ context: ChatContext) -> [(kind: String, from: Double, to: Double)] {
        var at = 0.0
        var out: [(kind: String, from: Double, to: Double)] = []
        for part in context.parts where part.kind != "free" && part.share > 0 {
            // A hair's gap between parts.
            let end = min(1, at + part.share)
            out.append((part.kind, at, max(at, end - 0.003)))
            at = end
        }
        return out
    }
}

/// The field to say something in, in one card with what goes with it:
/// the + for files and pictures, the model, and send -- or stop, while the
/// agent works and nothing is typed; with something typed, both. Over it,
/// as a `/` is typed, the commands the agent takes. Its draft is its own:
/// typing redraws the field, not the chat; and what is typed and not sent
/// is kept for the pane's chat, as it comes and goes.
private struct ChatComposer: View, Equatable {
    let model: TerminalModel
    let pane: Int
    let placeholder: String
    let canSend: Bool
    let canStop: Bool
    let canAttach: Bool
    let menu: ChatModelMenu?
    let commands: [ChatCommand]
    let files: [TerminalModel.ChatFile]
    /// What the core did not send (a command only the terminal takes) comes
    /// back with its notice, to be put right or sent again as words.
    let notice: ChatNotice?
    /// Something was said: the chat follows what comes of it.
    let onSent: () -> Void

    @State private var text = ""
    @State private var pickingPhotos = false
    @State private var pickingFiles = false
    @State private var photos: [PhotosPickerItem] = []
    /// Bumped on each thing said, for the tap felt.
    @State private var sent = 0
    @FocusState private var typing: Bool
    @Environment(\.chatTextScale) private var scale

    static func == (a: ChatComposer, b: ChatComposer) -> Bool {
        a.model === b.model && a.pane == b.pane && a.placeholder == b.placeholder && a.canSend == b.canSend && a.canStop == b.canStop
            && a.canAttach == b.canAttach && a.menu == b.menu && a.commands == b.commands && a.files == b.files
            && a.notice == b.notice
    }

    var body: some View {
        field
            .onAppear { if text.isEmpty { text = model.chatDrafts[pane] ?? "" } }
            .onChange(of: text) { _, now in model.chatDrafts[pane] = now.isEmpty ? nil : now }
            .onChange(of: notice?.seq) { _, _ in
                if let kept = notice?.kept, text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { text = kept }
            }
            .sensoryFeedback(.impact(weight: .light), trigger: sent)
            .photosPicker(
                isPresented: $pickingPhotos, selection: $photos, maxSelectionCount: 5,
                matching: .images, preferredItemEncoding: .compatible
            )
            .onChange(of: photos) { _, picked in
                guard !picked.isEmpty else { return }
                photos = []
                for item in picked { attachPhoto(item) }
            }
            .fileImporter(isPresented: $pickingFiles, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
                guard case .success(let urls) = result else { return }
                for url in urls { model.chatAttach(copying: url) }
            }
    }

    /// The field to say something in, in one card with what goes with it:
    /// the + for files and pictures, the model, and send -- or stop, while
    /// the agent works and nothing is typed. Over it, as a `/` is typed,
    /// the commands the agent takes.
    private var field: some View {
        let blank = text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        // What is picked goes when all of it is there.
        let waiting = files.contains { $0.path == nil && $0.error == nil }
        let ready = files.contains { $0.path != nil }
        let sendable = canSend && !waiting && (!blank || ready)
        let stop = canStop && blank && files.isEmpty
        let offer = commandOffer(commands, text: text)
        let card = RoundedRectangle(cornerRadius: 24, style: .continuous)
        return VStack(spacing: 8) {
            if let offer {
                CommandPalette(commands: offer.commands, menu: menu, pick: { pick($0, from: offer.from) }) { id, effort in
                    model.chatSetModel(id, effort: effort)
                    text = ""
                }
                .transition(
                    .asymmetric(
                        insertion: .opacity.combined(with: .scale(scale: 0.96, anchor: .bottom)).combined(with: .offset(y: 6)),
                        removal: .opacity.combined(with: .scale(scale: 0.98, anchor: .bottom))
                    )
                )
            }
            VStack(alignment: .leading, spacing: 0) {
                if !files.isEmpty {
                    PickedFiles(files: files, retry: { model.chatRetry($0) }) { model.chatDetach($0) }
                        .padding(.horizontal, 10)
                        .padding(.top, 10)
                        .transition(.opacity.combined(with: .move(edge: .bottom)))
                }
                TextField(placeholder, text: $text, axis: .vertical)
                    .lineLimit(1...6)
                    .font(.system(size: 16 * scale))
                    .focused($typing)
                    .padding(.horizontal, 14)
                    .padding(.top, 13)
                    .padding(.bottom, 3)
                HStack(spacing: 0) {
                    if canAttach {
                        attachMenu
                    }
                    if let menu {
                        modelChip(menu)
                    }
                    Spacer(minLength: 0)
                    // Something typed while it works: stopping stays at hand.
                    if canStop && !stop {
                        stopButton
                            .transition(.scale(scale: 0.8).combined(with: .opacity))
                    }
                    sendButton(canSend: sendable, stop: stop)
                }
                .animation(.snappy(duration: 0.16), value: canStop && !stop)
                .padding(.horizontal, 4)
                .padding(.bottom, 2)
            }
            // Glass, floating over the chat: what passes under it is blurred
            // and faded by the scroll edge.
            .glassEffect(.regular, in: card)
            .overlay { card.strokeBorder(Color.primary.opacity(0.08), lineWidth: 0.5) }
        }
        .animation(.snappy(duration: 0.18), value: offer?.commands.map(\.name))
        .animation(.snappy(duration: 0.22), value: files.count)
    }

    /// A command picked: one that runs as it is goes now, as typed; one
    /// followed by more is put in the field for it.
    private func pick(_ command: ChatCommand, from: String.Index) {
        if command.args == nil && from == text.startIndex {
            model.chatCommand(command.name)
            text = ""
            onSent()
            sent += 1
        } else {
            text = String(text[..<from]) + command.name + " "
            typing = true
        }
    }

    /// Stop, beside send while something is typed: the agent can be
    /// stopped without giving up what was typed.
    private var stopButton: some View {
        Button {
            model.chatInterrupt()
            sent += 1
        } label: {
            Image(systemName: "stop.fill")
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(.primary)
                .frame(width: 40, height: 40)
                .background(Circle().fill(Color.primary.opacity(0.12)))
                .frame(width: 46, height: 46)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(tr("chat.stop"))
    }

    private func sendButton(canSend: Bool, stop: Bool) -> some View {
        Button {
            if stop {
                model.chatInterrupt()
                sent += 1
            } else {
                model.chatSend(text.trimmingCharacters(in: .whitespacesAndNewlines))
                text = ""
                onSent()
                sent += 1
            }
        } label: {
            Image(systemName: stop ? "stop.fill" : "arrow.up")
                .font(.system(size: stop ? 12 : 15, weight: .bold))
                .foregroundStyle(stop ? Color(uiColor: .systemBackground) : canSend ? .white : .secondary)
                .contentTransition(.symbolEffect(.replace))
                .frame(width: 40, height: 40)
                .background(
                    Circle().fill(stop ? Color.primary : canSend ? Color.accentColor : Color.primary.opacity(0.1))
                )
                .frame(width: 46, height: 46)
                .contentShape(Rectangle())
                .animation(.easeOut(duration: 0.16), value: canSend)
                .animation(.easeOut(duration: 0.16), value: stop)
        }
        .buttonStyle(.plain)
        .disabled(!(stop || canSend))
        .accessibilityLabel(tr(stop ? "chat.stop" : "chat.send"))
    }

    /// The + beside the field: a photo, a file, or the clipboard's picture,
    /// put on the host to go with what is said next.
    private var attachMenu: some View {
        Menu {
            Button { pickingPhotos = true } label: { Label(tr("attach.photo"), systemImage: "photo.on.rectangle") }
            Button { pickingFiles = true } label: { Label(tr("attach.file"), systemImage: "folder") }
            if UIPasteboard.general.hasImages {
                Button {
                    if let data = UIPasteboard.general.image?.pngData() {
                        model.chatAttach(name: KeyBar.stamped("Pasted", "png"), data: data, image: true)
                    }
                } label: { Label(tr("attach.paste"), systemImage: "doc.on.clipboard") }
            }
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 16, weight: .semibold))
                .foregroundStyle(.primary)
                .frame(width: 40, height: 40)
                .background(Circle().fill(Color.primary.opacity(0.07)))
                .frame(width: 46, height: 46)
                .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .accessibilityLabel(tr("chat.attach"))
    }

    private func attachPhoto(_ item: PhotosPickerItem) {
        Task {
            guard let data = try? await item.loadTransferable(type: Data.self) else {
                await MainActor.run { model.note(tr("upload.unread", tr("attach.photo"))) }
                return
            }
            let named = item.supportedContentTypes.first?.preferredFilenameExtension ?? "jpg"
            let name = KeyBar.stamped("Photo", KeyBar.pictureExtension(data, else: named))
            await MainActor.run { model.chatAttach(name: name, data: data, image: true) }
        }
    }

    /// The model in use, in the field beside send. A tap opens on the
    /// models; one that can think more or less opens on how hard, a menu
    /// of its own.
    private func modelChip(_ menu: ChatModelMenu) -> some View {
        Menu {
            ModelMenu(menu: menu) { id, effort in model.chatSetModel(id, effort: effort) }
        } label: {
            HStack(spacing: 4) {
                Text(menu.label)
                    .font(.system(size: 13.5))
                    .foregroundStyle(.primary.opacity(0.8))
                    .lineLimit(1)
                Image(systemName: "chevron.down")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 8)
            .frame(maxWidth: 210, minHeight: 44)
            .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .sensoryFeedback(.selection, trigger: menu.label)
        .accessibilityLabel("\(tr("chat.model")): \(menu.label)")
    }
}

/// The models, each that thinks more or less opening on how hard: what
/// the model chip and `/model` open on.
private struct ModelMenu: View {
    let menu: ChatModelMenu
    let set: (String, String?) -> Void

    var body: some View {
        Section(menu.models_heading) {
            ForEach(menu.models) { choice in
                if choice.efforts.isEmpty {
                    Button {
                        if !choice.current { set(choice.id, nil) }
                    } label: {
                        if choice.current { Label(choice.label, systemImage: "checkmark") } else { Text(choice.label) }
                    }
                } else {
                    Menu {
                        Section(menu.efforts_heading) {
                            ForEach(choice.efforts) { effort in
                                Button {
                                    if !effort.current { set(choice.id, effort.id) }
                                } label: {
                                    if effort.current { Label(effort.label, systemImage: "checkmark") } else { Text(effort.label) }
                                }
                            }
                        }
                    } label: {
                        if choice.current { Label(choice.label, systemImage: "checkmark") } else { Text(choice.label) }
                    }
                }
            }
        }
    }
}

/// The word being typed, when it calls up commands: `/` at the start of
/// the field, which offers them all; or, anywhere, a word begun with a
/// skill's own sign (`$` for Codex, `@` for OpenCode), which offers those.
/// `from` is where the word begins: a pick takes its place.
private struct CommandOffer {
    let from: String.Index
    let commands: [ChatCommand]
}

/// A command's name without its sign: what is matched against what is
/// typed (`/skill:review` and `$review` are both "review").
private func bareName(_ name: String) -> String {
    var bare = name.dropFirst()
    if bare.hasPrefix("skill:") { bare = bare.dropFirst(6) }
    return bare.lowercased()
}

private func commandOffer(_ commands: [ChatCommand], text: String) -> CommandOffer? {
    guard !commands.isEmpty else { return nil }
    let from = text.hasPrefix("/") ? text.startIndex : (text.lastIndex(where: \.isWhitespace).map { text.index(after: $0) } ?? text.startIndex)
    let word = text[from...]
    guard let sign = word.first, !word.contains(where: \.isWhitespace) else { return nil }
    let signs = Set(commands.filter { $0.kind == "skill" }.compactMap(\.name.first)).subtracting(["/"])
    let offered: [ChatCommand]
    if sign == "/" && from == text.startIndex {
        offered = commands
    } else if signs.contains(sign) {
        offered = commands.filter { $0.name.first == sign }
    } else {
        return nil
    }
    let typed = String(word.dropFirst()).lowercased()
    // Those whose names begin with it, then those whose names hold it, then
    // those whose words on what they do hold it -- each group kept whole,
    // and in its place.
    func rank(_ command: ChatCommand) -> Int? {
        if typed.isEmpty { return 0 }
        let bare = bareName(command.name)
        if bare.hasPrefix(typed) || command.name.lowercased().hasPrefix(word.lowercased()) { return 0 }
        if bare.contains(typed) { return 1 }
        if command.description.lowercased().contains(typed) { return 2 }
        return nil
    }
    var groups: [String] = []
    for command in offered where !groups.contains(command.group) { groups.append(command.group) }
    let ranked = offered.enumerated().compactMap { at, command in rank(command).map { (command, $0, at) } }
    let all = ranked.sorted {
        let (a, b) = (groups.firstIndex(of: $0.0.group) ?? 0, groups.firstIndex(of: $1.0.group) ?? 0)
        return a != b ? a < b : $0.1 != $1.1 ? $0.1 < $1.1 : $0.2 < $1.2
    }.map(\.0)
    return all.isEmpty ? nil : CommandOffer(from: from, commands: all)
}

/// The commands the word typed may be, over the field, by group: a tap
/// runs one that runs as it is, or puts one in the field to be followed.
private struct CommandPalette: View {
    let commands: [ChatCommand]
    let menu: ChatModelMenu?
    let pick: (ChatCommand) -> Void
    let setModel: (String, String?) -> Void

    private let rowHeight: CGFloat = 54
    private let headingHeight: CGFloat = 30

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 22, style: .continuous)
        let headed = Set(commands.map(\.group)).count > 1
        let headings = headed ? Set(commands.map(\.group)).count : 0
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(commands.enumerated()), id: \.element.id) { at, command in
                        if headed && !command.group.isEmpty && (at == 0 || commands[at - 1].group != command.group) {
                            Text(command.group)
                                .font(.system(size: 12, weight: .semibold))
                                .foregroundStyle(.secondary)
                                .padding(.horizontal, 16)
                                .frame(height: headingHeight, alignment: .bottomLeading)
                                .accessibilityAddTraits(.isHeader)
                        }
                        if command.here == "model", let menu {
                            Menu {
                                ModelMenu(menu: menu, set: setModel)
                            } label: {
                                CommandRow(command: command).frame(height: rowHeight)
                            }
                            .menuStyle(.button)
                            .buttonStyle(.plain)
                        } else {
                            Button {
                                pick(command)
                            } label: {
                                CommandRow(command: command).frame(height: rowHeight)
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
                .padding(.vertical, 6)
                .id("top")
            }
            .scrollBounceBehavior(.basedOnSize)
            .frame(height: min(CGFloat(commands.count) * rowHeight + CGFloat(headings) * headingHeight + 12, 300))
            // A new word offers from its top.
            .onChange(of: commands.first?.id) { proxy.scrollTo("top", anchor: .top) }
        }
        .glassEffect(.regular, in: shape)
        .overlay { shape.strokeBorder(Color.primary.opacity(0.08), lineWidth: 0.5) }
    }
}

private struct CommandRow: View {
    let command: ChatCommand

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: symbol)
                .font(.system(size: 14, weight: .medium))
                .foregroundStyle(.secondary)
                .frame(width: 30, height: 30)
                .background(RoundedRectangle(cornerRadius: 9, style: .continuous).fill(Color.primary.opacity(0.07)))
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text(command.name)
                        .font(.system(size: 14.5, weight: .semibold))
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    if let args = command.args, !args.isEmpty {
                        Text(args)
                            .font(.system(size: 13))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                }
                if !command.description.isEmpty {
                    Text(command.description)
                        .font(.system(size: 12.5))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .contentShape(Rectangle())
    }

    private var symbol: String {
        if command.here == "model" { return "cpu" }
        if command.kind == "skill" { return "sparkles" }
        if command.kind == "prompt" { return "doc.text" }
        return "slash.circle"
    }
}

/// How far a left swipe moves the rows aside, for their times.
private let timeGutter: CGFloat = 64

/// When something came about, as the app's language words it: the time,
/// and the day above it when not today.
private func chatTimeWords(_ at: Int64) -> String {
    let then = Date(timeIntervalSince1970: TimeInterval(at))
    let locale = Locale(identifier: L10n.currentTag)
    let time = then.formatted(Date.FormatStyle(date: .omitted, time: .shortened).locale(locale))
    let calendar = Calendar.current
    if calendar.isDateInToday(then) { return time }
    let sameYear = calendar.component(.year, from: then) == calendar.component(.year, from: Date())
    let day = sameYear
        ? then.formatted(Date.FormatStyle().month(.abbreviated).day().locale(locale))
        : then.formatted(Date.FormatStyle(date: .abbreviated, time: .omitted).locale(locale))
    return day + "\n" + time
}

/// How many characters at the growing end of a reply fade in.
private let fading = 24
/// The slowest a reply comes in, in characters a second; and, faster, all
/// it is behind over this many seconds, again each frame: a burst comes in
/// quickly at first and eases as it catches up.
private let revealPace = 90.0
private let revealBehind = 0.6

/// How much of each reply on its way is on show. Only one begun while the
/// chat is open is stepped (from nothing), until it has caught up and is
/// written whole; the rest show whole and are not counted.
@MainActor
final class ChatReveal: ObservableObject {
    @Published private var shown: [String: Int] = [:]
    private var carry: [String: Double] = [:]
    private var initial: Set<String> = []
    private var primed = false

    func prime(_ rows: [ChatRow]) {
        guard !primed, !rows.isEmpty else { return }
        primed = true
        initial = Set(rows.map(\.id))
    }

    /// What the core let go of is let go here too.
    func prune(_ ids: Set<String>) {
        let gone = shown.keys.filter { !ids.contains($0) }
        guard !gone.isEmpty else { return }
        for id in gone {
            shown[id] = nil
            carry[id] = nil
        }
    }

    /// Whether the reply `id` is being stepped: what it has come to counts.
    func moving(_ id: String) -> Bool {
        shown[id] != nil
    }

    private func begins(_ id: String, writing: Set<String>) -> Bool {
        primed && writing.contains(id) && !initial.contains(id)
    }

    func shownOf(_ row: ChatRow, writing: Set<String>) -> Int? {
        guard case let .text(id, blocks, _) = row else { return nil }
        guard shown[id] != nil || begins(id, writing: writing) else { return nil }
        let at = shown[id] ?? 0
        return at < mdLength(blocks) ? at : nil
    }

    /// One frame on: each reply behind what came of it catches up a
    /// little. False when none is behind.
    func step(_ texts: [(String, [MdBlock])], writing: Set<String>, seconds: Double) -> Bool {
        var moved = false
        var next = shown
        for (id, blocks) in texts {
            guard next[id] != nil || begins(id, writing: writing) else { continue }
            let full = mdLength(blocks)
            let at = next[id] ?? 0
            if at >= full {
                // Caught up: let go once no more is to come of it.
                if !writing.contains(id) {
                    next[id] = nil
                    carry[id] = nil
                } else if next[id] != full {
                    next[id] = full
                }
                continue
            }
            let pace = max(revealPace, Double(full - at) / revealBehind)
            let gone = (carry[id] ?? 0) + pace * seconds
            let whole = Int(gone)
            carry[id] = gone - Double(whole)
            next[id] = min(full, at + whole)
            moved = true
        }
        if next != shown { shown = next }
        return moved
    }
}

/// How many characters `blocks` show: what a reply's growth is counted in.
func mdLength(_ blocks: [MdBlock]) -> Int {
    blocks.reduce(0) { $0 + mdLength($1) }
}

private func spansLength(_ spans: [MdSpan]) -> Int {
    spans.reduce(0) { $0 + $1.text.count }
}

func mdLength(_ block: MdBlock) -> Int {
    switch block {
    case let .para(spans), let .heading(_, spans), let .item(_, _, spans), let .quote(spans): return spansLength(spans)
    case let .code(_, text): return text.count
    case .rule: return 1
    case let .table(_, head, rows): return ([head] + rows).reduce(0) { $0 + $1.reduce(0) { $0 + spansLength($1) } }
    }
}

/// The first `n` characters of `blocks`, the newest of them fading in.
func mdHead(_ blocks: [MdBlock], _ n: Int) -> [MdBlock] {
    var out: [MdBlock] = []
    var left = n
    for block in blocks {
        guard left > 0 else { break }
        let size = mdLength(block)
        let whole = size <= left
        switch block {
        case let .para(spans): out.append(.para(spansHead(spans, left)))
        case let .heading(level, spans): out.append(.heading(level, spansHead(spans, left)))
        case let .item(depth, mark, spans): out.append(.item(depth: depth, mark: mark, spansHead(spans, left)))
        case let .quote(spans): out.append(.quote(spansHead(spans, left)))
        case let .code(lang, text): out.append(whole ? block : .code(lang: lang, text: String(text.prefix(left))))
        case .rule: out.append(block)
        // A table comes in row by row.
        case let .table(align, head, rows):
            if whole {
                out.append(block)
            } else {
                var room = left - head.reduce(0) { $0 + spansLength($1) }
                var kept: [[[MdSpan]]] = []
                for row in rows where room > 0 {
                    room -= row.reduce(0) { $0 + spansLength($1) }
                    kept.append(row)
                }
                out.append(.table(align: align, head: head, rows: kept))
            }
        }
        left -= size
    }
    return out
}

/// `spans` cut to `n` characters, the last of them fading in: the newest
/// faintest.
private func spansHead(_ spans: [MdSpan], _ n: Int) -> [MdSpan] {
    var cut: [MdSpan] = []
    var left = n
    for span in spans {
        guard left > 0 else { break }
        var piece = span
        if span.text.count > left { piece.text = String(span.text.prefix(left)) }
        cut.append(piece)
        left -= span.text.count
    }
    let total = spansLength(cut)
    guard total > 0, n < spansLength(spans) else { return cut }
    // Three bands at the end, each fainter than the one before it.
    let bands = [total - fading, total - fading * 2 / 3, total - fading / 3, total]
    let fades = [1.0, 0.75, 0.5, 0.25]
    var out: [MdSpan] = []
    var at = 0
    for span in cut {
        let characters = Array(span.text)
        let start = at
        let end = at + characters.count
        at = end
        var from = start
        while from < end {
            let band = bands.firstIndex { $0 > from } ?? 0
            let to = min(end, max(bands[band], from + 1))
            var piece = span
            piece.text = String(characters[(from - start)..<(to - start)])
            piece.fade = min(span.fade ?? 1, fades[band])
            out.append(piece)
            from = to
        }
    }
    return out
}

/// A row added while the chat is on show comes in: up a little, from
/// nothing. Rows there already, and all under Reduce Motion, are as
/// they are.
private struct Entrance: ViewModifier {
    @State private var hidden: Bool
    @Environment(\.accessibilityReduceMotion) private var still

    init(fresh: Bool) {
        _hidden = State(initialValue: fresh)
    }

    func body(content: Content) -> some View {
        content
            .opacity(hidden ? 0 : 1)
            .offset(y: hidden && !still ? 8 : 0)
            .onAppear {
                guard hidden else { return }
                withAnimation(.smooth(duration: 0.3)) { hidden = false }
            }
    }
}

/// What is picked to go next, over the field: pictures small, files by
/// name and size; each on its way shows how far, and each can be taken off.
private struct PickedFiles: View {
    let files: [TerminalModel.ChatFile]
    /// A file that did not go: sent again, and why said.
    let retry: (TerminalModel.ChatFile) -> Void
    let remove: (TerminalModel.ChatFile) -> Void

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 10) {
                ForEach(files) { file in
                    ZStack(alignment: .topTrailing) {
                        ZStack {
                            if let preview = file.preview {
                                Image(uiImage: preview)
                                    .resizable()
                                    .scaledToFill()
                                    .frame(width: 64, height: 64)
                                    .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                            } else {
                                FileCard(name: file.name, under: ByteCountFormatter.string(fromByteCount: file.size, countStyle: .file), image: file.image)
                                    .frame(height: 64)
                            }
                            if let error = file.error {
                                Button { retry(file) } label: {
                                    ZStack {
                                        RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.red.opacity(0.45))
                                        Image(systemName: "arrow.clockwise").font(.system(size: 16, weight: .semibold)).foregroundStyle(.white)
                                    }
                                }
                                .buttonStyle(.plain)
                                .accessibilityLabel("\(file.name): \(error)")
                                .accessibilityHint(tr("chat.attach.retry"))
                            } else if file.path == nil {
                                RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.black.opacity(0.35))
                                ProgressView(value: file.total > 0 ? Double(file.sent) / Double(file.total) : 0)
                                    .progressViewStyle(.circular)
                                    .tint(.white)
                            }
                        }
                        Button { remove(file) } label: {
                            Image(systemName: "xmark")
                                .font(.system(size: 10, weight: .bold))
                                .frame(width: 22, height: 22)
                                .background(Circle().fill(Color(uiColor: .systemGray3)))
                        }
                        .buttonStyle(.plain)
                        .offset(x: 8, y: -8)
                        .accessibilityLabel(tr("chat.attach.remove"))
                    }
                    .padding(.top, 8)
                    .padding(.trailing, 8)
                }
            }
        }
    }
}

/// A file as a card: its kind, its name, and a word under it (its size).
private struct FileCard: View {
    let name: String
    let under: String?
    let image: Bool

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: image ? "photo" : "doc.text")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(name).font(.footnote).lineLimit(1).truncationMode(.middle)
                if let under { Text(under).font(.caption).foregroundStyle(.secondary) }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .frame(minWidth: 120, maxWidth: 220, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color(uiColor: .secondarySystemFill)))
    }
}

/// What was sent with something said: its pictures, small, a tap showing
/// one big; its files as cards.
private struct SentFiles: View {
    let files: [ChatSent]
    let pictures: [String: URL]
    let thumbs: [String: UIImage]
    let show: (ChatSent) -> Void

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(Array(files.enumerated()), id: \.offset) { _, file in
                    if file.image, let picture = picture(file, side: 480) {
                        Button { show(file) } label: {
                            Image(uiImage: picture)
                                .resizable()
                                .scaledToFill()
                                .frame(width: 132, height: 132)
                                .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel(file.name)
                    } else if let path = file.path {
                        // Where it is on the host, to be pasted wherever it is wanted.
                        Button { UIPasteboard.general.string = path } label: {
                            FileCard(name: file.name, under: nil, image: file.image)
                        }
                        .buttonStyle(.plain)
                        .accessibilityHint(tr("copy"))
                    } else {
                        FileCard(name: file.name, under: nil, image: file.image)
                    }
                }
            }
        }
        .defaultScrollAnchor(.trailing)
    }

    private func picture(_ file: ChatSent, side: CGFloat) -> UIImage? {
        chatPicture(thumb: file.picture.flatMap { thumbs[$0] }, local: file.path.flatMap { pictures[$0] }, side: side)
    }
}

/// A picture to show: the phone's own copy when it sent it, else the
/// host's small one.
private func chatPicture(thumb: UIImage?, local: URL?, side: CGFloat) -> UIImage? {
    if let local, let image = UIImage(contentsOfFile: local.path) {
        return image.preparingThumbnail(of: CGSize(width: side, height: side)) ?? image
    }
    return thumb
}

extension ChatSent: Identifiable {
    var id: String { (path ?? "") + name + (picture ?? "") }
}

/// A small copy button: its symbol turns to a check for a moment once it
/// has copied.
struct CopyButton: View {
    let size: CGFloat
    let copy: () -> Void
    @State private var done = false

    var body: some View {
        Button {
            copy()
            done = true
            Task {
                try? await Task.sleep(for: .seconds(1.5))
                done = false
            }
        } label: {
            Image(systemName: done ? "checkmark" : "doc.on.doc")
                .font(.system(size: size, weight: .medium))
                .foregroundStyle(done ? AnyShapeStyle(Color.green) : AnyShapeStyle(.secondary))
                .contentTransition(.symbolEffect(.replace))
                .frame(width: size + 22, height: size + 22)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .sensoryFeedback(.success, trigger: done) { _, now in now }
        .accessibilityLabel(tr(done ? "copied" : "copy"))
    }
}

/// A picture over everything, as large as it fits. Two fingers zoom it, a
/// double tap zooms in or back; a tap puts it away. Share hands it to
/// another app (to save it, or send it on).
private struct PictureView: View {
    let file: ChatSent
    let thumb: UIImage?
    let local: URL?
    let close: () -> Void
    @State private var zoom: CGFloat = 1
    @GestureState private var pinch: CGFloat = 1
    @State private var offset: CGSize = .zero
    @GestureState private var drag: CGSize = .zero
    /// The phone's own copy, made ready once: not again with each step of
    /// a pinch.
    @State private var own: UIImage?

    var body: some View {
        let picture = own ?? thumb
        let scale = max(1, min(zoom * pinch, 6))
        ZStack(alignment: .topTrailing) {
            Color.black.ignoresSafeArea()
            if let picture {
                Image(uiImage: picture)
                    .resizable()
                    .scaledToFit()
                    .scaleEffect(scale)
                    .offset(x: offset.width + drag.width, y: offset.height + drag.height)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .ignoresSafeArea()
                    .gesture(
                        MagnifyGesture()
                            .updating($pinch) { value, state, _ in state = value.magnification }
                            .onEnded { value in
                                zoom = max(1, min(zoom * value.magnification, 6))
                                if zoom == 1 { offset = .zero }
                            }
                            .simultaneously(with: DragGesture()
                                .updating($drag) { value, state, _ in if zoom > 1 { state = value.translation } }
                                .onEnded { value in
                                    if zoom > 1 { offset = CGSize(width: offset.width + value.translation.width, height: offset.height + value.translation.height) }
                                })
                    )
                // The phone's own file as it is, else the small one shown.
                Group {
                    if let local {
                        ShareLink(item: local, preview: SharePreview(file.name, image: Image(uiImage: picture))) { shareLabel }
                    } else {
                        ShareLink(item: Image(uiImage: picture), preview: SharePreview(file.name, image: Image(uiImage: picture))) { shareLabel }
                    }
                }
                .padding(12)
                .accessibilityLabel(tr("chat.share"))
            }
        }
        .contentShape(Rectangle())
        .onTapGesture(count: 2) {
            withAnimation(.snappy(duration: 0.25)) {
                if zoom > 1 {
                    zoom = 1
                    offset = .zero
                } else {
                    zoom = 2.5
                }
            }
        }
        .onTapGesture(perform: close)
        .accessibilityAddTraits(.isButton)
        .onAppear {
            if own == nil, let local { own = chatPicture(thumb: nil, local: local, side: 2048) }
        }
        .accessibilityLabel(file.name)
    }

    private var shareLabel: some View {
        Image(systemName: "square.and.arrow.up")
            .font(.system(size: 16, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: 44, height: 44)
            .glassEffect(.regular.interactive(), in: .circle)
    }
}

/// While it works: what it is doing, and how long it has been at it.
private struct BusyLine: View {
    let text: String
    let since: Int64?
    @State private var sweep = false
    @Environment(\.accessibilityReduceMotion) private var still

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            HStack(spacing: 9) {
                Image(systemName: "ellipsis")
                    .symbolEffect(.variableColor.iterative, options: .repeating)
                    .foregroundStyle(.secondary)
                Text(said(at: context.date))
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .overlay {
                        // A light passes over the words, as long as it works.
                        GeometryReader { box in
                            LinearGradient(colors: [.clear, .primary.opacity(0.75), .clear], startPoint: .leading, endPoint: .trailing)
                                .frame(width: box.size.width * 0.5)
                                .offset(x: (sweep ? 1.2 : -0.6) * box.size.width)
                        }
                        .mask {
                            Text(said(at: context.date))
                                .font(.subheadline)
                                .lineLimit(1)
                        }
                        .allowsHitTesting(false)
                    }
            }
        }
        .onAppear {
            guard !still else { return }
            withAnimation(.linear(duration: 1.8).repeatForever(autoreverses: false)) { sweep = true }
        }
    }

    private func said(at date: Date) -> String {
        guard let since else { return text }
        let secs = max(0, Int64(date.timeIntervalSince1970) - since)
        return "\(text) · \(tr("chat.secs", Int(secs)))"
    }
}

/// What the agent did, folded into one line that opens on a tap.
private struct FoldView: View {
    let fold: ChatFold
    let toggle: () -> Void
    /// Turned at the tap, before the core's view comes back with what
    /// opens; following that view when it says otherwise.
    @State private var turned: Bool?

    private var open: Bool { turned ?? fold.open }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button {
                withAnimation(.snappy(duration: 0.2)) { turned = !open }
                toggle()
            } label: {
                HStack(spacing: 7) {
                    if fold.state == "running" {
                        ProgressView().controlSize(.mini)
                    } else {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 11, weight: .semibold))
                            .rotationEffect(.degrees(open ? 90 : 0))
                            .frame(width: 12)
                    }
                    Image(systemName: symbol)
                        .font(.system(size: 13))
                    Text(fold.label)
                        .font(.subheadline)
                        .lineLimit(1)
                    if let added = fold.added, added > 0 {
                        Text("+\(added)").font(.footnote).foregroundStyle(ChatColors.added)
                    }
                    if let removed = fold.removed, removed > 0 {
                        Text("−\(removed)").font(.footnote).foregroundStyle(ChatColors.removed)
                    }
                    switch fold.state {
                    case "failed": Text(fold.badge ?? "✕").font(.caption).foregroundStyle(ChatColors.removed)
                    case "declined": Text(fold.badge ?? "").font(.caption).foregroundStyle(ChatColors.waiting)
                    case "done" where fold.icon == "run":
                        Image(systemName: "checkmark").font(.caption2.weight(.bold)).foregroundStyle(ChatColors.added)
                    default: EmptyView()
                    }
                }
                .foregroundStyle(.secondary)
                .frame(minHeight: 26, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .disabled(!fold.can_open)
            // What opens slides open and fades in, and closes the same way.
            if fold.open, let body = fold.body {
                FoldBodyView(shown: body)
                    .padding(.leading, 20)
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .clipped()
        .animation(.snappy(duration: 0.22), value: fold.open)
        .onChange(of: fold.open) { turned = nil }
    }

    private var symbol: String { foldSymbol(fold.icon) }
}

private func foldSymbol(_ icon: String) -> String {
    switch icon {
    case "think": return "lightbulb"
    case "look": return "doc.text"
    case "run": return "terminal"
    case "edit": return "pencil"
    case "group": return "square.stack"
    default: return "wrench.and.screwdriver"
    }
}

/// Work done between two things said, as one line; open, its folds.
private struct GroupView: View {
    let group: ChatGroup
    let toggle: () -> Void
    let toggleFold: (String) -> Void
    /// Turned at the tap, before the core's view comes back.
    @State private var turned: Bool?

    private var open: Bool { turned ?? group.open }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button {
                withAnimation(.snappy(duration: 0.2)) { turned = !open }
                toggle()
            } label: {
                HStack(spacing: 7) {
                    if group.state == "running" {
                        ProgressView().controlSize(.mini)
                    } else {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 11, weight: .semibold))
                            .rotationEffect(.degrees(open ? 90 : 0))
                            .frame(width: 12)
                    }
                    Image(systemName: foldSymbol(group.icon))
                        .font(.system(size: 13))
                    Text(group.label)
                        .font(.subheadline)
                        .lineLimit(1)
                    if let added = group.added, added > 0 {
                        Text("+\(added)").font(.footnote).foregroundStyle(ChatColors.added)
                    }
                    if let removed = group.removed, removed > 0 {
                        Text("−\(removed)").font(.footnote).foregroundStyle(ChatColors.removed)
                    }
                    if let badge = group.badge {
                        Text(badge).font(.caption)
                            .foregroundStyle(group.state == "declined" ? ChatColors.waiting : ChatColors.removed)
                    }
                }
                .foregroundStyle(.secondary)
                .frame(minHeight: 26, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if group.open {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(group.rows) { row in
                        if case let .fold(fold) = row {
                            FoldView(fold: fold) { toggleFold(fold.id) }
                        }
                    }
                }
                .padding(.leading, 20)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .clipped()
        .animation(.snappy(duration: 0.22), value: group.open)
        .onChange(of: group.open) { turned = nil }
    }
}

private enum ChatColors {
    static let added = Color(red: 0.19, green: 0.82, blue: 0.35)
    static let removed = Color(red: 1.0, green: 0.41, blue: 0.38)
    static let waiting = Color(red: 1.0, green: 0.62, blue: 0.04)
}

private struct FoldBodyView: View {
    let shown: FoldBody
    @Environment(\.colorScheme) private var scheme
    /// Output, lists and diffs grow with the chat's text; the boxes
    /// around them do not.
    @Environment(\.chatTextScale) private var scale

    var body: some View {
        switch shown {
        case let .blocks(blocks):
            MarkdownBlocks(blocks: blocks, copies: false).foregroundStyle(.secondary)
        case let .output(text):
            ScrollView(.horizontal, showsIndicators: false) {
                Text(text)
                    .font(.system(size: 12 * scale, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .fixedSize()
                    .textSelection(.enabled)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
            }
            .background(codeGround, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        case let .list(lines):
            VStack(alignment: .leading, spacing: 2) {
                ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                    Text(line)
                        .font(.system(size: 12.5 * scale, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
        case let .diff(files):
            VStack(alignment: .leading, spacing: 8) {
                ForEach(files, id: \.self) { file in
                    VStack(alignment: .leading, spacing: 0) {
                        Text(file.path)
                            .font(.system(size: 11.5 * scale, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 10)
                            .padding(.vertical, 6)
                        ScrollView(.horizontal, showsIndicators: false) {
                            VStack(alignment: .leading, spacing: 0) {
                                ForEach(Array(file.lines.enumerated()), id: \.offset) { _, line in
                                    diffLine(line)
                                }
                            }
                        }
                    }
                    .background(codeGround, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                }
            }
        }
    }

    private var codeGround: Color {
        scheme == .dark ? Color(red: 0.09, green: 0.094, blue: 0.106) : Color(red: 0.95, green: 0.95, blue: 0.96)
    }

    private func diffLine(_ line: DiffLine) -> some View {
        let (ground, ink, mark): (Color, Color, String) = switch line.t {
        case "add": (ChatColors.added.opacity(0.14), scheme == .dark ? Color(red: 0.75, green: 0.97, blue: 0.80) : Color(red: 0.08, green: 0.41, blue: 0.18), "+ ")
        case "del": (ChatColors.removed.opacity(0.14), scheme == .dark ? Color(red: 1.0, green: 0.77, blue: 0.75) : Color(red: 0.62, green: 0.11, blue: 0.09), "- ")
        case "hunk": (.clear, Color(red: 0.5, green: 0.55, blue: 1.0), "")
        default: (.clear, .secondary, "  ")
        }
        return Text(mark + line.s)
            .font(.system(size: 11.5 * scale, design: .monospaced))
            .foregroundStyle(ink)
            .fixedSize()
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
            .background(ground)
    }
}

/// A table as a phone shows one: the header bold, a thin line under each
/// row, each column as wide as its widest cell (a longer one wraps), the
/// whole sliding sideways when wider than the screen.
private struct TableBlock: View {
    let align: [String]
    let head: [[MdSpan]]
    let rows: [[[MdSpan]]]
    @Environment(\.chatTextScale) private var scale

    var body: some View {
        let columns = max(head.count, rows.map(\.count).max() ?? 0)
        let all = [head] + rows
        ScrollView(.horizontal, showsIndicators: false) {
            TableLayout(columns: columns, widest: 240) {
                ForEach(0..<(all.count * columns), id: \.self) { at in
                    let (r, c) = (at / columns, at % columns)
                    cell(c < all[r].count ? all[r][c] : [], column: c, row: r, last: r == all.count - 1)
                }
            }
        }
        .scrollBounceBehavior(.basedOnSize, axes: .horizontal)
    }

    private func cell(_ spans: [MdSpan], column: Int, row: Int, last: Bool) -> some View {
        let lines = column < align.count ? align[column] : "none"
        let (frame, text): (Alignment, TextAlignment) = switch lines {
        case "center": (.top, .center)
        case "right": (.topTrailing, .trailing)
        default: (.topLeading, .leading)
        }
        return Text(MarkdownBlocks.attributed(spans, size: 14.5 * scale, weight: row == 0 ? .semibold : .regular))
            .lineSpacing(2)
            .multilineTextAlignment(text)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: frame)
            .overlay(alignment: .bottom) {
                if !last {
                    Rectangle()
                        .fill(Color.primary.opacity(row == 0 ? 0.22 : 0.1))
                        .frame(height: row == 0 ? 1 : 0.5)
                }
            }
    }
}

/// A table's cells, row by row: each column as wide as its widest cell up
/// to `widest` (past which a cell wraps), each row as tall as its tallest.
private struct TableLayout: Layout {
    let columns: Int
    let widest: CGFloat

    struct Measure {
        var widths: [CGFloat] = []
        var heights: [CGFloat] = []
    }

    private func measure(_ subviews: Subviews) -> Measure {
        guard columns > 0 else { return Measure() }
        var widths = Array(repeating: CGFloat(0), count: columns)
        for (i, view) in subviews.enumerated() {
            widths[i % columns] = max(widths[i % columns], min(view.sizeThatFits(.unspecified).width, widest))
        }
        var heights = Array(repeating: CGFloat(0), count: (subviews.count + columns - 1) / columns)
        for (i, view) in subviews.enumerated() {
            let height = view.sizeThatFits(ProposedViewSize(width: widths[i % columns], height: nil)).height
            heights[i / columns] = max(heights[i / columns], height)
        }
        return Measure(widths: widths, heights: heights)
    }

    func makeCache(subviews: Subviews) -> Measure { measure(subviews) }

    func updateCache(_ cache: inout Measure, subviews: Subviews) { cache = measure(subviews) }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Measure) -> CGSize {
        CGSize(width: cache.widths.reduce(0, +), height: cache.heights.reduce(0, +))
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Measure) {
        var y = bounds.minY
        for r in cache.heights.indices {
            var x = bounds.minX
            for c in 0..<columns {
                let i = r * columns + c
                guard i < subviews.count else { break }
                subviews[i].place(
                    at: CGPoint(x: x, y: y), anchor: .topLeading,
                    proposal: ProposedViewSize(width: cache.widths[c], height: cache.heights[r])
                )
                x += cache.widths[c]
            }
            y += cache.heights[r]
        }
    }
}

/// Markdown's blocks as text views.
struct MarkdownBlocks: View {
    let blocks: [MdBlock]
    /// Code blocks carry a copy button (what the agent said, not a fold's).
    var copies = true
    /// The blocks in full when `blocks` is only as far as they have come
    /// in: a code block's copy takes its code from it.
    var whole: [MdBlock]? = nil
    @Environment(\.colorScheme) private var scheme
    @Environment(\.codeScroll) private var codeScroll

    /// The code of block `at` of `whole`, when there is one.
    private static func code(_ whole: [MdBlock]?, _ at: Int) -> String? {
        guard let whole, at < whole.count, case let .code(_, text) = whole[at] else { return nil }
        return text
    }
    @Environment(\.chatTextScale) private var scale

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { at, block in
                blockView(block, at: at)
            }
        }
    }

    @ViewBuilder
    private func blockView(_ block: MdBlock, at: Int) -> some View {
        let first = at == 0
        switch block {
        case let .para(spans):
            Text(Self.attributed(spans, size: 16 * scale))
                .lineSpacing(4)
                .fixedSize(horizontal: false, vertical: true)
        case let .heading(level, spans):
            Text(Self.attributed(spans, size: (level == 1 ? 20 : level == 2 ? 18 : 16.5) * scale, weight: .semibold))
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, !first && level <= 2 ? 6 : 0)
        case let .item(depth, mark, spans):
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(mark).font(.system(size: 16 * scale)).foregroundStyle(.secondary.opacity(0.8)).frame(minWidth: 16, alignment: .leading)
                Text(Self.attributed(spans, size: 16 * scale))
                    .lineSpacing(4)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.leading, CGFloat(depth) * 18)
        case let .quote(spans):
            HStack(spacing: 12) {
                Capsule().fill(Color.secondary.opacity(0.3)).frame(width: 3)
                Text(Self.attributed(spans, size: 16 * scale))
                    .lineSpacing(4)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .fixedSize(horizontal: false, vertical: true)
        case let .code(lang, text):
            let named = !lang.trimmingCharacters(in: .whitespaces).isEmpty
            VStack(alignment: .leading, spacing: copies ? 0 : 4) {
                // Its language, and a way to copy it whole.
                if named || copies {
                    HStack(spacing: 0) {
                        Text(lang)
                            .font(.system(size: 11.5 * scale))
                            .foregroundStyle(.secondary)
                        Spacer(minLength: 0)
                        if copies {
                            CopyButton(size: 12) { UIPasteboard.general.string = Self.code(whole, at) ?? text }
                        }
                    }
                    .padding(.leading, 12)
                    .padding(.trailing, 2)
                    .padding(.top, copies ? 0 : 8)
                }
                ScrollView(.horizontal, showsIndicators: false) {
                    Text(text)
                        .font(.system(size: 13 * scale, design: .monospaced))
                        .lineSpacing(2)
                        .fixedSize()
                        .padding(.horizontal, 12)
                        .padding(.top, named || copies ? 0 : 10)
                        .padding(.bottom, 10)
                }
                .onScrollPhaseChange { _, phase in codeScroll.active = phase != .idle }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                scheme == .dark ? Color(red: 0.09, green: 0.094, blue: 0.106) : Color(red: 0.95, green: 0.95, blue: 0.96),
                in: RoundedRectangle(cornerRadius: 12, style: .continuous)
            )
        case .rule:
            Divider()
        case let .table(align, head, rows):
            TableBlock(align: align, head: head, rows: rows)
        }
    }

    static func attributed(_ spans: [MdSpan], size: CGFloat, weight: Font.Weight = .regular) -> AttributedString {
        var out = AttributedString()
        for span in spans {
            var piece = AttributedString(span.text)
            var font: Font = span.code == true ? .system(size: size - 2, design: .monospaced) : .system(size: size)
            font = font.weight(span.bold == true ? .semibold : weight)
            if span.italic == true { font = font.italic() }
            piece.font = font
            if span.code == true { piece.backgroundColor = Color.primary.opacity(0.08) }
            // Words just come in, still fading in.
            if let fade = span.fade, fade < 1 { piece.foregroundColor = Color.primary.opacity(fade) }
            if span.strike == true { piece.strikethroughStyle = .single }
            if let link = span.link, let url = URL(string: link) {
                piece.link = url
                piece.underlineStyle = .single
            }
            out.append(piece)
        }
        return out
    }
}
