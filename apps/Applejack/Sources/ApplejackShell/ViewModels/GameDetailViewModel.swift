import Foundation
import Observation

/// The config editor for one title: `config.get`, `config.set` per key, and
/// preset import with the launch-override confirmation.
@Observable @MainActor
public final class GameDetailViewModel {
    public let appid: UInt32
    public private(set) var config: FlowState<GameConfig> = .idle
    public private(set) var lastSave: FlowState<JSONValue> = .idle
    public private(set) var importState: FlowState<ConfigImportResult> = .idle
    /// Launch overrides the preset carries; the import is applied only after
    /// `confirmLaunchOverrides()`.
    public private(set) var pendingConfirmation: [ImportItem] = []
    private var pendingPresetPath: String?
    private let app: AppModel

    public init(app: AppModel, appid: UInt32) {
        self.app = app
        self.appid = appid
    }

    public func load() async {
        config = .loading
        if let next = await app.run({ try await $0.call("config.get", params: ConfigGetParams(appid: appid)) as GameConfig }) {
            config = next
        }
    }

    public func set(_ key: ConfigKey, to value: JSONValue) async {
        lastSave = .loading
        let params = ConfigSetParams(appid: appid, key: key.rawValue, value: value)
        if let next = await app.run({ try await $0.call("config.set", params: params) as JSONValue }) {
            lastSave = next
            if case .ready = next {
                await load()
            }
        }
    }

    public func importPreset(at path: String) async {
        pendingConfirmation = []
        pendingPresetPath = nil
        await runImport(path: path, acceptLaunchOverrides: false)
    }

    public func confirmLaunchOverrides() async {
        guard let path = pendingPresetPath else { return }
        pendingConfirmation = []
        pendingPresetPath = nil
        await runImport(path: path, acceptLaunchOverrides: true)
    }

    public func declineLaunchOverrides() {
        pendingConfirmation = []
        pendingPresetPath = nil
        importState = .idle
    }

    private func runImport(path: String, acceptLaunchOverrides: Bool) async {
        importState = .loading
        let params = ConfigImportParams(appid: appid, path: path, acceptLaunchOverrides: acceptLaunchOverrides)
        guard let next = await app.run({ try await $0.call("config.import", params: params) as ConfigImportResult }) else { return }
        importState = next
        if case .ready(let result) = next {
            if result.needsConfirmation {
                pendingConfirmation = result.confirmations
                pendingPresetPath = path
            } else if result.applied {
                await load()
            }
        }
    }
}
