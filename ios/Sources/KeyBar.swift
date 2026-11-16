import PhotosUI
import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// The keys a soft keyboard has not got, with sticky Ctrl and Alt, in
/// pieces of glass under the terminal whether the keyboard is up or not:
/// the keys, at their right end — fixed, never scrolling away — the
/// button that opens the extension panel underneath, and on its own the
/// keyboard's. The panel is not the soft keyboard's switch: it is the
/// rest of the keys, the snippets, what was sent, and the colours, as the
/// prototype's `.keybar` + `.kpanel`.
struct KeyBar: View {
    @ObservedObject var model: TerminalModel
    /// The terminal's keyboard is up.
    var keyboardUp = false
    @State private var pickingPhotos = false
    @State private var pickingFiles = false
    @State private var photos: [PhotosPickerItem] = []
    @State private var tab: KeyPanelTab = KeyBar.debugTab
    @ObservedObject private var settings = AppSettings.shared

    /// `--keypanel snippets` also picks the face it opens on (debug builds).
    private static var debugTab: KeyPanelTab {
        #if DEBUG
        let args = ProcessInfo.processInfo.arguments
        guard let i = args.firstIndex(of: "--keypanel"), i + 1 < args.count,
              let tab = KeyPanelTab(rawValue: args[i + 1]) else { return .keys }
        return tab
        #else
        return .keys
        #endif
    }

