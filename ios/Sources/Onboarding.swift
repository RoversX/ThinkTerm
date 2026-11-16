import SwiftUI

/// The project's home, linked from the first page.
private let repo = URL(string: "https://github.com/RoversX/ThinkTerm")!

/// A first run's three pages, on the starry sky: what ThinkTerm is,
/// connecting a computer (or not yet), and in. Only while no host is kept
/// and they were never gone through; `enter` rushes into the app. The same
/// pages as Android's `Onboarding`.
struct Onboarding: View {
    /// Told each page shown, for the sky to move with it.
    var onPage: (Int) -> Void
    var enter: () -> Void
    @EnvironmentObject private var store: HostStore
    @ObservedObject private var lang = AppLanguage.shared
    @Environment(\.openURL) private var openURL
    @State private var page = 0
    @State private var tailnet = false
    @State private var adding = false

    /// The pages' ink, on the sky whatever the phone's appearance.
    private static let ink = Color(red: 0.95, green: 0.95, blue: 0.97)
    private static let dim = Color(red: 0.68, green: 0.68, blue: 0.70)
    private static let faint = Color(red: 0.56, green: 0.56, blue: 0.58)

    var body: some View {
        ZStack(alignment: .bottomLeading) {
            Group {
                switch page {
                case 0: intro
                case 1: connect
                default: welcome
                }
            }
            .id(page)
            .transition(.asymmetric(
                insertion: .move(edge: .trailing).combined(with: .opacity),
                removal: .move(edge: .leading).combined(with: .opacity)
            ))
        }
        .frame(maxWidth: 460, maxHeight: .infinity, alignment: .bottomLeading)
        .padding(.horizontal, 28)
        .padding(.bottom, 30)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        // A swipe to the right goes back a page; to the left, on from the first.
        .gesture(DragGesture(minimumDistance: 24).onEnded { drag in
            let next: Int
            if drag.translation.width > 72, page > 0 {
                next = page - 1
            } else if drag.translation.width < -72, page == 0 {
                next = 1
            } else {
                return
            }
            UISelectionFeedbackGenerator().selectionChanged()
            go(next)
        })
        .foregroundStyle(Self.ink)
        // The pages are on the sky; the screens they open follow the phone.
        .environment(\.colorScheme, .dark)
        .sheet(isPresented: $tailnet, onDismiss: { go(2) }) {
            NavigationStack {
                TailnetSettingsView()
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) { Button(tr("done")) { tailnet = false } }
                    }
            }
        }
        .sheet(isPresented: $adding) {
            HostEditView(host: Host()) { host, secret, passphrase in
                Keychain.save(secret, account: host.id.uuidString)
                if let passphrase { Keychain.save(passphrase, account: host.id.uuidString + ".passphrase") }
                store.upsert(host)
                go(2)
            }
            .environmentObject(store)
        }
    }

    private func go(_ next: Int) {
        withAnimation(.spring(response: 0.42, dampingFraction: 0.9)) { page = next }
        onPage(next)
    }

    private var intro: some View {
        VStack(alignment: .leading, spacing: 0) {
            Image("OnboardingIcon")
                .resizable()
                .frame(width: 48, height: 48)
                .padding(.leading, -3)
            Text("ThinkTerm")
                .font(.system(size: 30, weight: .bold))
                .tracking(-0.5)
                .padding(.top, 14)
            Text(tr("ob.tagline"))
                .font(.system(size: 15))
                .foregroundStyle(Self.dim)
                .padding(.top, 6)
            Button { openURL(repo) } label: {
                HStack(spacing: 6) {
                    Image("GitHubMark").resizable().frame(width: 13, height: 13)
                    Text("github.com/RoversX/ThinkTerm").font(.system(size: 12.5))
                }
                .foregroundStyle(Self.faint)
                .padding(.vertical, 4)
            }
            .buttonStyle(.plain)
            .padding(.top, 8)
            primary(tr("ob.continue")) { go(1) }
                .padding(.top, 26)
        }
    }

    private var connect: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(tr("ob.connect"))
                .font(.system(size: 25, weight: .bold))
                .tracking(-0.4)
            Text(tr("ob.connect.body"))
                .font(.system(size: 14))
                .foregroundStyle(Self.dim)
                .padding(.top, 6)
            primary(tr("ob.tailscale")) { tailnet = true }
                .padding(.top, 24)
            secondary(tr("ob.manual")) { adding = true }
                .padding(.top, 9)
            small(tr("ob.later")) { go(2) }
                .frame(maxWidth: .infinity)
                .padding(.top, 12)
        }
    }

    private var welcome: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(tr("ob.welcome"))
                .font(.system(size: 34, weight: .bold))
                .tracking(-0.6)
            primary(tr("ob.start"), weight: .medium, action: enter)
                .padding(.top, 28)
        }
    }

    /// A press felt through the phone's own haptics, as the system's
    /// buttons are: the platform's feedback, so the user's settings hold.
    private func felt(_ weight: UIImpactFeedbackGenerator.FeedbackStyle, _ action: @escaping () -> Void) -> () -> Void {
        {
            UIImpactFeedbackGenerator(style: weight).impactOccurred()
            action()
        }
    }

    /// The lead button: clear glass a little brighter than the rest, so
    /// the stars behind it bend at its edges.
    private func primary(_ label: String, weight: UIImpactFeedbackGenerator.FeedbackStyle = .light, action: @escaping () -> Void) -> some View {
        Button(action: felt(weight, action)) {
            Text(label)
                .font(.system(size: 16, weight: .semibold))
                .frame(maxWidth: .infinity)
                .frame(height: 52)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .clearGlass(lead: true)
    }

    /// The other choices: clear glass.
    private func secondary(_ label: String, action: @escaping () -> Void) -> some View {
        Button(action: felt(.light, action)) {
            Text(label)
                .font(.system(size: 16, weight: .semibold))
                .frame(maxWidth: .infinity)
                .frame(height: 52)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .clearGlass(lead: false)
    }

    /// A small glass capsule, as wide as its words.
    private func small(_ label: String, action: @escaping () -> Void) -> some View {
        Button(action: felt(.light, action)) {
            Text(label)
                .font(.system(size: 13.5))
                .foregroundStyle(Color(red: 0.78, green: 0.78, blue: 0.8))
                .padding(.horizontal, 16)
                .frame(height: 38)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .clearGlass(lead: false)
    }
}

private extension View {
    /// The system's own glass that answers a touch, as its glass buttons
    /// do: clear, the lead one faintly lit; a frosted capsule before iOS 26.
    @ViewBuilder
    func clearGlass(lead: Bool) -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(lead ? .clear.tint(.white.opacity(0.14)).interactive() : .clear.interactive(), in: .capsule)
        } else {
            background(.ultraThinMaterial, in: Capsule())
        }
    }
}
