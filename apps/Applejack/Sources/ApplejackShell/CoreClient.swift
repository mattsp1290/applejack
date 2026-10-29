import Foundation

public enum CoreClientError: Error, Equatable, Sendable {
    /// `Process.run` failed; the path or the binary is wrong.
    case launchFailed(String)
    /// The core exited more than `maxRestarts` times; no further spawn is attempted.
    case restartLimit
    /// The core exited with this status while the request was in flight.
    case coreExited(Int32)
    /// `hello` was refused; the client is dead until recreated.
    case protocolMismatch(RpcError)
    /// The core answered with an error.
    case rpc(RpcError)
    /// The response line could not be decoded as the expected result type.
    case malformedResponse(String)
    /// `shutdown()` was called.
    case shutDown
}

/// Drives `applejack serve --stdio`: newline-delimited JSON-RPC 2.0, requests
/// matched to responses by id, `hello` on every spawn, and at most
/// `maxRestarts` respawns after an unexpected exit.
public actor CoreClient {
    public static let protocolVersion: UInt32 = 1

    public let executable: URL
    public let arguments: [String]
    public let environment: [String: String]
    public private(set) var restarts = 0
    public private(set) var helloResult: HelloResult?

    private let protocolVersion: UInt32
    private let maxRestarts: Int
    private var process: Process?
    private var input: FileHandle?
    private var buffer: [UInt8] = []
    private var generation = 0
    private var nextId: Int64 = 1
    private var pending: [Int64: CheckedContinuation<Data, any Error>] = [:]
    private var launched = false
    private var shuttingDown = false
    private var fatal: CoreClientError?

    public init(
        executable: URL,
        arguments: [String] = ["serve", "--stdio"],
        environment: [String: String] = [:],
        protocolVersion: UInt32 = CoreClient.protocolVersion,
        maxRestarts: Int = 3
    ) {
        self.executable = executable
        self.arguments = arguments
        self.environment = environment
        self.protocolVersion = protocolVersion
        self.maxRestarts = maxRestarts
        CoreClient.ignoreSigpipe()
    }

    /// Spawns the core if needed and returns its `hello` answer.
    public func hello() async throws -> HelloResult {
        try await ensureRunning()
        guard let helloResult else { throw CoreClientError.malformedResponse("hello result missing") }
        return helloResult
    }

    public func hostDoctor() async throws -> HostFacts {
        try await call("host.doctor", params: EmptyParams())
    }

    /// Sends one request and waits for its response. Cancelling the task
    /// abandons the request: nothing is sent to the core and a late
    /// response is dropped.
    public func call<Params: Encodable & Sendable, Result: Decodable & Sendable>(
        _ method: String, params: Params
    ) async throws -> Result {
        try await ensureRunning()
        return try await send(method, params: params)
    }

    /// Closes the core's stdin, which ends its loop with exit 0.
    public func shutdown() {
        shuttingDown = true
        tearDown(failing: .shutDown)
    }

    deinit {
        process?.terminate()
    }

    // MARK: - Lifecycle

    private func ensureRunning() async throws {
        if let fatal { throw fatal }
        if shuttingDown { throw CoreClientError.shutDown }
        if process != nil { return }
        if launched {
            guard restarts < maxRestarts else {
                fatal = .restartLimit
                throw CoreClientError.restartLimit
            }
            restarts += 1
        }
        launched = true
        try spawn()
        do {
            helloResult = try await send("hello", params: HelloParams(protocolVersion: protocolVersion))
        } catch CoreClientError.rpc(let error) where error.errorCode == .protocolMismatch {
            fatal = .protocolMismatch(error)
            tearDown(failing: .protocolMismatch(error))
            throw CoreClientError.protocolMismatch(error)
        }
    }

    private func spawn() throws {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        if !environment.isEmpty {
            process.environment = ProcessInfo.processInfo.environment.merging(environment) { $1 }
        }
        let stdin = Pipe()
        let stdout = Pipe()
        process.standardInput = stdin
        process.standardOutput = stdout
        // stderr is inherited: the core's `--log` lines belong to the shell's stderr.

        generation += 1
        let generation = self.generation
        let (stream, continuation) = AsyncStream<Data>.makeStream()
        let reader = stdout.fileHandleForReading
        // Chunks are yielded from the handler in arrival order and consumed by
        // one task, so reassembly never sees them reordered.
        reader.readabilityHandler = { handle in
            let data = handle.availableData
            if data.isEmpty {
                handle.readabilityHandler = nil
                continuation.finish()
            } else {
                continuation.yield(data)
            }
        }
        // `waitUntilExit` spins a run loop and can hang on a cooperative
        // thread; the exit status arrives through this handler instead.
        let (exits, exitContinuation) = AsyncStream<Int32>.makeStream()
        process.terminationHandler = { finished in
            exitContinuation.yield(finished.terminationStatus)
            exitContinuation.finish()
        }
        do {
            try process.run()
        } catch {
            reader.readabilityHandler = nil
            continuation.finish()
            exitContinuation.finish()
            throw CoreClientError.launchFailed(String(describing: error))
        }
        self.process = process
        self.input = stdin.fileHandleForWriting
        buffer = []
        Task.detached { [weak self] in
            for await chunk in stream {
                await self?.receive(chunk, generation: generation)
            }
            // stdout is closed; a core still running has nothing left to say.
            await self?.stdoutClosed(generation: generation)
            var status: Int32 = -1
            for await exitStatus in exits {
                status = exitStatus
            }
            await self?.coreExited(generation: generation, status: status)
        }
    }

    private func receive(_ chunk: Data, generation: Int) {
        guard generation == self.generation else { return }
        buffer.append(contentsOf: chunk)
        while let newline = buffer.firstIndex(of: UInt8(ascii: "\n")) {
            let line = Data(buffer[buffer.startIndex..<newline])
            buffer.removeSubrange(buffer.startIndex...newline)
            deliver(line)
        }
    }

    private func deliver(_ line: Data) {
        struct Envelope: Decodable {
            var id: RpcId?
        }
        guard let envelope = try? RpcCoding.decoder().decode(Envelope.self, from: line),
            case .number(let id)? = envelope.id,
            let continuation = pending.removeValue(forKey: id)
        else {
            // A notification (no id), a cancelled request's late answer, or
            // a line that is not JSON: nothing is waiting for it.
            return
        }
        continuation.resume(returning: line)
    }

    private func stdoutClosed(generation: Int) {
        guard generation == self.generation, let process, process.isRunning else { return }
        process.terminate()
    }

    private func coreExited(generation: Int, status: Int32) {
        guard generation == self.generation else { return }
        process = nil
        input = nil
        buffer = []
        failPending(with: shuttingDown ? .shutDown : .coreExited(status))
    }

    /// Drops the current process and fails every in-flight request. Closing
    /// stdin ends a healthy core's loop with exit 0; nothing waits for it.
    private func tearDown(failing error: CoreClientError) {
        if process != nil {
            generation += 1
            try? input?.close()
            input = nil
            process = nil
            buffer = []
        }
        failPending(with: error)
    }

    private func failPending(with error: CoreClientError) {
        let waiting = pending
        pending = [:]
        for continuation in waiting.values {
            continuation.resume(throwing: error)
        }
    }

    // MARK: - Requests

    private struct OutgoingRequest<Params: Encodable>: Encodable {
        var jsonrpc = RpcVersion.text
        var id: RpcId
        var method: String
        var params: Params
    }

    private func send<Params: Encodable & Sendable, Result: Decodable & Sendable>(
        _ method: String, params: Params
    ) async throws -> Result {
        guard let input, let process else { throw CoreClientError.coreExited(-1) }
        let id = nextId
        nextId += 1
        var data = try RpcCoding.encoder().encode(OutgoingRequest(id: .number(id), method: method, params: params))
        data.append(UInt8(ascii: "\n"))
        do {
            try input.write(contentsOf: data)
        } catch {
            // EPIPE: the core is gone and the request never reached it.
            let status = process.isRunning ? -1 : process.terminationStatus
            tearDown(failing: .coreExited(status))
            throw CoreClientError.coreExited(status)
        }

        let line: Data = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                pending[id] = continuation
                if Task.isCancelled {
                    pending.removeValue(forKey: id)
                    continuation.resume(throwing: CancellationError())
                }
            }
        } onCancel: {
            Task { await self.cancel(id: id) }
        }

        let response: RpcTypedResponse<Result>
        do {
            response = try RpcCoding.decoder().decode(RpcTypedResponse<Result>.self, from: line)
        } catch {
            throw CoreClientError.malformedResponse(String(describing: error))
        }
        if let error = response.error {
            throw CoreClientError.rpc(error)
        }
        guard let result = response.result else {
            throw CoreClientError.malformedResponse("response has neither result nor error")
        }
        return result
    }

    private func cancel(id: Int64) {
        pending.removeValue(forKey: id)?.resume(throwing: CancellationError())
    }

    /// A write to a core that already exited must surface as an error on the
    /// write, not kill the shell.
    private static let sigpipeIgnored: Bool = {
        signal(SIGPIPE, SIG_IGN)
        return true
    }()

    private static func ignoreSigpipe() {
        _ = sigpipeIgnored
    }
}
