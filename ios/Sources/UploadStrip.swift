import SwiftUI

/// The files on their way to the host (`TerminalModel.upload`), over the
/// keys: each with its name, how far it has got and a way to give it up;
/// once there it says so for a moment, and one that failed says why.
struct UploadStrip: View {
    @ObservedObject var model: TerminalModel

    var body: some View {
        if !model.uploads.isEmpty {
            VStack(spacing: 6) {
                ForEach(model.uploads) { upload in row(upload) }
            }
            .padding(.horizontal, 10)
            .transition(.move(edge: .bottom).combined(with: .opacity))
        }
    }

    private func row(_ upload: TerminalModel.Upload) -> some View {
        let fraction = upload.total > 0 ? Double(upload.sent) / Double(upload.total) : 0
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Image(systemName: symbol(upload))
                    .font(.system(size: 15, weight: .medium))
                    .foregroundColor(upload.error != nil ? .red : upload.path != nil ? .green : model.ink)
                Text(upload.name)
                    .font(.subheadline)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 8)
                if let error = upload.error {
                    Text(error).font(.footnote).foregroundColor(.red).lineLimit(1)
                } else if upload.path == nil {
                    Text(verbatim: "\(Int((fraction * 100).rounded(.down)))%")
                        .font(.footnote.monospacedDigit())
                        .foregroundColor(model.ink.opacity(0.7))
                    Button { model.cancelUpload(upload.id) } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 12, weight: .bold))
                            .frame(width: 30, height: 30)
                            .contentShape(Circle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(tr("cancel"))
                }
            }
            if upload.ended == nil {
                ProgressView(value: fraction)
                    .progressViewStyle(.linear)
                    .tint(.accentColor)
            }
        }
        .foregroundColor(model.ink)
        .padding(.leading, 14)
        .padding(.trailing, upload.ended == nil ? 6 : 14)
        .padding(.vertical, 8)
        .glassPanel(radius: 18)
        .accessibilityElement(children: .combine)
    }

    private func symbol(_ upload: TerminalModel.Upload) -> String {
        if upload.error != nil { return "exclamationmark.circle" }
        return upload.path != nil ? "checkmark.circle" : "arrow.up.circle"
    }
}
