import Foundation
import Observation

/// The config editor for one title: `config.get`, `config.set` per key,
/// preset import with the launch-override confirmation, `config.export`,
/// `app.run` with its session followed, `app.cancel`, and the title's
/// prefix snapshots. One instance per title lives in `AppModel`.
@Observable @MainActor
public final class GameDetailViewModel {
    /// A preset whose launch overrides wait for the user's answer. The alert
    /// is presented with this value, so its confirm button carries the path
    /// even after the dismissal cleared `pendingImport`.
    public struct PendingImport: Equatable, Sendable {
        public var path: String
        public var presetAppid: UInt32
        public var confirmations: [ImportItem]
        public var rejected: [RejectedItem]

        public init(path: String, presetAppid: UInt32, confirmations: [ImportItem], rejected: [RejectedItem]) {
            self.path = path
            self.presetAppid = presetAppid
            self.confirmations = confirmations
            self.rejected = rejected
        }
    }

    public let appid: UInt32
    public private(set) var config: FlowState<GameConfig> = .idle
    /// `config.resolve` while `hidraw_enabled` is unset: the value a run
    /// would apply and why. `idle` once the row stores one, whose stored
    /// value is its effective value.
    public private(set) var hidrawDefault: FlowState<ConfigResolveResult> = .idle
    public private(set) var lastSave: FlowState<JSONValue> = .idle
    public private(set) var importState: FlowState<ConfigImportResult> = .idle
    /// Set while a preset's launch overrides wait for confirmation.
    public private(set) var pendingImport: PendingImport?
    public private(set) var runState: FlowState<AppRunResult> = .idle
    /// The run's job: the session's phases, then `finished` once the row
    /// closed, or `failed` with the session's reason.
    public private(set) var session: FlowState<JobSnapshot> = .idle
    /// An open `sessions` row for this title that this model did not start
    /// (another core, the CLI, or an earlier app run), from `session.list`.
    public private(set) var openSession: SessionEntry?
    public private(set) var cancelState: FlowState<JobSnapshot> = .idle
    public private(set) var exportState: FlowState<ConfigExportResult> = .idle
    public let snapshots: PrefixSnapshotsViewModel
    private let app: AppModel
    private var follow: Task<Void, Never>?
    /// The `sessions` row of the run this model follows, from the `app.run`
    /// answer, until the follow ends. The core inserts the row before it
    /// answers, so `session.list` can show it before `session` is ready.
    private var ownSessionId: Int64?
    /// Counts `config.import` previews so a slow one answered after a newer
    /// preset's preview cannot replace that preset's pending plan.
    private var importToken = 0

    /// The launch overrides waiting for confirmation; empty when none.
    public var pendingConfirmation: [ImportItem] { pendingImport?.confirmations ?? [] }

    public init(app: AppModel, appid: UInt32) {
        self.app = app
        self.appid = appid
        snapshots = PrefixSnapshotsViewModel(app: app, prefix: String(appid))
    }

    /// Whether a session this model started is still open.
    public var isRunning: Bool {
        guard case .ready(let snapshot) = session else { return false }
        return !snapshot.finished
    }

    /// A cancel is in flight from its request until its job ends; the core
    /// accepts a second `app.cancel` for the same row, so the model refuses it.
    private var isCancelling: Bool {
        cancelState.isLoading || (cancelState.value.map { !$0.finished } ?? false)
    }

    /// Stop is offered for a session this model started, and for an open
    /// row `session.list` showed (the view confirms that one first).
    public var canCancel: Bool {
        !isCancelling && (isRunning || openSession != nil)
    }

    public func load() async {
        config = .loading
        if let next = await app.run({ try await $0.call("config.get", params: ConfigGetParams(appid: appid)) as GameConfig }) {
            config = next
        }
        await loadHidrawDefault()
        await loadSessions()
    }

    /// The text under an unset `hidraw_enabled` toggle, once `config.resolve`
    /// answered: `Default: off, DualSense connected`.
    public var hidrawDefaultText: String? {
        hidrawDefault.value?.defaultText
    }

