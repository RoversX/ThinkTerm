import SwiftUI
import UniformTypeIdentifiers

/// Add or edit one host. The private key never travels back out of the
/// Keychain into a field: the editor only reports whether one is stored,
/// and hands a new secret to `save` when the user supplies one.
///
/// As a sheet it adds or edits and goes. As a host's own page (`page`),
/// pushed from the list, it shows the host and its session first, with the
/// way into its terminal; Save keeps an edit and stays.
struct HostEditView: View {
    @Environment(\.dismiss) private var dismiss
    @EnvironmentObject var store: HostStore
    @State var host: Host
    var page: HostPage? = nil
    @ObservedObject private var lang = AppLanguage.shared
    @State private var secret = ""
    @State private var passphrase = ""
    @State private var hasStoredSecret = false
    @State private var showKeyText = false
    @State private var forgottenHostKey: String?
    @State private var showAdvanced = false
    @State private var importing = false
    @State private var importError: String?
    @State private var copiedPublicKey = false
    /// The page's host as last kept, and the secrets as they were then:
    /// what Save compares the fields with.
    @State private var kept: Host?
    @State private var keptSecret = ""
    @State private var keptPassphrase = ""
    @State private var confirmDelete = false
    /// Delete was asked of a host others are reached through: it is not.
    @State private var jumpInUse = false
    var save: (Host, String, String?) -> Void

