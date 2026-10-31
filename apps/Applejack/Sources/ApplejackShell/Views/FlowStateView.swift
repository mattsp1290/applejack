import SwiftUI

/// Switches on a `FlowState`: `ready` renders `content`, `unavailable` the
/// "not available in this build" affordance, `failed` the error and a retry.
public struct FlowStateView<Value: Equatable & Sendable, Content: View>: View {
    let state: FlowState<Value>
    let idleMessage: String
    let retry: (@MainActor () -> Void)?
    let content: (Value) -> Content

    public init(
        _ state: FlowState<Value>,
        idle idleMessage: String = "Nothing loaded yet.",
        retry: (@MainActor () -> Void)? = nil,
        @ViewBuilder content: @escaping (Value) -> Content
    ) {
        self.state = state
        self.idleMessage = idleMessage
        self.retry = retry
        self.content = content
    }

    public var body: some View {
        switch state {
        case .idle:
            Text(idleMessage).foregroundStyle(.secondary)
        case .loading:
            ProgressView().controlSize(.small)
        case .ready(let value):
            content(value)
        case .unavailable(let reason):
            UnavailableView(reason: reason)
        case .failed(let error):
            FailureView(error: error, retry: retry)
        }
    }
}

/// The core answered `NotImplemented`: the flow lands with a later work
/// package. Shown instead of an error so a pending method never reads as a
/// crash.
public struct UnavailableView: View {
    let reason: Unavailable

    public init(reason: Unavailable) {
        self.reason = reason
    }

    public var body: some View {
        ContentUnavailableView {
            Label("Not available in this build", systemImage: "hammer")
        } description: {
            if let method = reason.method {
                Text("`\(method)`: \(reason.message)")
            } else {
                Text(reason.message)
            }
        }
    }
}

public struct FailureView: View {
    let error: CoreClientError
    let retry: (@MainActor () -> Void)?

    public init(error: CoreClientError, retry: (@MainActor () -> Void)?) {
        self.error = error
        self.retry = retry
    }

    public var body: some View {
        ContentUnavailableView {
            Label("Failed", systemImage: "exclamationmark.triangle")
        } description: {
            Text(describe(error))
        } actions: {
            if let retry {
                Button("Retry", action: retry)
            }
        }
    }
}

/// One line per `CoreClientError` case, for banners and failure views.
public func describe(_ error: CoreClientError) -> String {
    switch error {
    case .launchFailed(let detail): "The core could not be launched: \(detail)"
    case .restartLimit: "The core exited too many times; quit and reopen Applejack."
    case .coreExited(let status): "The core exited with status \(status) while this request was in flight."
    case .notRunning: "The core is not running."
    case .handshakeTimedOut: "The core did not answer the handshake in time."
    case .protocolMismatch(let rpc): "Shell and core disagree on the protocol: \(rpc.message)"
    case .rpc(let rpc): "\(rpc.message) (code \(rpc.code))"
    case .malformedResponse(let detail): "The core's answer could not be read: \(detail)"
    case .shutDown: "The core was shut down."
    }
}

/// Progress of one job: phase, bytes when known, and the terminal states.
public struct JobProgressView: View {
    let state: FlowState<JobSnapshot>
    let title: String

    public init(_ state: FlowState<JobSnapshot>, title: String) {
        self.state = state
        self.title = title
    }

    public var body: some View {
        FlowStateView(state, idle: "") { snapshot in
            VStack(alignment: .leading, spacing: 4) {
                if snapshot.finished {
                    Label("\(title) finished", systemImage: "checkmark.circle")
                } else {
                    if let fraction = snapshot.fraction {
                        ProgressView(value: fraction) { Text("\(title): \(snapshot.phase)") }
                    } else {
                        ProgressView { Text("\(title): \(snapshot.phase)") }
                    }
                }
                if !snapshot.message.isEmpty {
                    Text(snapshot.message).font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}
