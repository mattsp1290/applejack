import Foundation

public enum CoreClientError: Error, Equatable, Sendable {
    /// `Process.run` failed; the path or the binary is wrong.
    case launchFailed(String)
    /// The core exited more than `maxRestarts` times; no further spawn is attempted.
    case restartLimit
    /// The core exited with this status while the request was in flight. The
    /// outcome is unknown: the core may have acted on the request first.
    case coreExited(Int32)
    /// No core was running when the request was sent, or its stdin was closed
    /// while it still ran; no exit status was observed.
    case notRunning
    /// The core did not answer `hello` within `handshakeTimeout`. It was torn
    /// down; the next call respawns it.
    case handshakeTimedOut
    /// `hello` was refused; the client is dead until recreated.
    case protocolMismatch(RpcError)
    /// The core answered with an error.
    case rpc(RpcError)
    /// The response line could not be decoded as the expected result type.
    case malformedResponse(String)
    /// `shutdown()` was called.
    case shutDown
}

/// One `job.*` notification, decoded into the `RpcTypes` mirrors.
public enum JobEvent: Equatable, Sendable {
    case progress(JobProgress)
    case done(JobDone)
    case failed(JobFailed)

    public var jobId: JobId {
        switch self {
        case .progress(let progress): progress.jobId
        case .done(let done): done.jobId
        case .failed(let failed): failed.jobId
        }
    }

    /// `job.done` and `job.failed` end a job's stream.
    public var isTerminal: Bool {
        switch self {
        case .progress: false
        case .done, .failed: true
        }
    }
}

/// What `CoreClient.events()` delivers: every job notification the core
/// sends, and every exit the client observes under its restart policy.
public enum CoreEvent: Equatable, Sendable {
    case job(JobEvent)
    case exited(Int32)
}

/// A started job and the stream of its own notifications. The stream ends
/// after `job.done` or `job.failed`, and throws `coreExited` or `shutDown`
/// when the core goes away before either arrives. A handle that is kept but
/// no longer iterated holds its subscription until one of those ends it;
/// drop the handle or cancel the consuming task to release it earlier.
public struct JobHandle: Sendable {
    public let started: JobStarted
    public let events: AsyncThrowingStream<JobEvent, any Error>

    public var jobId: JobId { started.jobId }
}