    var body: some View {
        if let page {
            form(page)
                .navigationTitle("")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button(tr("save")) { keep() }
                            .fontWeight(.semibold)
                            .disabled(!changed || !canSave)
                    }
                }
                .confirmationDialog(tr("host.delete.title"), isPresented: $confirmDelete, titleVisibility: .visible) {
                    Button(tr("host.delete.confirm", (kept ?? host).display), role: .destructive) {
                        page.delete()
                        dismiss()
                    }
                    Button(tr("cancel"), role: .cancel) {}
                } message: {
                    Text(tr("host.delete.body"))
                }
                .alert(tr("jump.inuse.title"), isPresented: $jumpInUse) {
                    Button(tr("ok"), role: .cancel) {}
                } message: {
                    Text(tr("jump.inuse.body", store.reachedThrough(host.id).map(\.display).joined(separator: ", ")))
                }
        } else {
            NavigationStack {
                form(nil)
                    .navigationTitle(host.hostname.isEmpty ? tr("newhost") : host.display)
                    .navigationBarTitleDisplayMode(.inline)
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) { Button(tr("cancel")) { dismiss() } }
                        ToolbarItem(placement: .confirmationAction) {
                            Button(tr("save")) {
                                save(edited(), secret, passphrase.isEmpty ? nil : passphrase)
                                dismiss()
                            }
                            .fontWeight(.semibold)
                            .disabled(!canSave)
                        }
                    }
            }
        }
    }

    private func form(_ page: HostPage?) -> some View {
        Form {
            if let page {
                // The host as kept, its session and the way in, over its
                // picture's choices.
                Section {
                    iconRow
                } header: {
                    VStack(spacing: 0) {
                        HostHero(host: kept ?? host, sessions: page.sessions, line: store.line(for: kept ?? host))
                        Text(tr("f.icon"))
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.top, 26)
                    }
                    .textCase(nil)
                }
            } else {
                Section(tr("f.icon")) { iconRow }
            }
            hostSection
            groupSection
            authSection
            jumpSection
            advancedSection
            if let page { manageSection(page) }
        }
        .scrollContentBackground(.hidden)
        .background(Color.pageBackdrop.ignoresSafeArea())
        .onAppear {
            hasStoredSecret = Keychain.load(account: host.id.uuidString) != nil
            if kept == nil { kept = host }
        }
        .fileImporter(isPresented: $importing, allowedContentTypes: [.data, .text]) { result in
            importKey(result)
        }
        .alert(
            tr("key.importfailed"),
            isPresented: Binding(get: { importError != nil }, set: { if !$0 { importError = nil } })
        ) {
            Button(tr("ok"), role: .cancel) { importError = nil }
        } message: {
            Text(importError ?? "")
        }
    }

    /// The desktop's host pictures, the one the host wears picked out.
    private var iconRow: some View {
        HStack(spacing: 0) {
            ForEach(Host.icons, id: \.self) { name in
                let chosen = (host.icon.isEmpty ? Host.icons[0] : host.icon) == name
                Button {
                    // The first is the one a host with none chosen wears.
                    host.icon = name == Host.icons[0] ? "" : name
                } label: {
                    Image(systemName: Host.iconSymbol(name))
                        .font(.system(size: 15, weight: .medium))
                        .foregroundStyle(chosen ? Color.white : Color.primary.opacity(0.8))
                        .frame(width: 36, height: 36)
                        .background(
                            RoundedRectangle(cornerRadius: 10, style: .continuous)
                                .fill(chosen ? Color.accentColor : Color(.tertiarySystemFill))
                        )
                }
                .buttonStyle(.borderless)
                .accessibilityLabel(name)
                .accessibilityAddTraits(chosen ? .isSelected : [])
                if name != Host.icons.last { Spacer(minLength: 2) }
            }
        }
        .padding(.vertical, 4)
    }

    /// Copying the host, forgetting its key, deleting it: at the page's end.
    private func manageSection(_ page: HostPage) -> some View {
        Section {
            Button(tr("host.duplicate")) {
                page.duplicate()
                dismiss()
            }
            // The key as the store has it now: a connection made from this
            // page may have pinned one since it opened.
            if store.hosts.first(where: { $0.id == host.id })?.knownHost != nil {
                Button(tr("forgetkey")) { page.sessions.forgetHostKey(host.id, store: store) }
            }
            Button(tr("host.delete"), role: .destructive) {
                if store.reachedThrough(host.id).isEmpty { confirmDelete = true } else { jumpInUse = true }
            }
        }
    }

    /// The page's fields differ from the host as kept.
    private var changed: Bool {
        guard let kept else { return false }
        return !Self.sameEdits(host, kept) || secret != keptSecret || passphrase != keptPassphrase
    }

    /// What the page edits of a host alike. The rest -- the key pinned on
    /// a connection, when it was last reached -- the store keeps up to
    /// date behind the page, and is not the page's to write back.
    private static func sameEdits(_ a: Host, _ b: Host) -> Bool {
        a.name == b.name && a.hostname == b.hostname && a.port == b.port && a.user == b.user
            && a.auth == b.auth && a.remoteCommand == b.remoteCommand && a.group == b.group
            && a.publicKey == b.publicKey && a.icon == b.icon && a.autoConnect == b.autoConnect
            && a.jumpHost == b.jumpHost
    }

    /// The edit onto the host as the store has it now: the key a
    /// connection pinned, when it was last reached, are the store's.
    private func edited() -> Host {
        var edited = store.hosts.first { $0.id == host.id } ?? host
        edited.name = host.name
        edited.hostname = host.hostname
        edited.port = host.port
        edited.user = host.user
        edited.auth = host.auth
        edited.remoteCommand = host.remoteCommand
        edited.group = host.group
        edited.publicKey = host.publicKey
        edited.autoConnect = host.autoConnect
        edited.icon = host.icon
        edited.jumpHost = host.jumpHost
        if let forgottenHostKey, edited.knownHost == forgottenHostKey {
            edited.knownHost = nil
        }
        return edited
    }

    /// Save on the page: the edit goes onto the host as the store has it
    /// now, and the page goes on with the result. A secret is handed on
    /// only when it is new since the last keep.
    private func keep() {
        let edited = edited()
        let newSecret = secret == keptSecret ? "" : secret
        let newPassphrase = passphrase == keptPassphrase || passphrase.isEmpty ? nil : passphrase
        save(edited, newSecret, newPassphrase)
        let now = store.hosts.first { $0.id == host.id } ?? edited
        host = now
        kept = now
        keptSecret = secret
        keptPassphrase = passphrase
        if !secret.isEmpty { hasStoredSecret = true }
    }

    /// The host this one is reached through, picked from the other saved
    /// hosts (`HostStore.jumpCandidates`). A host others are reached through
    /// has none of its own, and says why.
    private var jumpSection: some View {
        let behind = store.reachedThrough(host.id)
        let candidates = store.jumpCandidates(for: host)
        return Section {
            Picker(tr("f.jump"), selection: $host.jumpHost) {
                Text(tr("f.jump.none")).tag(UUID?.none)
                ForEach(candidates) { candidate in
                    Text(candidate.title).tag(UUID?.some(candidate.id))
                }
                // One no longer offered -- gone, or given a jump host of its
                // own since -- still shows as the one picked.
                if let chosen = host.jumpHost, !candidates.contains(where: { $0.id == chosen }) {
                    Text(store.hosts.first { $0.id == chosen }?.title ?? tr("jump.missing")).tag(UUID?.some(chosen))
                }
            }
            .disabled(!behind.isEmpty)
        } header: {
            Text(tr("f.jump"))
        } footer: {
            Text(behind.isEmpty ? tr("f.jump.foot") : tr("f.jump.isjump"))
        }
    }

    private var hostSection: some View {
        Section(tr("f.host")) {
            labelled(tr("f.name")) {
                TextField(tr("optional"), text: $host.name)
            }
            labelled(tr("f.hostname")) {
                TextField("example.com or 192.0.2.10", text: $host.hostname)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.URL)
            }
            labelled(tr("f.port")) {
                TextField("22", value: $host.port, format: .number)
                    .keyboardType(.numberPad)
            }
            labelled(tr("f.user")) {
                TextField("root", text: $host.user)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
            }
            Toggle(tr("host.autoconnect"), isOn: $host.autoConnect)
        }
    }

    private var groupSection: some View {
        Section {
            labelled(tr("f.group")) {
                TextField(tr("none"), text: $host.group)
                    .autocorrectionDisabled()
            }
            if !store.groups.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(store.groups, id: \.self) { group in
                            Button {
                                host.group = host.group == group ? "" : group
                            } label: {
                                Text(group)
                                    .font(.footnote.weight(.medium))
                                    .padding(.horizontal, 12)
                                    .padding(.vertical, 6)
                                    .background(
                                        Capsule().fill(
                                            host.group == group
                                                ? Color.accentColor.opacity(0.85)
                                                : Color.secondary.opacity(0.2)
                                        )
                                    )
                                    .foregroundStyle(host.group == group ? Color.white : Color.primary)
                            }
                            .buttonStyle(.plain)
                        }
                    }
                    .padding(.vertical, 2)
                }
                .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 8, trailing: 16))
            }
        } header: {
            Text(tr("f.group"))
        } footer: {
            Text(tr("f.groupfoot"))
        }
    }

    private var authSection: some View {
        Section(tr("f.auth")) {
            Picker(tr("f.method"), selection: $host.auth) {
                ForEach(Host.AuthKind.allCases) { kind in Text(kind.label).tag(kind) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.vertical, 2)

            if host.auth == .key {
                keyStatusRow
                Button { pasteKey() } label: { Label(tr("key.paste"), systemImage: "doc.on.clipboard") }
                Button { importing = true } label: { Label(tr("key.import"), systemImage: "folder") }
                Button { generateKey() } label: { Label(tr("key.generate"), systemImage: "key.fill") }
                if let publicKey = host.publicKey, !publicKey.isEmpty {
                    publicKeyRow(publicKey)
                }
                DisclosureGroup(tr("key.pastetext"), isExpanded: $showKeyText) {
                    TextEditor(text: $secret)
                        .font(.system(size: 11, design: .monospaced))
                        .frame(minHeight: 120)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .scrollContentBackground(.hidden)
                        .onChange(of: secret) { _, _ in refreshPublicKey() }
                }
                SecureField(tr("key.passphrase"), text: $passphrase)
            } else {
                SecureField(tr("f.password"), text: $secret)
            }
        }
    }

    private var keyStatusRow: some View {
        HStack(spacing: 10) {
            Image(systemName: keyStatus.symbol)
                .foregroundStyle(keyStatus.tint)
            Text(keyStatus.text)
                .font(.subheadline)
            Spacer(minLength: 0)
        }
        .padding(.vertical, 2)
    }

    private func publicKeyRow(_ publicKey: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(publicKey)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(4)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            Button {
                UIPasteboard.general.string = publicKey
                copiedPublicKey = true
            } label: {
                Label(
                    copiedPublicKey ? tr("copied") : tr("key.copypub"),
                    systemImage: copiedPublicKey ? "checkmark" : "doc.on.doc"
                )
                .font(.subheadline)
            }
            .buttonStyle(.bordered)
            Text(tr("key.authorized"))
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 6)
    }

    private var advancedSection: some View {
        Section {
            DisclosureGroup(tr("f.advanced"), isExpanded: $showAdvanced) {
                VStack(alignment: .leading, spacing: 6) {
                    TextField(tr("f.remotecmd"), text: $host.remoteCommand)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.system(size: 13, design: .monospaced))
                    Text(tr("f.remotecmd.foot"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 4)
                if let known = host.knownHost {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(tr("f.hostkey"))
                            .font(.subheadline)
                        Text(known)
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .lineLimit(3)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(.vertical, 4)
                    if page == nil {
                        Button(role: .destructive) {
                            forgottenHostKey = known
                            host.knownHost = nil
                        } label: {
                            Label(tr("forgetkey"), systemImage: "xmark.shield")
                        }
                    }
                }
            }
        }
    }

    private func labelled<Field: View>(_ label: String, @ViewBuilder field: () -> Field) -> some View {
        HStack(spacing: 10) {
            Text(label)
                .foregroundStyle(.secondary)
                .frame(width: 86, alignment: .leading)
            field()
        }
    }

    private var isExisting: Bool { store.hosts.contains { $0.id == host.id } }

    private var canSave: Bool {
        guard !host.hostname.trimmingCharacters(in: .whitespaces).isEmpty,
              !host.user.trimmingCharacters(in: .whitespaces).isEmpty else { return false }
        guard (1...65535).contains(host.port) else { return false }
        // A new host has nothing in the Keychain yet, so it needs a secret now.
        return isExisting || hasStoredSecret || !secret.isEmpty
    }

    private var keyStatus: (text: String, symbol: String, tint: Color) {
        if !secret.isEmpty {
            return host.publicKey == nil
                ? (tr("key.loaded"), "checkmark.seal.fill", .green)
                : (tr("key.loaded.ed25519"), "checkmark.seal.fill", .green)
        }
        if hasStoredSecret { return (tr("key.stored"), "lock.fill", .green) }
        return (tr("key.none"), "exclamationmark.triangle.fill", .orange)
    }

    private var keyComment: String { "\(host.user)@thinkterm-ios" }

    private func pasteKey() {
        guard let text = UIPasteboard.general.string,
              !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        secret = text
        refreshPublicKey()
    }

    private func generateKey() {
        let key = SSHKey.generate(comment: keyComment)
        secret = key.privatePEM
        host.publicKey = key.publicLine
        copiedPublicKey = false
    }

    private func importKey(_ result: Result<URL, Error>) {
        switch result {
        case .success(let url):
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            guard let text = try? String(contentsOf: url, encoding: .utf8) else {
                importError = tr("key.notext")
                return
            }
            secret = text
            refreshPublicKey()
        case .failure(let error):
            importError = error.localizedDescription
        }
    }

    /// Only an unencrypted ed25519 key yields a public line; for anything
    /// else the user pastes the public half on the host themselves.
    private func refreshPublicKey() {
        host.publicKey = SSHKey.publicLine(ofPrivate: secret, comment: keyComment)
        copiedPublicKey = false
    }
}

