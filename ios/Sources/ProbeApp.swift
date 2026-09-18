import SwiftUI

@main
struct ProbeApp: App {
    @StateObject private var model = ProbeModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
        }
        // The lifecycle the plan cares about: the surface is handed back
        // before the app is suspended and re-attached when it returns.
        // (iOS keeps the layer alive across a background stay, but Android
        // does not, and the core must be exercised the same way on both.)
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background: model.detachForBackground()
            case .active: model.reattachAfterBackground()
            default: break
            }
        }
    }
}

struct ContentView: View {
    @EnvironmentObject var model: ProbeModel

    var body: some View {
        VStack(spacing: 0) {
            connection
            ZStack(alignment: .topLeading) {
                MetalView()
                TerminalInput()
                if let composing = model.composing {
                    Text(composing)
                        .font(.system(size: 14, design: .monospaced))
                        .padding(4)
                        .background(Color.yellow.opacity(0.9))
                        .foregroundColor(.black)
                        .padding(8)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding(model.inset)
            .background(Color.black)
            keyBar
            controls
            ScrollView {
                Text(model.logText)
                    .font(.system(size: 10, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(6)
            }
            .frame(height: 120)
            .background(Color(white: 0.1))
        }
        .background(Color.black)
        .ignoresSafeArea(.container, edges: .bottom)
    }

    private var connection: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                TextField("host", text: $model.host).frame(width: 110)
                TextField("port", text: $model.port).frame(width: 50)
                TextField("user", text: $model.user).frame(width: 80)
                Button("Connect") { model.connect() }
                Button("Drop") { model.disconnect() }
            }
            .textFieldStyle(.roundedBorder)
            .font(.caption)
            .autocorrectionDisabled()
            .textInputAutocapitalization(.never)
            Text(model.status)
                .font(.system(size: 10, design: .monospaced))
                .lineLimit(1)
        }
        .padding(6)
        .background(Color(white: 0.15))
        .foregroundColor(.white)
    }

    private var keyBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                Button("Esc") { model.key("Escape") }
                Button("Tab") { model.key("Tab") }
                Button("^C") { model.key("c", ctrl: true) }
                Button("^D") { model.key("d", ctrl: true) }
                Button("^L") { model.key("l", ctrl: true) }
                Button("↑") { model.key("ArrowUp") }
                Button("↓") { model.key("ArrowDown") }
                Button("←") { model.key("ArrowLeft") }
                Button("→") { model.key("ArrowRight") }
                Button("⌫") { model.key("Backspace") }
                Button("Scroll↑") { model.scroll(5) }
                Button("Scroll↓") { model.scroll(-5) }
                Button("⌨︎") { model.focusKeyboard() }
                Button("⌨︎✕") { model.dismissKeyboard() }
            }
            .buttonStyle(.bordered)
            .font(.caption)
            .padding(.horizontal, 6)
        }
        .frame(height: 36)
        .background(Color(white: 0.12))
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Button(model.attached ? "Detach" : "Attach") { model.toggleAttached() }
                Button(model.animating ? "Stop" : "Animate") { model.toggleAnimating() }
                Button("Frame") { model.requestOneFrame() }
                Button(model.inset > 0 ? "Grow" : "Shrink") { model.toggleInset() }
                Button("Cycle x20") { model.cycleAttachDetach(times: 20) }
            }
            .buttonStyle(.bordered)
            .font(.caption)
            Text(model.stats)
                .font(.system(size: 9, design: .monospaced))
                .lineLimit(3)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(6)
        .background(Color(white: 0.15))
        .foregroundColor(.white)
    }
}
