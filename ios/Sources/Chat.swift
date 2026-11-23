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
    case text(id: String, blocks: [MdBlock])
    case fold(ChatFold)
    case note(id: String, text: String)
    case group(ChatGroup)

    var id: String {
        switch self {
        case let .user(id, _, _): return id
        case let .text(id, _), let .note(id, _): return id
        case let .fold(fold): return fold.id
        case let .group(group): return group.id
        }
    }

    private enum Key: String, CodingKey { case kind, id, text, blocks, attachments }

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
        case "text": self = .text(id: id, blocks: try c.decode([MdBlock].self, forKey: .blocks))
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

    private enum Key: String, CodingKey { case t, spans, level, depth, mark, lang, text }

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
}

// MARK: the page

private let working = Color(red: 0.20, green: 0.78, blue: 0.35)
private let waiting = Color(red: 1.0, green: 0.62, blue: 0.04)

/// The chat with the agent of a pane, over that pane's terminal: what it
/// said and did, what it asks, and a field to say something. Going back --
/// the button, or a swipe in from the left edge -- is going to its terminal.
struct ChatPage: View {
    @ObservedObject var model: TerminalModel
    @State private var text = ""
    @State private var notice: ChatNotice?
    @State private var noticeSeen: Int64 = 0
    /// The end of the list is in view: what comes next is shown as it
    /// comes. Scrolled up to read, the list stays where it is.
    @State private var atEnd = true
    /// Just said something: what comes of it is shown, wherever the
    /// person was.
    @State private var follow = false
    @State private var pickingPhotos = false
    @State private var pickingFiles = false
    @State private var photos: [PhotosPickerItem] = []
    /// A picture sent, shown big.
    @State private var shownPicture: ChatSent?
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let chat = model.chat
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
                    ForEach(chat?.rows ?? []) { row in
                        rowView(row).id(row.id)
                    }
                    if let busy = chat?.busy {
                        BusyLine(text: busy, since: chat?.since).id("busy")
                    }
                    Color.clear.frame(height: 1).id("end")
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
            .scrollDismissesKeyboard(.interactively)
            .scrollEdgeEffectStyle(.soft, for: .all)
            .defaultScrollAnchor(.bottom)
            .safeAreaInset(edge: .top, spacing: 0) { header(chat) }
            .safeAreaInset(edge: .bottom, spacing: 0) { foot(chat) }
            .onScrollGeometryChange(for: Bool.self) { geometry in
                geometry.visibleRect.maxY >= geometry.contentSize.height - 120
            } action: { _, near in
                atEnd = near
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
        .background(Color.chromeBackdrop.ignoresSafeArea())
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
        .fullScreenCover(item: $shownPicture) { picture in
            PictureView(
                file: picture,
                thumb: picture.picture.flatMap { model.chatThumbs[$0] },
                local: picture.path.flatMap { model.chatPictures[$0] }
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
            DragGesture(minimumDistance: 20, coordinateSpace: .global).onEnded { drag in
                if drag.startLocation.x < 28 && drag.translation.width > 80 && abs(drag.translation.height) < 80 {
                    model.closeChat()
                }
            }
        )
    }

    // MARK: the bar

    private func header(_ chat: ChatState?) -> some View {
        let target = model.chatWith
        let dot: Color = switch chat?.state {
        case "working": working
        case "blocked": waiting
        default: .gray
        }
        let who = (chat?.agent).flatMap { $0.isEmpty ? nil : $0 } ?? target?.agent ?? ""
        let said = [who, chat?.state_label ?? "", chat?.title ?? ""].filter { !$0.isEmpty }.joined(separator: " · ")
        return HStack(spacing: 10) {
            Button {
                model.closeChat()
            } label: {
                Image(systemName: "chevron.left")
                    .font(.system(size: 17, weight: .semibold))
                    .frame(width: 44, height: 44)
            }
            .buttonStyle(.glass)
            .buttonBorderShape(.circle)
            .accessibilityLabel(tr("back"))
            VStack(alignment: .leading, spacing: 2) {
                Text(target?.place ?? "")
                    .font(.headline)
                    .lineLimit(1)
                HStack(spacing: 6) {
                    Circle().fill(dot).frame(width: 7, height: 7)
                    Text(said)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 0)
            if let menu = chat?.model {
                modelButton(menu)
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 4)
        .padding(.bottom, 10)
    }

    // MARK: what it said and did

    @ViewBuilder
    private func rowView(_ row: ChatRow) -> some View {
        switch row {
        case let .user(_, text, attachments):
            VStack(alignment: .trailing, spacing: 6) {
                if !attachments.isEmpty {
                    SentFiles(files: attachments, pictures: model.chatPictures, thumbs: model.chatThumbs) { shownPicture = $0 }
                }
                if !text.isEmpty {
                    HStack {
                        Spacer(minLength: 48)
                        Text(text)
                            .font(.system(size: 16))
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
        case let .text(_, blocks):
            MarkdownBlocks(blocks: blocks)
                .textSelection(.enabled)
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
        GlassEffectContainer(spacing: 10) {
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
                if let problem = chat?.problem {
                    problemCard(problem)
                } else if let ask = chat?.ask {
                    askCard(ask)
                }
                field(chat)
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 8)
        .padding(.bottom, 8)
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
                    model.closeChat()
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

    private func problemCard(_ problem: ChatProblem) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(problem.text)
                .font(.subheadline)
                .fixedSize(horizontal: false, vertical: true)
            if !problem.detail.isEmpty {
                Text(problem.detail)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
            }
            if let action = problem.action {
                Button {
                    model.chatAction(action.id)
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

    /// The field to say something in, and the button beside it: send, or
    /// stop while the agent works and nothing is typed.
    private func field(_ chat: ChatState?) -> some View {
        let files = model.chatFiles
        let blank = text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        // What is picked goes when all of it is there.
        let waiting = files.contains { $0.path == nil && $0.error == nil }
        let ready = files.contains { $0.path != nil }
        let canSend = chat?.can_send == true && !waiting && (!blank || ready)
        let stop = chat?.can_stop == true && blank && files.isEmpty
        return VStack(spacing: 8) {
            if !files.isEmpty {
                PickedFiles(files: files) { model.chatDetach($0) }
            }
            fieldRow(chat, canSend: canSend, stop: stop)
        }
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
                .font(.system(size: 18, weight: .semibold))
                .frame(width: 46, height: 46)
        }
        .buttonStyle(.glass)
        .buttonBorderShape(.circle)
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

    /// The model in use. A tap opens on the models; one that can think more
    /// or less opens on how hard, a menu of its own.
    private func modelButton(_ menu: ChatModelMenu) -> some View {
        Menu {
            Section(menu.models_heading) {
                ForEach(menu.models) { choice in
                    if choice.efforts.isEmpty {
                        Button {
                            if !choice.current { model.chatSetModel(choice.id, effort: nil) }
                        } label: {
                            if choice.current { Label(choice.label, systemImage: "checkmark") } else { Text(choice.label) }
                        }
                    } else {
                        Menu {
                            Section(menu.efforts_heading) {
                                ForEach(choice.efforts) { effort in
                                    Button {
                                        if !effort.current { model.chatSetModel(choice.id, effort: effort.id) }
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
        } label: {
            HStack(spacing: 4) {
                Text(menu.label)
                    .font(.footnote)
                    .lineLimit(1)
                Image(systemName: "chevron.down")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 12)
            .frame(maxWidth: 170, minHeight: 36)
        }
        .buttonStyle(.glass)
        .accessibilityLabel("\(tr("chat.model")): \(menu.label)")
    }

    private func fieldRow(_ chat: ChatState?, canSend: Bool, stop: Bool) -> some View {
        HStack(alignment: .bottom, spacing: 8) {
            if chat?.can_attach == true {
                attachMenu
            }
            TextField(chat?.placeholder ?? "", text: $text, axis: .vertical)
                .lineLimit(1...6)
                .font(.system(size: 16))
                .padding(.horizontal, 18)
                .padding(.vertical, 12)
                .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 23, style: .continuous))
            Button {
                if stop {
                    model.chatInterrupt()
                } else {
                    model.chatSend(text.trimmingCharacters(in: .whitespacesAndNewlines))
                    text = ""
                    follow = true
                }
            } label: {
                Image(systemName: stop ? "stop.fill" : "arrow.up")
                    .font(.system(size: 17, weight: .bold))
                    .frame(width: 46, height: 46)
            }
            .buttonStyle(.glassProminent)
            .buttonBorderShape(.circle)
            .tint(stop ? .primary : .accentColor)
            .disabled(!(stop || canSend))
            .accessibilityLabel(tr(stop ? "chat.stop" : "chat.send"))
        }
    }
}

/// What is picked to go next, over the field: pictures small, files by
/// name and size; each on its way shows how far, and each can be taken off.
private struct PickedFiles: View {
    let files: [TerminalModel.ChatFile]
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
                            if file.error != nil {
                                RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.red.opacity(0.45))
                                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.white)
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

/// A picture over everything, as large as it fits. A tap puts it away.
private struct PictureView: View {
    let file: ChatSent
    let thumb: UIImage?
    let local: URL?
    let close: () -> Void

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            if let picture = chatPicture(thumb: thumb, local: local, side: 2048) {
                Image(uiImage: picture)
                    .resizable()
                    .scaledToFit()
                    .ignoresSafeArea()
            }
        }
        .contentShape(Rectangle())
        .onTapGesture(perform: close)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(file.name)
    }
}

/// While it works: what it is doing, and how long it has been at it.
private struct BusyLine: View {
    let text: String
    let since: Int64?

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
            }
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

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button(action: toggle) {
                HStack(spacing: 7) {
                    if fold.state == "running" {
                        ProgressView().controlSize(.mini)
                    } else {
                        Image(systemName: fold.open ? "chevron.down" : "chevron.right")
                            .font(.system(size: 11, weight: .semibold))
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
            if fold.open, let body = fold.body {
                FoldBodyView(shown: body).padding(.leading, 20)
            }
        }
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

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button(action: toggle) {
                HStack(spacing: 7) {
                    if group.state == "running" {
                        ProgressView().controlSize(.mini)
                    } else {
                        Image(systemName: group.open ? "chevron.down" : "chevron.right")
                            .font(.system(size: 11, weight: .semibold))
                            .frame(width: 12)
                    }
                    Image(systemName: foldSymbol(group.icon))
                        .font(.system(size: 13))
                    Text(group.label)
                        .font(.subheadline)
                        .lineLimit(1)
                    if let badge = group.badge {
                        Text(badge).font(.caption).foregroundStyle(ChatColors.removed)
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
            }
        }
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

    var body: some View {
        switch shown {
        case let .blocks(blocks):
            MarkdownBlocks(blocks: blocks).foregroundStyle(.secondary)
        case let .output(text):
            ScrollView(.horizontal, showsIndicators: false) {
                Text(text)
                    .font(.system(size: 12, design: .monospaced))
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
                        .font(.system(size: 12.5, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
        case let .diff(files):
            VStack(alignment: .leading, spacing: 8) {
                ForEach(files, id: \.self) { file in
                    VStack(alignment: .leading, spacing: 0) {
                        Text(file.path)
                            .font(.system(size: 11.5, design: .monospaced))
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
            .font(.system(size: 11.5, design: .monospaced))
            .foregroundStyle(ink)
            .fixedSize()
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
            .background(ground)
    }
}

/// Markdown's blocks as text views.
struct MarkdownBlocks: View {
    let blocks: [MdBlock]
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                blockView(block)
            }
        }
    }

    @ViewBuilder
    private func blockView(_ block: MdBlock) -> some View {
        switch block {
        case let .para(spans):
            Text(Self.attributed(spans, size: 16)).fixedSize(horizontal: false, vertical: true)
        case let .heading(level, spans):
            Text(Self.attributed(spans, size: level == 1 ? 21 : level == 2 ? 19 : 17, weight: .semibold))
                .fixedSize(horizontal: false, vertical: true)
        case let .item(depth, mark, spans):
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(mark).font(.system(size: 16)).foregroundStyle(.secondary).frame(minWidth: 16, alignment: .leading)
                Text(Self.attributed(spans, size: 16)).fixedSize(horizontal: false, vertical: true)
            }
            .padding(.leading, CGFloat(depth) * 16)
        case let .quote(spans):
            HStack(spacing: 10) {
                Capsule().fill(Color.secondary.opacity(0.4)).frame(width: 3)
                Text(Self.attributed(spans, size: 16)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            .fixedSize(horizontal: false, vertical: true)
        case let .code(_, text):
            ScrollView(.horizontal, showsIndicators: false) {
                Text(text)
                    .font(.system(size: 13, design: .monospaced))
                    .fixedSize()
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                scheme == .dark ? Color(red: 0.09, green: 0.094, blue: 0.106) : Color(red: 0.95, green: 0.95, blue: 0.96),
                in: RoundedRectangle(cornerRadius: 12, style: .continuous)
            )
        case .rule:
            Divider()
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
