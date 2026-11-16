import SafariServices
import SwiftUI
import UIKit

/// The app's own device in a tailnet: ThinkTerm joins by itself, so the
/// tailnet's machines are reached without the Tailscale app. What it runs
/// on is the user's (whether to join, a control server, a name) and the
/// device's (its keys, an auth key: in the Keychain); the node itself is
/// the core's (`tailnetSet`), which this tells what to run and asks how it
/// goes.
final class TailnetStore: ObservableObject {
    static let shared = TailnetStore()

    /// Whether this build can join at all.
    let supported = tailnetSupported()
    @Published private(set) var status = TailnetStatus(
        revision: 0, state: "off", loginUrl: "", name: "", address: "", error: "", peers: []
    )
    /// Why the settings as they stand start no node.
    @Published private(set) var settingsError = ""
    @Published var enabled: Bool {
        didSet { defaults.set(enabled, forKey: "tailnet.enabled") }
    }
    /// Blank for Tailscale's own.
    @Published var controlURL: String {
        didSet { defaults.set(controlURL, forKey: "tailnet.control") }
    }
    /// Blank for a name made from the device's keys.
    @Published var hostname: String {
        didSet { defaults.set(hostname, forKey: "tailnet.hostname") }
    }

    private let defaults = UserDefaults.standard
    private var poll: Timer?
    private static let keysAccount = "tailnet.keys"
    private static let authKeyAccount = "tailnet.authkey"

    private init() {
        enabled = defaults.bool(forKey: "tailnet.enabled")
        controlURL = defaults.string(forKey: "tailnet.control") ?? ""
        hostname = defaults.string(forKey: "tailnet.hostname") ?? ""
    }

    /// An auth key, for joining without a login in the browser; kept with
    /// the other secrets, never shown again.
    var hasAuthKey: Bool { Keychain.load(account: Self.authKeyAccount)?.isEmpty == false }
    /// This device has an identity in a tailnet, signed in or about to be:
    /// there is something for Log Out to forget.
    var hasKeys: Bool { volatileKeys != nil || Keychain.load(account: Self.keysAccount) != nil }

    func setAuthKey(_ key: String) {
        let key = key.trimmingCharacters(in: .whitespacesAndNewlines)
        if key.isEmpty {
            Keychain.delete(account: Self.authKeyAccount)
        } else {
            Keychain.save(key, account: Self.authKeyAccount)
        }
    }

    /// Has the core run a node on the settings as they stand, or none.
    func apply() {
        guard supported else { return }
        guard enabled else {
            settingsError = tailnetSet(config: nil)
            watch()
            return
        }
        // The device's identity in the tailnet: made once, kept until a
        // log out. A build that cannot keep it (an unsigned simulator one)
        // is a new device at every launch.
        var keys = Keychain.load(account: Self.keysAccount) ?? volatileKeys
        if keys == nil, let made = tailnetNewKeys() {
            Keychain.save(made, account: Self.keysAccount)
            volatileKeys = made
            keys = made
        }
        guard let keys else {
            settingsError = "no keys"
            return
        }
        settingsError = tailnetSet(config: TailnetConfig(
            keys: keys, controlUrl: controlURL, hostname: hostname,
            authKey: Keychain.load(account: Self.authKeyAccount) ?? ""
        ))
        watch()
    }

    /// Try again, at a person's tap: the settings as they stand, and the
    /// node running made anew on them. Settings that did not change leave
    /// the node as it is, so `apply` alone would do nothing for one that
    /// failed.
    func retry() {
        apply()
        tailnetRestart()
    }

    /// Leave the tailnet and forget the device: joining again is a new one.
    func logOut() {
        enabled = false
        Keychain.delete(account: Self.keysAccount)
        Keychain.delete(account: Self.authKeyAccount)
        volatileKeys = nil
        apply()
    }

    private var volatileKeys: String?

