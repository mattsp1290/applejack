import Foundation
import Observation

/// `session.list`, then a tail of the selected session's `log_path`.
@Observable @MainActor
public final class LogsViewModel {
    public static let maxLines = 2000

    public private(set) var sessions: FlowState<SessionListResult> = .idle
    public private(set) var selected: SessionEntry?
    public private(set) var tailedPath: String?
    public private(set) var lines: [String] = []
    private var tail: Task<Void, Never>?
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    public func load(appid: UInt32? = nil) async {
        sessions = .loading
        if let next = await app.run({ try await $0.call("session.list", params: SessionListParams(appid: appid)) as SessionListResult }) {
            sessions = next
        }
    }

    public func select(_ session: SessionEntry) {
        selected = session
        if let path = session.logPath {
            follow(path)
        } else {
            stop()
        }
    }

    /// Follows `path` from its start; replaces any tail in progress.
    public func follow(_ path: String) {
        stop()
        tailedPath = path
        tail = Task { [weak self] in
            for await line in LogTailer.lines(at: path) {
                // `stop()` and `suspend()` cancel on this actor, but lines the
                // tailer already buffered still arrive; they belong to the old
                // tail, not to whatever `lines` holds now.
                guard let self, !Task.isCancelled else { return }
                lines.append(line)
                if lines.count > Self.maxLines {
                    lines.removeFirst(lines.count - Self.maxLines)
                }
            }
        }
    }

    public func stop() {
        tail?.cancel()
        tail = nil
        tailedPath = nil
        lines = []
    }

    /// Stops polling while the view is hidden; `tailedPath` stays so
    /// `resume()` can pick the file up again.
    public func suspend() {
        tail?.cancel()
        tail = nil
    }

    /// Follows `tailedPath` again, from the start, after `suspend()`.
    public func resume() {
        guard tail == nil, let path = tailedPath else { return }
        follow(path)
    }
}
