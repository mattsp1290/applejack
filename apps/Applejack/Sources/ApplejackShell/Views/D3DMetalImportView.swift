import AppKit
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
                    .accessibilityIdentifier("d3dmetal.chooseVolume")
                Text(model.volumePath ?? "No volume chosen").foregroundStyle(.secondary)
            }
            Button("Import D3DMetal") { Task { await model.requestImport() } }
                .disabled(model.volumePath == nil)
                .accessibilityIdentifier("d3dmetal.import")
            FlowStateView(model.importState, idle: "") { _ in
                Label("D3DMetal imported.", systemImage: "checkmark.circle")
            }
            Spacer()
        }
        .padding()
        .navigationTitle("D3DMetal")
        .fileImporter(isPresented: $choosingVolume, allowedContentTypes: [.folder]) { result in
            if case .success(let url) = result {
                model.chooseVolume(url.path)
            }
        }
        .sheet(isPresented: Binding(get: { model.showingLicense }, set: { if !$0 { model.declineLicense() } })) {
            LicenseSheet(model: model)
        }
    }
}

/// Apple's license, `License.pdf` on the volume, must be accepted before
/// each import request is sent. The sheet names the file and opens it; it
/// does not paraphrase terms the project has not read.
struct LicenseSheet: View {
    let model: D3DMetalImportViewModel

    private var licenseURL: URL? { model.licenseURL }
    private var licenseFound: Bool { model.licenseFound }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Game Porting Toolkit license").font(.headline)
            Text("D3DMetal is Apple's. Its license is the file License.pdf on the volume you chose; read it before continuing. Applejack records that you accepted it and copies the payload into the engine's per-user overlay only.")
            if let licenseURL {
                LabeledContent("License", value: licenseURL.path)
                if licenseFound {
                    Button("Open License.pdf") { NSWorkspace.shared.open(licenseURL) }
                        .accessibilityIdentifier("d3dmetal.openLicense")
                } else {
                    Text("License.pdf was not found on this volume. Choose the mounted Game Porting Toolkit image.")
                        .foregroundStyle(.red)
                }
            }
            HStack {
                Spacer()
                Button("Cancel") { model.declineLicense() }
                Button("I accept Apple's license") { Task { await model.acknowledgeLicense() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!licenseFound)
                    .accessibilityIdentifier("d3dmetal.acceptLicense")
            }
        }
        .padding()
        .frame(width: 440)
        // The volume may be mounted while the sheet is open.
        .task {
            while !Task.isCancelled {
                model.refreshLicense()
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }
}
