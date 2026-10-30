import Foundation
import Observation

/// `engine.d3dmetal.import` behind the license acknowledgement: the request
/// is never sent until the dialog was accepted, then always with
/// `license_acknowledged: true`, and one acceptance covers one request.
@Observable @MainActor
public final class D3DMetalImportViewModel {
    public private(set) var volumePath: String?
    /// Set by the dialog, consumed by the import that follows it, and dropped
    /// when the volume changes: a new volume carries its own `License.pdf`.
    public internal(set) var licenseAcknowledged = false
    public private(set) var showingLicense = false
    public private(set) var importState: FlowState<JSONValue> = .idle
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    /// Apple's `License.pdf` on the chosen volume, the file the
    /// acknowledgement refers to (plan 02, "D3DMetal user supply").
    public var licenseURL: URL? {
        volumePath.map { URL(fileURLWithPath: $0).appendingPathComponent("License.pdf") }
    }

    public func chooseVolume(_ path: String?) {
        guard path != volumePath else { return }
        volumePath = path
        licenseAcknowledged = false
    }

    /// Imports if the license was acknowledged; otherwise shows the dialog.
    public func requestImport() async {
        guard volumePath != nil else { return }
        guard licenseAcknowledged else {
            showingLicense = true
            return
        }
        await runImport()
    }

    public func acknowledgeLicense() async {
        licenseAcknowledged = true
        showingLicense = false
        await runImport()
    }

    public func declineLicense() {
        licenseAcknowledged = false
        showingLicense = false
    }

    private func runImport() async {
        guard let volumePath, licenseAcknowledged else { return }
        licenseAcknowledged = false
        importState = .loading
        let params = EngineD3dmetalImportParams(path: volumePath, licenseAcknowledged: true)
        if let next = await app.run({ try await $0.call("engine.d3dmetal.import", params: params) as JSONValue }) {
            importState = next
        }
    }
}
