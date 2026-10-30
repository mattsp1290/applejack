import Foundation
import Observation

/// The one `CoreClient` the shell owns, the core's lifecycle as the banner
/// shows it, and the two ways a flow talks to the core: `run` for a request
/// and `runJob` for a job with notifications. `CoreClient` stays the single
/// source of truth for restarts; this model only reads its outcome.
@Observable @MainActor
public final class AppModel {
    public enum CoreStatus: Equatable, Sendable {
        /// No `applejack-core` beside the executable and no `APPLEJACK_CORE_BIN`.
        case missing
        case starting
        case running(HelloResult)
        /// The core exited with this status; the next call respawns it
        /// under the restart cap, which "Restart" triggers.
        case exited(Int32)
        /// `restartLimit`, `launchFailed`, `handshakeTimedOut`, or
        /// `protocolMismatch`: the core is unusable until the app is reopened.
        case failed(CoreClientError)
    }

    public let client: CoreClient?
    public let coreLocation: URL?
    public private(set) var coreStatus: CoreStatus
    private var exitObserver: Task<Void, Never>?

    public init(client: CoreClient?, coreLocation: URL? = nil) {
        self.client = client
        self.coreLocation = coreLocation ?? client?.executable
        coreStatus = client == nil ? .missing : .starting
    }

    public convenience init(locator: CoreLocator = CoreLocator()) {
        let location = locator.locate()
        self.init(client: location.map { CoreClient(executable: $0) }, coreLocation: location)
    }

    /// Greets the core and starts watching for exits.
    public func start() async {
        guard let client else { return }
        if exitObserver == nil {
            exitObserver = Task { [weak self] in
                for await event in await client.events() {
                    guard let self, case .exited(let status) = event else { continue }
                    if case .running = coreStatus {
                        coreStatus = .exited(status)
                    }
                }
            }
        }
        await greet(client)
    }

    /// Respawns the core (lazily, through `hello`) after an exit.
    public func restart() async {
        guard let client else { return }
        coreStatus = .starting
        await greet(client)
    }

    public func shutdown() async {
        exitObserver?.cancel()
        exitObserver = nil
        await client?.shutdown()
    }

    private func greet(_ client: CoreClient) async {
        do {
            coreStatus = .running(try await client.hello())
        } catch is CancellationError {
        } catch let error as CoreClientError {
            note(error)
        } catch {
            coreStatus = .failed(.malformedResponse(String(describing: error)))
        }
    }

    /// Records a core-level failure in the banner; an `rpc` or decode error
    /// belongs to the flow that got it, not here.
    public func note(_ error: CoreClientError) {
        switch error {
        case .coreExited(let status):
            coreStatus = .exited(status)
        case .restartLimit, .launchFailed, .handshakeTimedOut, .protocolMismatch, .notRunning:
            coreStatus = .failed(error)
        case .shutDown, .rpc, .malformedResponse:
            break
        }
    }

    /// Runs one request. `nil` when the task was cancelled, so the caller
    /// keeps its state.
    public func run<Value: Equatable & Sendable>(_ operation: (CoreClient) async throws -> Value) async -> FlowState<Value>? {
        guard let client else { return .failed(.launchFailed("no applejack-core binary")) }
        do {
            let value = try await operation(client)
            await noteHealthy(client)
            return .ready(value)
        } catch {
            if let error = error as? CoreClientError { note(error) }
            return FlowState.outcome(of: error)
        }
    }

    /// Runs a job method and reports every change of its progress through
    /// `onChange` until `job.done` (`finished`), `job.failed` (`failed` with
    /// the typed code), or the core goes away.
    public func runJob<Params: Encodable & Sendable>(
        _ method: String, params: Params, onChange: (FlowState<JobSnapshot>) -> Void
    ) async {
        guard let client else {
            onChange(.failed(.launchFailed("no applejack-core binary")))
            return
        }
        onChange(.loading)
        let handle: JobHandle
        do {
            handle = try await client.startJob(method, params: params)
        } catch {
            if let error = error as? CoreClientError { note(error) }
            if let outcome = FlowState<JobSnapshot>.outcome(of: error) { onChange(outcome) }
            return
        }
        await noteHealthy(client)
        var snapshot = JobSnapshot(jobId: handle.jobId)
        onChange(.ready(snapshot))
        do {
            for try await event in handle.events {
                switch event {
                case .progress(let progress):
                    snapshot.apply(progress)
                    onChange(.ready(snapshot))
                case .done:
                    snapshot.finished = true
                    onChange(.ready(snapshot))
                case .failed(let failed):
                    onChange(.failed(.rpc(RpcError(code: failed.code, message: failed.message))))
                }
            }
        } catch {
            if let error = error as? CoreClientError { note(error) }
            if let outcome = FlowState<JobSnapshot>.outcome(of: error) { onChange(outcome) }
        }
    }

    private func noteHealthy(_ client: CoreClient) async {
        switch coreStatus {
        case .exited, .starting:
            if let hello = await client.helloResult {
                coreStatus = .running(hello)
            }
        case .missing, .running, .failed:
            break
        }
    }
}