    var body: some View {
        VStack(spacing: 8) {
            HStack(spacing: 8) {
                HStack(spacing: 0) {
                    // Room between the keys: four show, the rest scroll.
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 6) {
                            ForEach(KeyCaps.bar) { cap in
                                key(cap)
                            }
                        }
                        .padding(.horizontal, 8)
                    }
                    more
                }
                .frame(height: Self.height)
                .clipShape(Capsule())
                .glassCapsule()
                attach
                keyboard
            }
            .padding(.horizontal, 10)
            if model.panelOpen {
                KeyPanel(model: model, tab: $tab)
            }
        }
        .background(model.background)
        // As a browser would hand them over: a photo in a format anything
        // reads, not the camera's own.
        .photosPicker(
            isPresented: $pickingPhotos, selection: $photos, maxSelectionCount: 5,
            matching: .images, preferredItemEncoding: .compatible
        )
        .onChange(of: photos) { _, picked in
            guard !picked.isEmpty else { return }
            photos = []
            for item in picked { send(item) }
        }
        .fileImporter(isPresented: $pickingFiles, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            guard case .success(let urls) = result else { return }
            for url in urls {
                model.upload(copying: url)
            }
            KeyCaps.tap()
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { note in
            // The keyboard coming up takes the panel's place: the two
            // never stack. A hardware keyboard (the simulator's) posts a
            // zero-height frame and takes nothing.
            let frame = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect
            guard (frame?.height ?? 0) > 0 else { return }
            withAnimation(.easeOut(duration: 0.16)) { model.panelOpen = false }
        }
    }

    private func key(_ cap: KeyCap) -> some View {
        Button { KeyCaps.send(cap, to: model) } label: {
            Text(cap.label)
                .font(.system(size: 15, design: .monospaced))
                .lineLimit(1)
                .fixedSize()
                .padding(.horizontal, 8)
                .frame(minWidth: 40, minHeight: 34)
                .background(isHeld(cap) ? Color.accentColor : Color.clear, in: Capsule())
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .foregroundColor(isHeld(cap) ? .white : model.ink)
    }

    // MARK: files for the host

    /// A photo, a file or the clipboard's picture, left on the host with
    /// its path put on the command line (`TerminalModel.upload`).
    private var attach: some View {
        Menu {
            Button { pickingPhotos = true } label: { Label(tr("attach.photo"), systemImage: "photo.on.rectangle") }
            Button { pickingFiles = true } label: { Label(tr("attach.file"), systemImage: "folder") }
            // Asked whether, not what: looking at it is the user's to allow.
            if UIPasteboard.general.hasImages {
                Button { pasteImage() } label: { Label(tr("attach.paste"), systemImage: "doc.on.clipboard") }
            }
        } label: {
            Image(systemName: "paperclip")
                .font(.system(size: 16, weight: .medium))
                .foregroundColor(model.ink)
                .frame(width: Self.button, height: Self.height)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .glassCapsule()
        .accessibilityLabel(tr("attach"))
    }

    /// "Photo 2026-01-02 at 03.04.05.jpg", as the desktop names a pasted
    /// picture ("Pasted ... .png").
    private static func stamped(_ word: String, _ ext: String) -> String {
        let format = DateFormatter()
        format.locale = Locale(identifier: "en_US_POSIX")
        format.dateFormat = "yyyy-MM-dd 'at' HH.mm.ss"
        return "\(word) \(format.string(from: Date())).\(ext)"
    }

    /// What a picture's bytes say it is; the picker's word for it is the
    /// format it was in before it was made one anything reads.
    private static func pictureExtension(_ data: Data, else fallback: String) -> String {
        let head = [UInt8](data.prefix(12))
        if head.starts(with: [0xFF, 0xD8]) { return "jpg" }
        if head.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "png" }
        if head.starts(with: [0x47, 0x49, 0x46, 0x38]) { return "gif" }
        if head.count == 12, head[4...7].elementsEqual([0x66, 0x74, 0x79, 0x70]) { return "heic" }
        return fallback
    }

    /// Begun as it is chosen: its path goes into the pane focused now,
    /// however long the photo takes to load.
    private func send(_ item: PhotosPickerItem) {
        let begun = model.beginUpload()
        Task {
            guard let data = try? await item.loadTransferable(type: Data.self) else {
                await MainActor.run {
                    model.cancelUpload(begun)
                    model.note(tr("upload.unread", tr("attach.photo")))
                }
                return
            }
            let named = item.supportedContentTypes.first?.preferredFilenameExtension ?? "jpg"
            let name = Self.stamped("Photo", Self.pictureExtension(data, else: named))
            await MainActor.run { upload(name: name, data: data, begun: begun) }
        }
    }

    private func pasteImage() {
        guard let data = UIPasteboard.general.image?.pngData() else { return }
        upload(name: Self.stamped("Pasted", "png"), data: data)
    }

    private func upload(name: String, data: Data, begun: UInt64? = nil) {
        model.upload(name: name, data: data, begun: begun)
        KeyCaps.tap()
    }

    /// A piece's height: the keys' capsule and the buttons beside it.
    static let height: CGFloat = 46
    /// A button's width: as wide as it is high.
    static let button: CGFloat = 46

    /// The keyboard, up or away: on its own beside the keys.
    private var keyboard: some View {
        Button {
            if keyboardUp {
                model.dismissKeyboard()
            } else {
                withAnimation(.easeOut(duration: 0.16)) { model.panelOpen = false }
                model.focusKeyboard()
            }
            KeyCaps.tap()
        } label: {
            Image(systemName: keyboardUp ? "keyboard.chevron.compact.down" : "keyboard")
                .font(.system(size: 16, weight: .medium))
                .foregroundColor(model.ink)
                .frame(width: Self.button, height: Self.height)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .glassCapsule()
        .accessibilityLabel(tr(keyboardUp ? "keyboard.hide" : "keyboard.show"))
    }

    /// A sticky modifier shows as pressed until the next key spends it.
    private func isHeld(_ cap: KeyCap) -> Bool {
        switch cap.action {
        case .sticky(.ctrl): return model.ctrlSticky
        case .sticky(.alt): return model.altSticky
        default: return false
        }
    }

    private var more: some View {
        Button {
            // Opening the panel puts the keyboard away: the panel is its
            // stand-in, not a shelf on top of it.
            if !model.panelOpen { model.dismissKeyboard() }
            withAnimation(.easeOut(duration: 0.16)) { model.panelOpen.toggle() }
            KeyCaps.tap()
        } label: {
            Image(systemName: model.panelOpen ? "chevron.down" : "square.grid.2x2")
                .font(.system(size: 14, weight: .semibold))
                .frame(width: 34, height: 34)
                .background(model.panelOpen ? Color.accentColor : model.ink.opacity(0.12), in: Capsule())
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .foregroundColor(model.panelOpen ? .white : model.ink)
        .padding(.leading, 2)
        .padding(.trailing, 5)
    }
}
