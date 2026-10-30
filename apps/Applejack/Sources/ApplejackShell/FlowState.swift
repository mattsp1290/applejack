import Foundation

/// Why a flow cannot run in this build: the core answered `NotImplemented`
/// (code 1002) for `method`. The view renders this as "not available", not
/// as a failure.
public struct Unavailable: Equatable, Hashable, Sendable {
    public var method: String?
    public var message: String

    public init(method: String?, message: String) {
        self.method = method
        self.message = message
    }

    /// `nil` unless `error` is a `NotImplemented` answer.
    public init?(_ error: CoreClientError) {
        guard case .rpc(let rpc) = error, rpc.errorCode == .notImplemented else { return nil }
        var method: String?
        if case .object(let data)? = rpc.data, case .string(let name)? = data["method"] {
            method = name
        }
        self.init(method: method, message: rpc.message)
    }
}

/// The state every view switches on. `unavailable` is what `NotImplemented`
/// maps to; every other `CoreClientError` is `failed`.
public enum FlowState<Value: Equatable & Sendable>: Equatable, Sendable {
    case idle
    case loading
    case ready(Value)
    case unavailable(Unavailable)
    case failed(CoreClientError)

    public var value: Value? {
        if case .ready(let value) = self { return value }
        return nil
    }

    public var isLoading: Bool {
        if case .loading = self { return true }
        return false
    }

    public func map<Other: Equatable & Sendable>(_ transform: (Value) -> Other) -> FlowState<Other> {
        switch self {
        case .idle: .idle
        case .loading: .loading
        case .ready(let value): .ready(transform(value))
        case .unavailable(let reason): .unavailable(reason)
        case .failed(let error): .failed(error)
        }
    }

    /// `unavailable` for `NotImplemented`, `failed` for anything else.
    public static func failure(_ error: CoreClientError) -> FlowState {
        if let reason = Unavailable(error) {
            return .unavailable(reason)
        }
        return .failed(error)
    }

    /// The state `error` leaves a flow in; `nil` for cancellation, which
    /// leaves the flow where it was.
    public static func outcome(of error: any Error) -> FlowState? {
        switch error {
        case is CancellationError: nil
        case let error as CoreClientError: failure(error)
        default: .failed(.malformedResponse(String(describing: error)))
        }
    }
}

/// Progress of one job as the view shows it; `finished` after `job.done`.
public struct JobSnapshot: Equatable, Sendable {
    public var jobId: JobId
    public var phase: String
    public var doneBytes: UInt64?
    public var totalBytes: UInt64?
    public var message: String
    public var finished: Bool

    public init(jobId: JobId, phase: String = "", doneBytes: UInt64? = nil, totalBytes: UInt64? = nil, message: String = "", finished: Bool = false) {
        self.jobId = jobId
        self.phase = phase
        self.doneBytes = doneBytes
        self.totalBytes = totalBytes
        self.message = message
        self.finished = finished
    }

    /// 0...1 when the core said how many bytes there are.
    public var fraction: Double? {
        guard let doneBytes, let totalBytes, totalBytes > 0 else { return nil }
        return min(1, Double(doneBytes) / Double(totalBytes))
    }

    public mutating func apply(_ progress: JobProgress) {
        phase = progress.phase
        doneBytes = progress.doneBytes
        totalBytes = progress.totalBytes
        message = progress.message
    }
}
