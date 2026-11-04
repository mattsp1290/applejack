import Foundation
import Observation

/// One `job.progress` line as the runtime section lists it.
public struct ProgressLine: Equatable, Sendable, Identifiable {
    public var id: Int
    public var phase: String
    public var message: String

    public init(id: Int, phase: String, message: String) {
        self.id = id
        self.phase = phase
        self.message = message
    }

    public var text: String { message.isEmpty ? phase : "\(phase): \(message)" }
}

/// `runtime.steam.status` and the four runtime jobs. Every `job.progress`
/// line of the running action is kept, not just the latest: the `start`
/// phase carries the macOS microphone warning, and a line the user did not
/// see in time is one the client has already stalled on.
@Observable @MainActor
public final class SteamClientRuntimeViewModel {
    public enum Action: String, Sendable {
        case install = "Install"
        case update = "Update"
        case repair = "Repair"
        case reinstall = "Reinstall"
        case stop = "Stop"

        var method: String {
            switch self {
            case .install: "runtime.steam.install"
            case .update: "runtime.steam.update"
            case .repair, .reinstall: "runtime.steam.repair"
            case .stop: "runtime.steam.stop"
            }
        }
    }

    public static let prefix = "steam-client"

    public private(set) var status: FlowState<ClientRuntimeStatus> = .idle
    public private(set) var action: Action?
    public private(set) var job: FlowState<JobSnapshot> = .idle
    public private(set) var progress: [ProgressLine] = []
    public let snapshots: PrefixSnapshotsViewModel
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
        snapshots = PrefixSnapshotsViewModel(app: app, prefix: Self.prefix)
    }

    public var isBusy: Bool {
        job.isLoading || (job.value.map { !$0.finished } ?? false)
    }

    /// Whether `action` can do anything against the status read last:
    /// Install always (it is the resume of an interrupted one), Stop only
    /// while the client runs, the rest only once a prefix exists.
    public func canPerform(_ action: Action) -> Bool {
        guard !isBusy else { return false }
        switch action {
        case .install: return true
        case .stop: return status.value?.running == true
        case .update, .repair, .reinstall:
            guard let state = status.value?.state else { return false }
            return state != .absent
        }
    }

    public func load() async {
        status = .loading
        if let next = await app.run({ try await $0.call("runtime.steam.status", params: EmptyParams()) as ClientRuntimeStatus }) {
            status = next
        } else {
            status = .idle
        }
    }

    /// Runs `action` as a job, one at a time, and reads the status again
    /// when it ends, however it ended.
    public func perform(_ action: Action) async {
        guard !isBusy else { return }
        self.action = action
        progress = []
        switch action {
        case .install, .update, .stop:
            await app.runJob(action.method, params: EmptyParams()) { track($0) }
        case .repair:
            await app.runJob(action.method, params: RuntimeSteamRepairParams(reinstall: false)) { track($0) }
        case .reinstall:
            await app.runJob(action.method, params: RuntimeSteamRepairParams(reinstall: true)) { track($0) }
        }
        if case .loading = job { job = .idle }
        await load()
    }

    private func track(_ state: FlowState<JobSnapshot>) {
        job = state
        guard let snapshot = state.value, !snapshot.finished else { return }
        let line = ProgressLine(id: progress.count, phase: snapshot.phase, message: snapshot.message)
        if snapshot.phase.isEmpty { return }
        if let last = progress.last, last.phase == line.phase, last.message == line.message { return }
        progress.append(line)
    }
}
