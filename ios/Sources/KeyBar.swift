import SwiftUI
import UIKit

/// The keys a soft keyboard has not got, with sticky Ctrl and Alt, and at
/// the row's right end — fixed, never scrolling away — the button that
/// opens the extension panel underneath. The panel is not the soft
/// keyboard's switch: it is the rest of the keys, the snippets, what was
/// sent, and the colours, as the prototype's `.keybar` + `.kpanel`.
struct KeyBar: View {
    @ObservedObject var model: TerminalModel
    @State private var tab: KeyPanelTab = KeyBar.debugTab
    @ObservedObject private var settings = AppSettings.shared

    /// `--keypanel snippets` also picks the face it opens on.
    private static var debugTab: KeyPanelTab {
        let args = ProcessInfo.processInfo.arguments
        guard let i = args.firstIndex(of: "--keypanel"), i + 1 < args.count,
              let tab = KeyPanelTab(rawValue: args[i + 1]) else { return .keys }
        return tab
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 0) {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 6) {
                        ForEach(KeyCaps.bar) { cap in
                            key(cap)
                        }
                    }
                    .padding(.horizontal, 6)
                }
                more
            }
            .frame(height: 38)
            if model.panelOpen {
                KeyPanel(model: model, tab: $tab)
            }
        }
        .background(model.background)
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
                .font(.system(size: 13, design: .monospaced))
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(isHeld(cap) ? Color.accentColor : Color.white.opacity(0.08))
                .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .foregroundColor(.white)
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
                .frame(width: 40, height: 28)
                .background(model.panelOpen ? Color.accentColor : Color.white.opacity(0.14))
                .clipShape(RoundedRectangle(cornerRadius: 7))
        }
        .buttonStyle(.plain)
        .foregroundColor(.white)
        .padding(.leading, 4)
        .padding(.trailing, 6)
    }
}
