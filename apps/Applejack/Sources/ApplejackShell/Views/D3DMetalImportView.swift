import SwiftUI
import UniformTypeIdentifiers

public struct D3DMetalImportView: View {
    let model: D3DMetalImportViewModel
    @State private var choosingVolume = false

    public init(model: D3DMetalImportViewModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Copies the D3DMetal payload from a mounted Game Porting Toolkit image into the engine's per-user overlay. Applejack never downloads or ships it.")
                .foregroundStyle(.secondary)
            HStack {
                Button("Choose volume…") { choosingVolume = true }
                Text(model.volumePath ?? "No volume chosen").foregroundStyle(.secondary)
            }
            Button("Import D3DMetal") { Task { await model.requestImport() } }
                .disabled(model.volumePath == nil)
            FlowStateView(model.importState, idle: "") { _ in
                Label("D3DMetal imported.", systemImage: "checkmark.circle")
            }
            Spacer()
        }
        .padding()
        .navigationTitle("D3DMetal")
        .fileImporter(isPresented: $choosingVolume, allowedContentTypes: [.folder]) { result in
            if case .success(let url) = result {
                model.volumePath = url.path
            }
        }
        .sheet(isPresented: Binding(get: { model.showingLicense }, set: { if !$0 { model.declineLicense() } })) {
            LicenseSheet(model: model)
        }
    }
}

/// Apple's license must be accepted before the import request is sent.
struct LicenseSheet: View {
    let model: D3DMetalImportViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Game Porting Toolkit license").font(.headline)
            Text("D3DMetal is Apple's, distributed under the Game Porting Toolkit license for non-commercial use. By continuing you confirm that you downloaded the toolkit from Apple under your own developer account and accept its license terms. Applejack records that you accepted them.")
            HStack {
                Spacer()
                Button("Cancel") { model.declineLicense() }
                Button("I accept") { Task { await model.acknowledgeLicense() } }.keyboardShortcut(.defaultAction)
            }
        }
        .padding()
        .frame(width: 440)
    }
}