/// What a host's own page has beyond the editor: the sessions, for the
/// host's, and the ways to copy or remove the host.
struct HostPage {
    let sessions: Sessions
    var duplicate: () -> Void
    var delete: () -> Void
}

/// The top of a host's page: its picture, name and address as kept, where
/// its session stands, and the way into its terminal.
private struct HostHero: View {
    let host: Host
    @ObservedObject var sessions: Sessions
    /// Its address and what it is reached through (`HostStore.line`).
    var line: String? = nil

    var body: some View {
        VStack(spacing: 0) {
            HostTile(size: 76) { Image(systemName: host.iconSymbol).font(.system(size: 32, weight: .medium)) }
            // The colours themselves: in a section's header the hierarchical
            // styles are the header's own, a step quieter.
            Text(host.title)
                .font(.title2.weight(.bold))
                .foregroundStyle(Color.primary)
                .lineLimit(1)
                .padding(.top, 14)
            Text(line ?? host.address)
                .font(.subheadline.monospaced())
                .foregroundStyle(Color.secondary)
                .lineLimit(1)
                .padding(.top, 3)
            if let model = sessions.existing(host.id) {
                HostHeroSession(host: host, model: model)
            } else {
                HostStatusPill(state: .none)
                HostActions(host: host, state: .none) {}
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.top, 4)
    }
}

/// The part of the top that follows a session as it goes.
private struct HostHeroSession: View {
    let host: Host
    @ObservedObject var model: TerminalModel
    @ObservedObject private var settings = AppSettings.shared

