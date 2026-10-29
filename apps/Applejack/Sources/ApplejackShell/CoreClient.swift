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
/// matched to responses by id, one shared `hello` per spawn, and at most
/// `maxRestarts` respawns after an unexpected exit.
public actor CoreClient {
    public static let protocolVersion: UInt32 = 1

    /// How long stdout is still read after the core exits, so an answer the
    /// core wrote on its way out is not lost.
    static let exitDrainGrace: Duration = .milliseconds(250)
    /// How long a torn-down core may keep running after its stdin closed
    /// before it is terminated.
    static let terminateGrace: Duration = .seconds(2)

    public let executable: URL
    public let arguments: [String]
    public let environment: [String: String]
    public private(set) var restarts = 0
    public private(set) var helloResult: HelloResult?

    private let protocolVersion: UInt32
    private let maxRestarts: Int
    private var process: Process?
    private var input: FileHandle?
    /// Serializes stdin writes off the actor so a full pipe stalls only the
    /// writer, never every caller.
    private let writerQueue = DispatchQueue(label: "applejack.core-client.stdin")
    private var buffer: [UInt8] = []
    private var generation = 0
    private var nextId: Int64 = 1
    private var pending: [Int64: CheckedContinuation<Data, any Error>] = [:]
    /// The `hello` of the current process; every caller awaits the same one.
    private var handshake: Task<HelloResult, any Error>?
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
    }

    public func hostDoctor() async throws -> HostFacts {
        try await call("host.doctor", params: EmptyParams())
    }

    /// Sends one request and waits for its response. Cancelling the task
    /// abandons the request: nothing is sent to the core and a late
    /// response is dropped. A cancelled caller still waits for an in-flight
    /// `hello`, which it cannot cancel.
    public func call<Params: Encodable & Sendable, Result: Decodable & Sendable>(
        _ method: String, params: Params
    ) async throws -> Result {
        _ = try await ensureRunning()
        return try await send(method, params: params)
    }

    /// Closes the core's stdin, which ends its loop with exit 0, and
    /// terminates it if it is still running after `terminateGrace`.
    public func shutdown() {
        shuttingDown = true
        tearDown(failing: .shutDown)
    }

    deinit {
        process?.terminate()
    }

    // MARK: - Lifecycle

    @discardableResult
    private func ensureRunning() async throws -> HelloResult {
        if let fatal { throw fatal }
        if shuttingDown { throw CoreClientError.shutDown }
        if process == nil {
            if launched {
                guard restarts < maxRestarts else {
                    fatal = .restartLimit
                    throw CoreClientError.restartLimit
                }
            }
            helloResult = nil
            try spawn()
            // Counted after the spawn: a launch failure is not a restart.
            if launched {
                restarts += 1
            }
            launched = true
            let generation = self.generation
            handshake = Task { try await self.performHello(generation: generation) }
        }
        guard let handshake else { throw CoreClientError.coreExited(-1) }
        return try await handshake.value
    }

    private func performHello(generation: Int) async throws -> HelloResult {
        do {
            let result: HelloResult = try await send("hello", params: HelloParams(protocolVersion: protocolVersion))
            if generation == self.generation {
                helloResult = result
            }
            return result
        } catch CoreClientError.rpc(let error) where error.errorCode == .protocolMismatch {
            fatal = .protocolMismatch(error)
            tearDown(failing: .protocolMismatch(error))
            throw CoreClientError.protocolMismatch(error)
        } catch {
            // A core that cannot be greeted serves nothing; the next call respawns it.
            if generation == self.generation {
                tearDown(failing: error as? CoreClientError ?? .coreExited(-1))
            }
            throw error
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
            // stdout reaches EOF only when every process holding it exits. A
            // grandchild that inherited it must not keep callers waiting.
            Task.detached {
                try? await Task.sleep(for: CoreClient.exitDrainGrace)
                reader.readabilityHandler = nil
                continuation.finish()
            }
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
            var method: String?
            var error: RpcError?
        }
        guard let envelope = try? RpcCoding.decoder().decode(Envelope.self, from: line) else {
            CoreClient.log("dropped \(line.count) bytes that are not a JSON message")
            return
        }
        // A notification; no caller routes them until a job method lands.
        if envelope.method != nil { return }
        guard case .number(let id)? = envelope.id else {
            // The core could not read the id of some request line. The call
            // that sent it keeps waiting, because nothing says which one.
            let detail = envelope.error.map { "\($0.code) \($0.message)" } ?? "no error member"
            CoreClient.log("response with id \(String(describing: envelope.id)) matches no request: \(detail)")
            return
        }
        // A cancelled request's late answer has nothing waiting for it.
        guard let continuation = pending.removeValue(forKey: id) else { return }
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
        handshake = nil
        buffer = []
        failPending(with: shuttingDown ? .shutDown : .coreExited(status))
    }

    /// Drops the current process and fails every in-flight request. Closing
    /// stdin ends a healthy core's loop with exit 0; one that lingers past
    /// `terminateGrace` is terminated.
    private func tearDown(failing error: CoreClientError) {
        if let process {
            generation += 1
            if let input {
                // Queued after any request still being written.
                writerQueue.async { try? input.close() }
            }
            input = nil
            self.process = nil
            handshake = nil
            buffer = []
            Task.detached {
                try? await Task.sleep(for: CoreClient.terminateGrace)
                if process.isRunning {
                    process.terminate()
                }
            }
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
        try Task.checkCancellation()
        guard let input, process != nil else { throw CoreClientError.coreExited(-1) }
        let id = nextId
        nextId += 1
        var encoded = try RpcCoding.encoder().encode(OutgoingRequest(id: .number(id), method: method, params: params))
        encoded.append(UInt8(ascii: "\n"))
        let data = encoded
        let generation = self.generation

        let line: Data = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                if Task.isCancelled {
                    continuation.resume(throwing: CancellationError())
                    return
                }
                // Registered before the write so a fast answer finds its caller.
                pending[id] = continuation
                writerQueue.async { [weak self] in
                    do {
                        try input.write(contentsOf: data)
                    } catch {
                        // EPIPE: the core is gone and the request never reached it.
                        guard let self else { return }
                        Task { await self.writeFailed(generation: generation) }
                    }
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

    private func writeFailed(generation: Int) {
        guard generation == self.generation, let process else { return }
        let status = process.isRunning ? -1 : process.terminationStatus
        tearDown(failing: .coreExited(status))
    }

    private func cancel(id: Int64) {
        pending.removeValue(forKey: id)?.resume(throwing: CancellationError())
    }

    private static func log(_ message: String) {
        FileHandle.standardError.write(Data("applejack core client: \(message)\n".utf8))
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