    /// `config.resolve` is asked only while the row leaves `hidraw_enabled`
    /// unset: a stored value needs no resolving, and the read costs a
    /// `system_profiler` run in the core.
    private func loadHidrawDefault() async {
        guard let stored = config.value, stored.hidrawEnabled == nil else {
            hidrawDefault = .idle
            return
        }
        hidrawDefault = .loading
        if let next = await app.run({ try await $0.call("config.resolve", params: ConfigResolveParams(appid: appid)) as ConfigResolveResult }) {
            hidrawDefault = next
        }
    }

    /// `session.list` for this title; an open row that is not this model's
    /// run becomes `openSession`. While `app.run` is unanswered the list is
    /// left alone: the row it shows may be the start's own, whose id is
    /// not known yet.
    public func loadSessions() async {
        guard let next = await app.run({ try await $0.call("session.list", params: SessionListParams(appid: appid)) as SessionListResult }) else {
            return
        }
        guard case .ready(let list) = next, !runState.isLoading else { return }
        openSession = list.sessions.first { $0.endedAt == nil && $0.id != ownSessionId }
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
    /// in flight or while its session is open sends nothing. Returns once
    /// the core answered: a ready result names the session's log, which the
    /// view hands to Logs; a refusal leaves `runState` failed. The request
    /// and the run's job run on a task of the model's own, followed in
    /// `session` until the row closes, so cancelling the caller (which no
    /// view does) returns at the core's answer and the run goes on.
    public func run() async {
        if case .loading = runState { return }
        if isRunning { return }
        runState = .loading
        session = .idle
        cancelState = .idle
        ownSessionId = nil
        await withCheckedContinuation { (answered: CheckedContinuation<Void, Never>) in
            let answer = Once(answered)
            follow = Task { [weak self] in
                defer { answer.resume() }
                guard let self else { return }
                await app.runJob(
                    "app.run", params: AppRunParams(appid: appid), jobId: { JobId(rawValue: $0.jobId) },
                    onStarted: { state in
                        runState = state
                        if let started = state.value {
                            ownSessionId = started.id
                            if openSession?.id == started.id { openSession = nil }
                        }
                        if !state.isLoading { answer.resume() }
                    },
                    onChange: { session = $0 }
                )
                if case .loading = runState { runState = .idle }
                // A follow that ended without a terminal event was cancelled;
                // the row, if still open, is `session.list`'s to show.
                if isRunning { session = .idle }
                ownSessionId = nil
                await loadSessions()
            }
        }
    }

    /// `app.cancel` for this title, followed to its end; `session.list` is
    /// read again afterwards, so a row the cancel closed leaves `openSession`.
    public func cancel(killPrefix: Bool = false) async {
        if isCancelling { return }
        await app.runJob("app.cancel", params: AppCancelParams(appid: appid, killPrefix: killPrefix)) { cancelState = $0 }
        if case .loading = cancelState { cancelState = .idle }
        await loadSessions()
    }

    /// `config.export` to `path`; the core applies the export denylist.
    public func export(to path: String) async {
        exportState = .loading
        let params = ConfigExportParams(appid: appid, path: path)
        if let next = await app.run({ try await $0.call("config.export", params: params) as ConfigExportResult }) {
            exportState = next
        } else {
            exportState = .idle
        }
    }

    /// The warning for a preset exported for another title, which is the
    /// easy way to slip a foreign executable and arguments past the user.
    public func presetMismatch(presetAppid: UInt32) -> String? {
        presetAppid == appid ? nil : "This preset was exported for app \(presetAppid), not this game."
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
                pendingImport = PendingImport(
                    path: path, presetAppid: result.presetAppid, confirmations: result.confirmations, rejected: result.rejected)
            } else if result.applied {
                await load()
            }
        }
    }
}

/// Resumes a continuation once, whichever of the answer and the task's end
/// comes first.
@MainActor
private final class Once {
    private var continuation: CheckedContinuation<Void, Never>?

    init(_ continuation: CheckedContinuation<Void, Never>) {
        self.continuation = continuation
    }

    func resume() {
        continuation?.resume()
        continuation = nil
    }
}
