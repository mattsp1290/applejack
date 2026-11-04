import Foundation
import Observation

/// `prefix.snapshot`, `prefix.restore`, and `prefix.reset` for one prefix.
/// There is no listing method, so the restore picker offers the timestamps
/// this model made (snapshots, and the safety snapshots a restore or reset
/// reported) plus whatever the user types.
@Observable @MainActor
public final class PrefixSnapshotsViewModel {
    public let prefix: String
    public private(set) var snapshot: FlowState<SnapshotResult> = .idle
    public private(set) var restore: FlowState<SnapshotResult> = .idle
    public private(set) var reset: FlowState<JobSnapshot> = .idle
    /// Timestamps seen this session, newest first.
    public private(set) var knownTimestamps: [String] = []
    public var restoreTimestamp = ""
    private let app: AppModel

    public init(app: AppModel, prefix: String) {
        self.app = app
        self.prefix = prefix
    }

    public var isBusy: Bool {
        snapshot.isLoading || restore.isLoading || reset.isLoading || (reset.value.map { !$0.finished } ?? false)
    }

    public func takeSnapshot() async {
        snapshot = .loading
        if let next = await app.run({ try await $0.call("prefix.snapshot", params: PrefixSnapshotParams(prefix: prefix)) as SnapshotResult }) {
            snapshot = next
            if let made = next.value {
                remember(made.timestamp)
            }
        } else {
            snapshot = .idle
        }
    }

    /// Restores `restoreTimestamp`, trimmed; an empty one sends nothing.
    public func restoreSelected() async {
        let timestamp = restoreTimestamp.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !timestamp.isEmpty else { return }
        await restore(timestamp: timestamp)
    }

    public func restore(timestamp: String) async {
        restore = .loading
        let params = PrefixRestoreParams(prefix: prefix, timestamp: timestamp)
        if let next = await app.run({ try await $0.call("prefix.restore", params: params) as SnapshotResult }) {
            restore = next
            // The answer is the safety snapshot of what the restore replaced.
            if let safety = next.value {
                remember(safety.timestamp)
            }
        } else {
            restore = .idle
        }
    }

    public func resetPrefix(keepUserData: Bool) async {
        let params = PrefixResetParams(prefix: prefix, keepUserData: keepUserData)
        await app.runJob("prefix.reset", params: params) { state in
            reset = state
            if let timestamp = state.value.flatMap({ Self.timestamp(in: $0.message) }) {
                remember(timestamp)
            }
        }
        if case .loading = reset { reset = .idle }
        if let timestamp = resetFailureSnapshot {
            remember(timestamp)
        }
    }

    /// The snapshot a failed reset names (`applejack prefix restore <name>
    /// <timestamp>` in its message), which `restore(timestamp:)` takes.
    public var resetFailureSnapshot: String? {
        guard case .failed(.rpc(let error)) = reset else { return nil }
        return Self.timestamp(in: error.message)
    }

    /// The first snapshot name in `text`: `20261031T120000Z`, or
    /// `20261031T120000Z-001` when the core took two in one second.
    static func timestamp(in text: String) -> String? {
        guard let range = text.firstRange(of: /[0-9]{8}T[0-9]{6}Z(?:-[0-9]{3})?/) else { return nil }
        return String(text[range])
    }

    private func remember(_ timestamp: String) {
        knownTimestamps.removeAll { $0 == timestamp }
        knownTimestamps.insert(timestamp, at: 0)
    }
}
