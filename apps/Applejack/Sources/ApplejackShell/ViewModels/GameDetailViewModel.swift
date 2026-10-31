import Foundation
import Observation

/// The config editor for one title: `config.get`, `config.set` per key,
/// preset import with the launch-override confirmation, and `app.run`.
@Observable @MainActor
public final class GameDetailViewModel {
    /// A preset whose launch overrides wait for the user's answer. The alert
    /// is presented with this value, so its confirm button carries the path
    /// even after the dismissal cleared `pendingImport`.
    public struct PendingImport: Equatable, Sendable {
        public var path: String
        public var confirmations: [ImportItem]
        public var rejected: [RejectedItem]

        public init(path: String, confirmations: [ImportItem], rejected: [RejectedItem]) {
            self.path = path
            self.confirmations = confirmations
            self.rejected = rejected
        }
    }

    public let appid: UInt32
    public private(set) var config: FlowState<GameConfig> = .idle
    public private(set) var lastSave: FlowState<JSONValue> = .idle
    public private(set) var importState: FlowState<ConfigImportResult> = .idle
    /// Set while a preset's launch overrides wait for confirmation.
    public private(set) var pendingImport: PendingImport?
    public private(set) var runState: FlowState<AppRunResult> = .idle
    private let app: AppModel
    /// Counts `config.import` previews so a slow one answered after a newer
    /// preset's preview cannot replace that preset's pending plan.
    private var importToken = 0

    /// The launch overrides waiting for confirmation; empty when none.
    public var pendingConfirmation: [ImportItem] { pendingImport?.confirmations ?? [] }

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
        pendingImport = nil
        await runImport(path: path, acceptLaunchOverrides: false)
    }

    /// Applies the preset at `path` with its launch overrides. The path is a
    /// parameter, not read from `pendingImport`: SwiftUI writes `false` to
    /// the alert's `isPresented` binding, which declines, before the confirm
    /// button's task runs.
    public func confirmLaunchOverrides(path: String) async {
        pendingImport = nil
        await runImport(path: path, acceptLaunchOverrides: true)
    }

    /// Confirms the import waiting in `pendingImport`, if any.
    public func confirmLaunchOverrides() async {
        guard let pending = pendingImport else { return }
        await confirmLaunchOverrides(path: pending.path)
    }

    /// Drops the pending import. Nothing pending means a confirm already
    /// took it, and the alert's dismissal must not undo that. The preview
    /// stays in `importState` so the accepted and rejected items remain on
    /// screen; a ready result with confirmations and nothing pending reads
    /// as declined.
    public func declineLaunchOverrides() {
        guard pendingImport != nil else { return }
        pendingImport = nil
    }

    /// `app.run` for this title, one at a time: a second call while one is
    /// in flight sends nothing. A ready result names the session's log,
    /// which the view hands to Logs.
    public func run() async {
        if case .loading = runState { return }
        runState = .loading
        if let next = await app.run({ try await $0.call("app.run", params: AppRunParams(appid: appid)) as AppRunResult }) {
            runState = next
        }
    }

    private func runImport(path: String, acceptLaunchOverrides: Bool) async {
        importToken += 1
        let token = importToken
        importState = .loading
        let params = ConfigImportParams(appid: appid, path: path, acceptLaunchOverrides: acceptLaunchOverrides)
        guard let next = await app.run({ try await $0.call("config.import", params: params) as ConfigImportResult }),
            token == importToken
        else { return }
        importState = next
        if case .ready(let result) = next {
            if result.needsConfirmation {
                pendingImport = PendingImport(path: path, confirmations: result.confirmations, rejected: result.rejected)
            } else if result.applied {
                await load()
            }
        }
    }
}