/// Drives `applejack serve --stdio`: newline-delimited JSON-RPC 2.0, requests
/// matched to responses by id, one shared `hello` per spawn, and at most
/// `maxRestarts` respawns after an unexpected exit.
///
/// The first `init` sets the process-wide `SIGPIPE` disposition to `SIG_IGN`,
/// so a write to an exited core fails with `EPIPE` instead of killing the
/// host. A host that wants its own `SIGPIPE` handler installs it afterwards.
///
/// Invariants:
/// - `generation` increases on every `spawn()` and every `tearDown()` that
///   drops a process. Every callback from a process (chunks, exit, write
///   failure, handshake deadline) carries the generation it was spawned under
///   and is ignored if it does not match. Whichever of `coreExited` and
///   `tearDown` drops the process broadcasts `.exited` (outside `shutdown()`),
///   so every job stream ends and no handle outlives its core.
/// - `process != nil` implies that `helloResult` is set or `handshake` is in
///   flight, for the same generation. A `hello` that fails for any reason,
///   including the deadline, ends in `tearDown`. The handshake is completed
///   from `deliver`, not awaited by a task, so an unanswered `hello` holds no
///   task and no reference to the client; callers wait on per-caller
///   continuations and a cancelled one is released at once.
/// - `pending` and the handshake waiters are drained by `tearDown` and
///   `coreExited`. Ids come from `nextId`, which never resets, so a late
///   answer from an old process matches no request.
/// - `restarts` counts successful respawns only and never resets; `fatal` is
///   sticky.
public actor CoreClient {
    public static let protocolVersion: UInt32 = 1

    /// How long the core may take to answer `hello` before it is torn down.
    public static let handshakeTimeout: Duration = .seconds(10)
    /// How long stdout is still read after the core exits, so an answer the
    /// core wrote on its way out is not lost.
    static let exitDrainGrace: Duration = .milliseconds(250)
    /// How long a torn-down core may keep running after its stdin closed
    /// before it is terminated.
    static let terminateGrace: Duration = .seconds(2)
    /// How long a terminated core may ignore `SIGTERM` before `SIGKILL`.
    static let killGrace: Duration = .seconds(1)

    public let executable: URL
    public let arguments: [String]
    public let environment: [String: String]
    public private(set) var restarts = 0
    /// The current process's `hello` answer; `nil` until it arrives and
    /// whenever no process is running.
    public private(set) var helloResult: HelloResult?

    private let protocolVersion: UInt32
    private let maxRestarts: Int
    private let handshakeTimeout: Duration
    private var process: Process?
    /// The current process's stdin and the queue that serializes writes to
    /// it off the actor, so a full pipe stalls only the writer. Both are per
    /// spawn: a write stuck in a dead core's pipe cannot delay the next
    /// core's `hello`.
    private var writer: StdinWriter?
    private var buffer: [UInt8] = []
    private var generation = 0
    private var nextId: Int64 = 1
    private var pending: [Int64: CheckedContinuation<Data, any Error>] = [:]
    /// The current process's `hello` while it is unanswered.
    private var handshake: Handshake?
    private var launched = false
    private var shuttingDown = false
    private var fatal: CoreClientError?
    /// Live `events()` streams and the per-job subscriptions behind
    /// `startJob`, by ticket. Each yields every `CoreEvent`; a job handle
    /// filters its own id so a notification is never lost to another job.
    private var subscribers: [Int: AsyncStream<CoreEvent>.Continuation] = [:]
    private var nextSubscriber = 0

    private struct StdinWriter {
        let handle: FileHandle
        let queue: DispatchQueue
    }

    private struct Handshake {
        let id: Int64
        /// Callers parked until `hello` is answered, by ticket.
        var waiters: [Int: CheckedContinuation<HelloResult, any Error>] = [:]
        var nextTicket = 0
        let deadline: Task<Void, Never>
    }

    public init(
        executable: URL,
        arguments: [String] = ["serve", "--stdio"],
        environment: [String: String] = [:],
        protocolVersion: UInt32 = CoreClient.protocolVersion,
        maxRestarts: Int = 3,
        handshakeTimeout: Duration = CoreClient.handshakeTimeout
    ) {
        self.executable = executable
        self.arguments = arguments
        self.environment = environment
        self.protocolVersion = protocolVersion
        self.maxRestarts = maxRestarts
        self.handshakeTimeout = handshakeTimeout
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
    /// stops the wait: a request not yet written is never sent, one already
    /// written still runs in the core, and its late response is dropped. A
    /// caller cancelled while `hello` is in flight returns at once; the
    /// handshake itself continues for the other callers.
    public func call<Params: Encodable & Sendable, Result: Decodable & Sendable>(
        _ method: String, params: Params
    ) async throws -> Result {
        _ = try await ensureRunning()
        return try await send(method, params: params)
    }

    /// Every job notification and every observed core exit from now on. The
    /// subscription is registered before this returns, so an event the core
    /// sends after the call cannot be missed. The stream ends on `shutdown()`.
    public func events() -> AsyncStream<CoreEvent> {
        let (stream, continuation) = AsyncStream<CoreEvent>.makeStream()
        let ticket = addSubscriber(continuation)
        continuation.onTermination = { [weak self] _ in
            guard let self else { return }
            Task { await self.removeSubscriber(ticket) }
        }
        return stream
    }

    /// Sends a request whose result is `JobStarted` and returns the stream of
    /// that job's notifications. The subscription is registered before the
    /// request is written, so a `job.progress` the core emits right after
    /// answering is delivered even if it beats the response on the wire.
    public func startJob<Params: Encodable & Sendable>(_ method: String, params: Params) async throws -> JobHandle {
        _ = try await ensureRunning()
        let (all, allContinuation) = AsyncStream<CoreEvent>.makeStream()
        let ticket = addSubscriber(allContinuation)
        let started: JobStarted
        do {
            started = try await send(method, params: params)
        } catch {
            removeSubscriber(ticket)
            throw error
        }
        let jobId = started.jobId
        let (events, eventsContinuation) = AsyncThrowingStream<JobEvent, any Error>.makeStream()
        let forwarder = Task {
            for await event in all {
                switch event {
                case .job(let job) where job.jobId == jobId:
                    eventsContinuation.yield(job)
                    if job.isTerminal {
                        eventsContinuation.finish()
                        return
                    }
                case .job:
                    continue
                case .exited(let status):
                    eventsContinuation.finish(throwing: CoreClientError.coreExited(status))
                    return
                }
            }
            // The subscription ended without a terminal event: `shutdown()`.
            eventsContinuation.finish(throwing: CoreClientError.shutDown)
        }
        eventsContinuation.onTermination = { [weak self] _ in
            forwarder.cancel()
            guard let self else { return }
            Task { await self.removeSubscriber(ticket) }
        }
        return JobHandle(started: started, events: events)
    }

    /// Closes the core's stdin, which ends its loop with exit 0, and
    /// terminates it if it is still running after `terminateGrace`.
    public func shutdown() {
        shuttingDown = true
        tearDown(failing: .shutDown)
        let open = subscribers
        subscribers = [:]
        for continuation in open.values {
            continuation.finish()
        }
    }

    // MARK: - Events

    private func addSubscriber(_ continuation: AsyncStream<CoreEvent>.Continuation) -> Int {
        let ticket = nextSubscriber
        nextSubscriber += 1
        subscribers[ticket] = continuation
        return ticket
    }

    private func removeSubscriber(_ ticket: Int) {
        subscribers.removeValue(forKey: ticket)?.finish()
    }

    private func broadcast(_ event: CoreEvent) {
        for continuation in subscribers.values {
            continuation.yield(event)
        }
    }

    private func deliverNotification(method: String, line: Data) {
        struct Envelope<Params: Decodable>: Decodable {
            var params: Params
        }
        func decode<Params: Decodable>(_: Params.Type) -> Params? {
            (try? RpcCoding.decoder().decode(Envelope<Params>.self, from: line))?.params
        }
        let event: JobEvent?
        switch method {
        case JobProgress.method: event = decode(JobProgress.self).map(JobEvent.progress)
        case JobDone.method: event = decode(JobDone.self).map(JobEvent.done)
        case JobFailed.method: event = decode(JobFailed.self).map(JobEvent.failed)
        default:
            CoreClient.log("dropped notification `\(method)` that no view handles")
            return
        }
        guard let event else {
            CoreClient.log("dropped `\(method)` notification whose params do not decode")
            return
        }
        broadcast(.job(event))
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
            try spawn()
            // Counted after the spawn: a launch failure is not a restart.
            if launched {
                restarts += 1
            }
            launched = true
            do {
                try startHandshake()
            } catch {
                tearDown(failing: .notRunning)
                throw error
            }
        }
        if let helloResult { return helloResult }
        return try await awaitHandshake()
    }

    private func startHandshake() throws {
        guard let writer else { throw CoreClientError.notRunning }
        let id = nextId
        nextId += 1
        let generation = self.generation
        let line = try CoreClient.encodeLine(
            OutgoingRequest(id: .number(id), method: "hello", params: HelloParams(protocolVersion: protocolVersion))
        )
        let timeout = handshakeTimeout
        let deadline = Task.detached { [weak self] in
            do {
                try await Task.sleep(for: timeout)
            } catch {
                return
            }
            await self?.handshakeTimedOut(generation: generation)
        }
        handshake = Handshake(id: id, deadline: deadline)
        enqueue(line, on: writer, generation: generation)
    }

    private func awaitHandshake() async throws -> HelloResult {
        guard var current = handshake else { throw CoreClientError.notRunning }
        let ticket = current.nextTicket
        current.nextTicket += 1
        handshake = current
        let generation = self.generation
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                if Task.isCancelled {
                    continuation.resume(throwing: CancellationError())
                } else if handshake == nil {
                    continuation.resume(throwing: CoreClientError.notRunning)
                } else {
                    handshake?.waiters[ticket] = continuation
                }
            }
        } onCancel: {
            Task { await self.cancelHandshakeWait(ticket: ticket, generation: generation) }
        }
    }

    private func cancelHandshakeWait(ticket: Int, generation: Int) {
        // Tickets restart with every handshake; only this generation's count.
        guard generation == self.generation else { return }
        handshake?.waiters.removeValue(forKey: ticket)?.resume(throwing: CancellationError())
    }

    private func completeHandshake(with line: Data) {
        let result: HelloResult
        do {
            result = try CoreClient.decodeResult(from: line)
        } catch CoreClientError.rpc(let error) where error.errorCode == .protocolMismatch {
            fatal = .protocolMismatch(error)
            tearDown(failing: .protocolMismatch(error))
            return
        } catch {
            // A core that cannot be greeted serves nothing; the next call respawns it.
            tearDown(failing: error as? CoreClientError ?? .malformedResponse(String(describing: error)))
            return
        }
        helloResult = result
        guard let handshake else { return }
        self.handshake = nil
        handshake.deadline.cancel()
        for waiter in handshake.waiters.values {
            waiter.resume(returning: result)
        }
    }

    private func handshakeTimedOut(generation: Int) {
        guard generation == self.generation, handshake != nil else { return }
        tearDown(failing: .handshakeTimedOut)
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
            throw CoreClientError.launchFailed("\(executable.path): \(error.localizedDescription)")
        }
        self.process = process
        writer = StdinWriter(
            handle: stdin.fileHandleForWriting,
            queue: DispatchQueue(label: "applejack.core-client.stdin.\(generation)")
        )
        helloResult = nil
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
        if let method = envelope.method {
            deliverNotification(method: method, line: line)
            return
        }
        guard case .number(let id)? = envelope.id else {
            // The core could not read the id of some request line. The call
            // that sent it keeps waiting, because nothing says which one.
            let detail = envelope.error.map { "\($0.code) \($0.message)" } ?? "no error member"
            CoreClient.log("response with id \(String(describing: envelope.id)) matches no request: \(detail)")
            return
        }
        if let handshake, handshake.id == id {
            completeHandshake(with: line)
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
        writer = nil
        helloResult = nil
        buffer = []
        failPending(with: shuttingDown ? .shutDown : .coreExited(status))
        if !shuttingDown {
            broadcast(.exited(status))
        }
    }

    /// Drops the current process and fails every in-flight request. Closing
    /// stdin ends a healthy core's loop with exit 0; one that lingers past
    /// `terminateGrace` is terminated, and killed `killGrace` later if it
    /// ignores that too.
    private func tearDown(failing error: CoreClientError) {
        var dropped: Int32?
        if let process {
            generation += 1
            if let writer {
                // Queued after any request still being written.
                writer.queue.async { try? writer.handle.close() }
            }
            writer = nil
            self.process = nil
            helloResult = nil
            buffer = []
            // -1 while it still runs: its real status arrives under the old
            // generation and is never reported.
            dropped = process.isRunning ? -1 : process.terminationStatus
            Task.detached {
                try? await Task.sleep(for: CoreClient.terminateGrace)
                guard process.isRunning else { return }
                process.terminate()
                try? await Task.sleep(for: CoreClient.killGrace)
                if process.isRunning {
                    kill(process.processIdentifier, SIGKILL)
                }
            }
        }
        failPending(with: error)
        // The generation bump makes `coreExited` ignore this spawn, so the
        // exit is announced here or never.
        if let dropped, !shuttingDown {
            broadcast(.exited(dropped))
        }
    }

    private func failPending(with error: CoreClientError) {
        let waiting = pending
        pending = [:]
        for continuation in waiting.values {
            continuation.resume(throwing: error)
        }
        if let handshake {
            self.handshake = nil
            handshake.deadline.cancel()
            for waiter in handshake.waiters.values {
                waiter.resume(throwing: error)
            }
        }
    }

    // MARK: - Requests

    /// The request envelope as written to the core; `GoldenTests` pins its
    /// `hello` bytes to the golden.
    struct OutgoingRequest<Params: Encodable>: Encodable {
        var jsonrpc = RpcVersion.text
        var id: RpcId
        var method: String
        var params: Params
    }

    static func encodeLine<Params: Encodable>(_ request: OutgoingRequest<Params>) throws -> Data {
        var encoded = try RpcCoding.encoder().encode(request)
        encoded.append(UInt8(ascii: "\n"))
        return encoded
    }

    private func send<Params: Encodable & Sendable, Result: Decodable & Sendable>(
        _ method: String, params: Params
    ) async throws -> Result {
        try Task.checkCancellation()
        guard let writer, process != nil else { throw CoreClientError.notRunning }
        let id = nextId
        nextId += 1
        let data = try CoreClient.encodeLine(OutgoingRequest(id: .number(id), method: method, params: params))
        let generation = self.generation

        let line: Data = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                if Task.isCancelled {
                    continuation.resume(throwing: CancellationError())
                    return
                }
                // Registered before the write so a fast answer finds its caller.
                pending[id] = continuation
                enqueue(data, on: writer, generation: generation)
            }
        } onCancel: {
            Task { await self.cancel(id: id) }
        }
        return try CoreClient.decodeResult(from: line)
    }

    /// Queues one line for the core's stdin. A failed write (`EPIPE`) means
    /// the core is gone; it is reported on the actor under its generation.
    private func enqueue(_ data: Data, on writer: StdinWriter, generation: Int) {
        writer.queue.async { [weak self] in
            do {
                try writer.handle.write(contentsOf: data)
            } catch {
                guard let self else { return }
                Task { await self.writeFailed(generation: generation) }
            }
        }
    }

    private static func decodeResult<Result: Decodable & Sendable>(from line: Data) throws -> Result {
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
        tearDown(failing: process.isRunning ? .notRunning : .coreExited(process.terminationStatus))
    }

    private func cancel(id: Int64) {
        pending.removeValue(forKey: id)?.resume(throwing: CancellationError())
    }

    private static func log(_ message: String) {
        FileHandle.standardError.write(Data("applejack core client: \(message)\n".utf8))
    }

    /// A write to a core that already exited must surface as an error on the
    /// write, not kill the shell. Process-wide; see the actor doc comment.
    private static let sigpipeIgnored: Bool = {
        signal(SIGPIPE, SIG_IGN)
        return true
    }()

    private static func ignoreSigpipe() {
        _ = sigpipeIgnored
    }
}