    #if DEBUG
    /// `--tailnettest`: join, and say on the console how the node stands
    /// as it changes -- the state alone, never the login link or a
    /// machine's name.
    func debugJoin() {
        enabled = true
        apply()
        var last = ""
        Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            guard let self else { return }
            let now = "\(self.status.state) link=\(!self.status.loginUrl.isEmpty) machines=\(self.status.peers.count) error=\(!self.status.error.isEmpty || !self.settingsError.isEmpty)"
            if now != last {
                last = now
                print("TAILNETTEST \(now)")
            }
        }
    }
    #endif

    /// The status, asked every second while there is a node -- and while
    /// one is on its way out: a node stopped (a log out, the switch off)
    /// winds down after the call that stops it, still saying it is in, and
    /// its last word, off, must be read too or the page keeps the old one.
    private func watch() {
        refresh()
        guard poll == nil else { return }
        poll = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.refresh()
            if !self.enabled && self.status.state == "off" {
                self.poll?.invalidate()
                self.poll = nil
            }
        }
    }

    private func refresh() {
        let now = tailnetStatus()
        if now.revision != status.revision || now.state != status.state { status = now }
    }

    /// The machines of the tailnet other than this one, those that answer
    /// first and each group by name.
    var machines: [TailnetPeer] {
        status.peers.sorted { a, b in
            let (ao, bo) = (a.online == true, b.online == true)
            return ao != bo ? ao : a.name.localizedCaseInsensitiveCompare(b.name) == .orderedAscending
        }
    }

    /// What the status says, in a line.
    var summary: String {
        switch status.state {
        case "starting": return tr("tn.starting")
        case "needs_login": return tr("tn.needslogin")
        case "needs_approval": return tr("tn.needsapproval")
        case "connected": return tr("tn.connected", status.name)
        case "failed": return tr("tn.failed")
        default: return tr("tn.off")
        }
    }

    var color: Color {
        switch status.state {
        case "connected": return .green
        case "starting", "needs_login", "needs_approval": return .orange
        case "failed": return .red
        default: return .secondary
        }
    }
}

/// The tailnet on the hosts' page: where it stands, the way to its
/// settings, and once in, its machines -- a tap on one opens the page of
/// the host saved for it, or adds a new one with its name filled in.
struct TailnetSection: View {
    @ObservedObject private var tailnet = TailnetStore.shared
    @ObservedObject private var lang = AppLanguage.shared
    /// A saved host answers to this machine, or none does.
    var saved: (TailnetPeer) -> Host?
    var add: (Host) -> Void