    var body: some View {
        let state = SessionState(model, autoReconnect: model.autoReconnect)
        HostStatusPill(state: state)
        HostActions(host: host, state: state) { model.disconnect() }
    }
}

private struct HostStatusPill: View {
    let state: SessionState

    private var look: (text: String, color: Color) {
        switch state {
        case .connected: return (tr("host.connected"), .green)
        case .connecting: return (tr("connecting"), .orange)
        case .lost: return (tr("conn.disconnected"), .red)
        case .none: return (tr("host.notconnected"), .secondary)
        }
    }

    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(look.color).frame(width: 8, height: 8)
            Text(look.text)
        }
        .font(.footnote.weight(.medium))
        .foregroundStyle(look.color)
        .padding(.horizontal, 11)
        .padding(.vertical, 5)
        .background(Capsule().fill(look.color.opacity(0.14)))
        .padding(.top, 10)
    }
}

/// Into the terminal -- opened, or connected first -- and, while there is
/// a session, the way to end it.
private struct HostActions: View {
    let host: Host
    let state: SessionState
    var disconnect: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            NavigationLink(value: HostTerminal(host: host)) {
                HStack(spacing: 7) {
                    Image(systemName: state == .connected ? "terminal" : "link")
                    Text(tr(state == .connected ? "host.open" : "connect"))
                        .lineLimit(1)
                        .minimumScaleFactor(0.8)
                }
                .font(.body.weight(.semibold))
                .foregroundStyle(.white)
                // Clear of the capsule's round ends: a word too long for
                // its half shrinks a little rather than run under them.
                .padding(.horizontal, 16)
                .frame(maxWidth: .infinity, minHeight: 50)
                .contentShape(Capsule())
                .glassCapsule(tint: .accentColor)
            }
            .buttonStyle(.plain)
            if state == .connected || state == .connecting {
                Button(action: disconnect) {
                    HStack(spacing: 7) {
                        Image(systemName: "power")
                        Text(tr("disconnect"))
                            .lineLimit(1)
                            .minimumScaleFactor(0.8)
                    }
                    .font(.body.weight(.semibold))
                    .foregroundStyle(Color.primary)
                    .padding(.horizontal, 16)
                    .frame(maxWidth: .infinity, minHeight: 50)
                    .contentShape(Capsule())
                    .glassCapsule()
                }
                .buttonStyle(.plain)
            }
        }
        .glassGroup(spacing: 10)
        .padding(.top, 18)
    }
}
