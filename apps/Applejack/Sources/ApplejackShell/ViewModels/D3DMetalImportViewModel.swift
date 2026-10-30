import Foundation
import Observation

/// `engine.d3dmetal.import` behind the license acknowledgement: the request
/// is never sent until the dialog was accepted, and then always with
/// `license_acknowledged: true`.
@Observable @MainActor
public final class D3DMetalImportViewModel {
    public var volumePath: String?
    public private(set) var licenseAcknowledged = false
    public private(set) var showingLicense = false
    public private(set) var importState: FlowState<JSONValue> = .idle
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
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
        importState = .loading
        let params = EngineD3dmetalImportParams(path: volumePath, licenseAcknowledged: true)
        if let next = await app.run({ try await $0.call("engine.d3dmetal.import", params: params) as JSONValue }) {
            importState = next
        }
    }
}