    var body: some View {
        if tailnet.supported {
            Section("Tailscale") {
                NavigationLink {
                    TailnetSettingsView()
                } label: {
                    HostRowLayout(title: "Tailscale", subtitle: tailnet.summary) {
                        Image(systemName: "point.3.connected.trianglepath.dotted")
                            .font(.system(size: 17, weight: .medium))
                            .foregroundStyle(Color.accentColor)
                    }
                }
                if tailnet.status.state == "connected" {
                    if tailnet.machines.isEmpty {
                        Text(tr("tn.none")).font(.subheadline).foregroundStyle(.secondary)
                    }
                    ForEach(tailnet.machines, id: \.fqdn) { peer in
                        if let host = saved(peer) {
                            // Kept already: its own page.
                            NavigationLink(value: host) {
                                machine(peer) {
                                    Text(tr("tn.added")).font(.subheadline).foregroundStyle(.secondary)
                                }
                            }
                        } else {
                            Button {
                                // Its name is its address: the node knows it.
                                var host = Host()
                                host.name = peer.name
                                host.hostname = peer.hostname
                                add(host)
                            } label: {
                                machine(peer) {
                                    Image(systemName: "plus")
                                        .font(.system(size: 13, weight: .bold))
                                        .foregroundStyle(Color.accentColor)
                                        .frame(width: 28, height: 28)
                                        .background(Circle().fill(Color.accentColor.opacity(0.16)))
                                }
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
            }
        }
    }

    /// A machine: its maker's mark, its name, what it is and whether it is
    /// in; one that is away is quieter, the whole row.
    private func machine<Trailing: View>(_ peer: TailnetPeer, @ViewBuilder trailing: () -> Trailing) -> some View {
        HStack(spacing: 12) {
            HostRowLayout(title: peer.name, subtitle: Self.detail(peer)) {
                let mark = Self.mark(peer)
                if mark.brand {
                    BrandIcon(name: mark.name, size: 19)
                } else {
                    Image(systemName: mark.name).font(.system(size: 17, weight: .medium))
                }
            }
            trailing()
        }
        .contentShape(Rectangle())
        .opacity(peer.online == false ? 0.5 : 1)
    }

    /// "macOS · Online", as much of it as the machine says.
    private static func detail(_ peer: TailnetPeer) -> String {
        let state = peer.online.map { tr($0 ? "tn.online" : "tn.offline") }
        return [peer.os, state].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
    }

    /// The maker's mark where there is one -- the distribution's, for
    /// Linux -- and a plain picture where not: the desktop's choice
    /// (`machine_icon` in its hosts page), in the system's own symbols where
    /// it has them. A brand's name, or a symbol's.
    static func mark(_ peer: TailnetPeer) -> (name: String, brand: Bool) {
        switch peer.os.lowercased() {
        case "macos", "ios", "ipados", "tvos": return ("apple.logo", false)
        case "android": return ("android", true)
        case "linux": return (distroMark(peer.distro) ?? "linux", true)
        // The app carries no Windows mark.
        case "windows": return ("pc", false)
        default: return ("server.rack", false)
        }
    }

    /// A distribution's mark, by the name it gives itself (the desktop's
    /// `distro_to_icon`).
    private static func distroMark(_ id: String) -> String? {
        switch id.trimmingCharacters(in: .whitespaces).lowercased() {
        case "ubuntu": return "ubuntu"
        case "debian": return "debian"
        case "raspbian": return "raspberrypi"
        case "fedora": return "fedora"
        case "arch", "archlinux", "arch-linux": return "archlinux"
        case "alpine": return "alpinelinux"
        case "rhel", "redhat", "red hat": return "redhat"
        case "centos": return "centos"
        case "nixos": return "nixos"
        case "proxmox", "pve": return "proxmox"
        default: return nil
        }
    }
}

/// Whether ThinkTerm joins a tailnet, the login it needs, and what the
/// node runs on. The fields are kept as they are left, and the node is
/// started on them then.
struct TailnetSettingsView: View {
    @ObservedObject private var tailnet = TailnetStore.shared
    @ObservedObject private var lang = AppLanguage.shared
    @State private var copied = false
    @State private var copiedAddress = false
    /// The login page is up, inside the app.
    @State private var login: LoginPage?

    /// Where it stands, off when not asked for whatever the node said last.
    private var state: String { tailnet.enabled ? tailnet.status.state : "off" }

    var body: some View {
        List {
            // Where it stands and the one thing to do about it, first.
            Section {
                hero
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 4, leading: 16, bottom: 4, trailing: 16))
            }

            if state == "needs_login", !tailnet.status.loginUrl.isEmpty {
                Section(tr("tn.qr.title")) { LoginCode(link: tailnet.status.loginUrl) }
            }

            if state == "connected" {
                Section(tr("tn.thisdevice")) {
                    LabeledContent(tr("f.name"), value: tailnet.status.name)
                    if !tailnet.status.address.isEmpty {
                        LabeledContent(tr("tn.address")) {
                            HStack(spacing: 10) {
                                Text(tailnet.status.address).font(.body.monospaced()).foregroundStyle(.primary)
                                Button {
                                    UIPasteboard.general.string = tailnet.status.address
                                    copiedAddress = true
                                    DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { copiedAddress = false }
                                } label: {
                                    Image(systemName: copiedAddress ? "checkmark" : "doc.on.doc")
                                }
                                .buttonStyle(.borderless)
                                .accessibilityLabel(tr(copiedAddress ? "copied" : "copy"))
                            }
                        }
                    }
                    LabeledContent(tr("tn.machines"), value: tr("tn.online.count", "\(tailnet.machines.filter { $0.online == true }.count)"))
                }
            }

            Section {
                NavigationLink(tr("f.advanced")) { TailnetAdvancedView() }
            }

            // Ending it: for now, or for good.
            if tailnet.enabled {
                Section {
                    Button(tr("tn.turnoff")) {
                        tailnet.enabled = false
                        tailnet.apply()
                    }
                    Button(tr("tn.logout"), role: .destructive) { tailnet.logOut() }
                } footer: {
                    Text(tr("tn.turnoff.foot"))
                }
            } else if tailnet.hasKeys {
                Section {
                    Button(tr("tn.logout"), role: .destructive) { tailnet.logOut() }
                }
            }
        }
        .navigationTitle("Tailscale")
        .navigationBarTitleDisplayMode(.inline)
        // The login stays in the app: the system's own browser sheet, which
        // the sign-in providers accept where a web view of the app's they
        // refuse. It goes by itself once the device is in.
        .sheet(item: $login) { page in
            SafariPage(url: page.url).ignoresSafeArea()
        }
        .onChange(of: tailnet.status.state) { _, state in
            if state == "connected" { login = nil }
        }
    }

    /// Its tile, where it stands, a word on that, and the way on.
    private var hero: some View {
        VStack(spacing: 0) {
            GlassTile(symbol: "point.3.connected.trianglepath.dotted", tint: GlassTile.blue, size: 76)
                .overlay(alignment: .bottomTrailing) {
                    if state != "off" {
                        Circle()
                            .fill(tailnet.color)
                            .frame(width: 16, height: 16)
                            .overlay(Circle().stroke(Color(.systemGroupedBackground), lineWidth: 3))
                            .offset(x: 4, y: 4)
                    }
                }
            Text("Tailscale")
                .font(.title.weight(.bold))
                .foregroundStyle(Color.primary)
                .padding(.top, 14)
            let pill = state == "off" ? Color.secondary : tailnet.color
            HStack(spacing: 6) {
                Circle().fill(pill).frame(width: 8, height: 8)
                Text(state == "off" ? tr("tn.off") : tailnet.summary)
            }
            .font(.footnote.weight(.medium))
            .foregroundStyle(pill)
            .padding(.horizontal, 11)
            .padding(.vertical, 5)
            .background(Capsule().fill(pill.opacity(0.14)))
            .padding(.top, 10)
            if !blurb.isEmpty {
                Text(blurb)
                    .font(.subheadline)
                    .foregroundStyle(Color.secondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 10)
            }
            // The settings' own error even while it is off: they are why it
            // did not start. While it is connecting, what held up its last
            // try, quietly: it tries again by itself, nothing for a person
            // to do.
            let error = tailnet.settingsError.isEmpty ? (state == "off" ? "" : tailnet.status.error) : tailnet.settingsError
            let quiet = tailnet.settingsError.isEmpty && state == "starting"
            if !error.isEmpty {
                Text(error)
                    .font(.footnote)
                    .foregroundStyle(quiet ? Color.secondary : Color.red)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
            }
            actions.padding(.top, 20)
        }
        .frame(maxWidth: .infinity)
    }

    private var blurb: String {
        switch state {
        case "off": return tr("tn.join.foot")
        case "needs_login": return tr("tn.login.body")
        // Not an error: something for a person to do elsewhere.
        case "needs_approval": return tr("tn.approval.hint")
        case "connected": return tr("tn.connected.body")
        default: return ""
        }
    }

    @ViewBuilder
    private var actions: some View {
        switch state {
        case "off":
            wide(tr("tn.signin"), symbol: "person.crop.circle.fill", lead: true) {
                tailnet.enabled = true
                tailnet.apply()
            }
        case "failed":
            wide(tr("retry"), symbol: "arrow.clockwise", lead: true) { tailnet.retry() }
        case "needs_login":
            HStack(spacing: 10) {
                if let url = URL(string: tailnet.status.loginUrl) {
                    wide(tr("tn.login"), symbol: "safari", lead: true) { login = LoginPage(url: url) }
                }
                wide(tr(copied ? "copied" : "copy"), symbol: copied ? "checkmark" : "doc.on.doc", lead: false) {
                    UIPasteboard.general.string = tailnet.status.loginUrl
                    copied = true
                    DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { copied = false }
                }
            }
            .glassGroup(spacing: 10)
        default:
            EmptyView()
        }
    }

    /// A button as wide as there is room for: glass, tinted for the one
    /// that leads.
    private func wide(_ title: String, symbol: String, lead: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Label(title, systemImage: symbol)
                .lineLimit(1)
                .minimumScaleFactor(0.8)
                .font(.body.weight(.semibold))
                .foregroundStyle(lead ? Color.white : Color.primary)
                // Clear of the capsule's round ends, two side by side too.
                .padding(.horizontal, 16)
                .frame(maxWidth: .infinity, minHeight: 50)
                .contentShape(Capsule())
                .glassCapsule(tint: lead ? .accentColor : nil)
        }
        .buttonStyle(.borderless)
    }
}

/// The tailnet's own settings, for a company's or one's own control
/// server: kept as they are left, and the node started on them then.
private struct TailnetAdvancedView: View {
    @ObservedObject private var tailnet = TailnetStore.shared
    @ObservedObject private var lang = AppLanguage.shared
    @State private var authKey = ""

