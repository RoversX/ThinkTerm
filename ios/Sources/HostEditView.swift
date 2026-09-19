import SwiftUI
import UniformTypeIdentifiers

/// Add or edit one host. The private key never travels back out of the
/// Keychain into a field: the editor only reports whether one is stored,
/// and hands a new secret to `save` when the user supplies one.
struct HostEditView: View {
    @Environment(\.dismiss) private var dismiss
    @EnvironmentObject var store: HostStore
    @State var host: Host
    @State private var secret = ""
    @State private var passphrase = ""
    @State private var hasStoredSecret = false
    @State private var showKeyText = false
    @State private var showAdvanced = false
    @State private var importing = false
    @State private var importError: String?
    @State private var copiedPublicKey = false
    var save: (Host, String, String?) -> Void

    var body: some View {
        NavigationStack {
            Form {
                hostSection
                groupSection
                authSection
                advancedSection
            }
            .scrollContentBackground(.hidden)
            .background(Color.black.ignoresSafeArea())
            .navigationTitle(host.hostname.isEmpty ? "New host" : host.display)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        save(host, secret, passphrase.isEmpty ? nil : passphrase)
                        dismiss()
                    }
                    .fontWeight(.semibold)
                    .disabled(!canSave)
                }
            }
            .onAppear { hasStoredSecret = Keychain.load(account: host.id.uuidString) != nil }
            .fileImporter(isPresented: $importing, allowedContentTypes: [.data, .text]) { result in
                importKey(result)
            }
            .alert(
                "Import failed",
                isPresented: Binding(get: { importError != nil }, set: { if !$0 { importError = nil } })
            ) {
                Button("OK", role: .cancel) { importError = nil }
            } message: {
                Text(importError ?? "")
            }
        }
    }

    private var hostSection: some View {
        Section("Host") {
            labelled("Name") {
                TextField("Optional", text: $host.name)
            }
            labelled("Hostname") {
                TextField("example.com or 10.0.0.4", text: $host.hostname)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.URL)
            }
            labelled("Port") {
                TextField("22", value: $host.port, format: .number)
                    .keyboardType(.numberPad)
            }
            labelled("User") {
                TextField("root", text: $host.user)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
            }
        }
    }

    private var groupSection: some View {
        Section {
            labelled("Group") {
                TextField("None", text: $host.group)
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
            Text("Group")
        } footer: {
            Text("Groups become sections in the host list.")
        }
    }

    private var authSection: some View {
        Section("Authentication") {
            Picker("Method", selection: $host.auth) {
                ForEach(Host.AuthKind.allCases) { kind in Text(kind.label).tag(kind) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.vertical, 2)

            if host.auth == .key {
                keyStatusRow
                Button { pasteKey() } label: { Label("Paste from clipboard", systemImage: "doc.on.clipboard") }
                Button { importing = true } label: { Label("Import file…", systemImage: "folder") }
                Button { generateKey() } label: { Label("Generate new key", systemImage: "key.fill") }
                if let publicKey = host.publicKey, !publicKey.isEmpty {
                    publicKeyRow(publicKey)
                }
                DisclosureGroup("Paste key text", isExpanded: $showKeyText) {
                    TextEditor(text: $secret)
                        .font(.system(size: 11, design: .monospaced))
                        .frame(minHeight: 120)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .scrollContentBackground(.hidden)
                        .onChange(of: secret) { _, _ in refreshPublicKey() }
                }
                SecureField("Key passphrase (if any)", text: $passphrase)
            } else {
                SecureField("Password", text: $secret)
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
                    copiedPublicKey ? "Copied" : "Copy public key",
                    systemImage: copiedPublicKey ? "checkmark" : "doc.on.doc"
                )
                .font(.subheadline)
            }
            .buttonStyle(.bordered)
            Text("Add this line to ~/.ssh/authorized_keys on the host.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 6)
    }

    private var advancedSection: some View {
        Section {
            DisclosureGroup("Advanced", isExpanded: $showAdvanced) {
                VStack(alignment: .leading, spacing: 6) {
                    TextField("Remote command", text: $host.remoteCommand)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.system(size: 13, design: .monospaced))
                    Text("Blank runs `thinkterm cli --prefer-mux proxy` on the host.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 4)
                if let known = host.knownHost {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Host key")
                            .font(.subheadline)
                        Text(known)
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .lineLimit(3)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(.vertical, 4)
                    Button(role: .destructive) {
                        host.knownHost = nil
                    } label: {
                        Label("Forget host key", systemImage: "xmark.shield")
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
        // A new host has nothing in the Keychain yet, so it needs a secret now.
        return isExisting || hasStoredSecret || !secret.isEmpty
    }

    private var keyStatus: (text: String, symbol: String, tint: Color) {
        if !secret.isEmpty {
            return host.publicKey == nil
                ? ("Key loaded", "checkmark.seal.fill", .green)
                : ("ed25519 key loaded", "checkmark.seal.fill", .green)
        }
        if hasStoredSecret { return ("Key stored", "lock.fill", .green) }
        return ("No key yet", "exclamationmark.triangle.fill", .orange)
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
                importError = "That file is not readable as text."
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