    var body: some View {
        List {
            Section {
                SecureField(tailnet.hasAuthKey ? "••••••••" : tr("tn.authkey"), text: $authKey)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .onSubmit { keepAuthKey() }
                TextField(tr("tn.control"), text: $tailnet.controlURL, prompt: Text(verbatim: "controlplane.tailscale.com"))
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .onSubmit { tailnet.apply() }
                TextField(tr("tn.name"), text: $tailnet.hostname, prompt: Text(tr("tn.auto")))
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .onSubmit { tailnet.apply() }
            }
        }
        .navigationTitle(tr("f.advanced"))
        .navigationBarTitleDisplayMode(.inline)
        // What was typed and left is kept, as a field's return keeps it.
        .onDisappear {
            if !authKey.isEmpty { keepAuthKey() } else if tailnet.enabled { tailnet.apply() }
        }
    }

    private func keepAuthKey() {
        tailnet.setAuthKey(authKey)
        authKey = ""
        if tailnet.enabled { tailnet.apply() }
    }
}

/// The login link as a code another device's camera reads, to log this
/// one in there (a computer's password manager, a security key). The core
/// draws it (`qrPicture`), as it draws it for the Android app; shown pixel
/// for pixel, dark on white whatever the appearance.
private struct LoginCode: View {
    let link: String
    @Environment(\.displayScale) private var scale

    var body: some View {
        VStack(spacing: 10) {
            if let picture = qrPicture(text: link, size: UInt32((200 * scale).rounded())),
               let image = TabIconImages.image(picture, scale: scale) {
                Image(uiImage: image)
                    .interpolation(.none)
                    .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .accessibilityHidden(true)
            }
            Text(tr("tn.qr"))
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
    }
}

/// The tailnet's login page, by its address.
private struct LoginPage: Identifiable {
    let url: URL
    var id: URL { url }
}

/// A page in the system's in-app browser.
private struct SafariPage: UIViewControllerRepresentable {
    let url: URL

    func makeUIViewController(context: Context) -> SFSafariViewController {
        SFSafariViewController(url: url)
    }

    func updateUIViewController(_ controller: SFSafariViewController, context: Context) {}
}

extension TailnetPeer {
    /// Whether another of the tailnet's machines has this one's name: one
    /// of its own and one shared into it from another tailnet, say. The
    /// node takes that name for its own tailnet's machine; the full names
    /// tell the two apart.
    func nameShared(among peers: [TailnetPeer]) -> Bool {
        peers.filter { $0.name.caseInsensitiveCompare(name) == .orderedSame }.count > 1
    }

    /// What a host for this machine connects to: its full name. Its short
    /// name may come to stand for another -- one of this tailnet's own that
    /// joins under it later, which the node takes before a machine shared
    /// in.
    var hostname: String {
        let full = fqdn.trimmingCharacters(in: CharacterSet(charactersIn: "."))
        return full.isEmpty ? name : full
    }

    /// What a saved host for this machine may connect to: its full name or
    /// an address, or its name where no other machine has it too.
    func names(among peers: [TailnetPeer]) -> [String] {
        [nameShared(among: peers) ? "" : name, fqdn, fqdn.trimmingCharacters(in: CharacterSet(charactersIn: ".")), ipv4, ipv6]
            .filter { !$0.isEmpty }.map { $0.lowercased() }
    }
}
