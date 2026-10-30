import Foundation
import XCTest

@testable import ApplejackShell

final class CoreClientTests: XCTestCase {
    static let fakeCore = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .appendingPathComponent("Fixtures")
        .appendingPathComponent("fake-core.sh")

    struct Echo: Codable, Equatable, Sendable {
        var tag: String
        var delayMs: Int?

        enum CodingKeys: String, CodingKey {
            case tag
            case delayMs = "delay_ms"
        }
    }

    func makeClient(
        environment: [String: String] = [:],
        protocolVersion: UInt32 = 1,
        handshakeTimeout: Duration = CoreClient.handshakeTimeout
    ) -> CoreClient {
        CoreClient(
            executable: Self.fakeCore,
            arguments: [],
            environment: environment,
            protocolVersion: protocolVersion,
            handshakeTimeout: handshakeTimeout
        )
    }

    func makePidFile() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("fake-core-\(UUID().uuidString).pid")
    }

    func readPid(_ pidFile: URL) throws -> Int32 {
        let text = try String(contentsOf: pidFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
        return try XCTUnwrap(Int32(text), text)
    }

    func waitUntilGone(_ pid: Int32, within limit: Duration = .seconds(5)) async throws {
        let deadline = ContinuousClock.now + limit
        while processState(pid) != nil, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(100))
        }
    }

    func testFakeCoreIsExecutable() {
        XCTAssertTrue(FileManager.default.isExecutableFile(atPath: Self.fakeCore.path), Self.fakeCore.path)
    }

    func testPartialLinesReassemble() async throws {
        let client = makeClient(environment: ["FAKE_CORE_SPLIT": "1"])
        let hello = try await client.hello()
        XCTAssertEqual(hello, HelloResult(protocolVersion: 1, coreVersion: "fake"))
        let facts = try await client.hostDoctor()
        XCTAssertEqual(facts.macosVersion, "26.0")
        XCTAssertEqual(facts.freeDiskBytes, 123_456_789)
        XCTAssertEqual(facts.screenRecording, .unknown)
        await client.shutdown()
    }

    func testConcurrentRequestsGetTheirOwnResponses() async throws {
        let client = makeClient()
        _ = try await client.hello()
        // The slow echo is sent first and answered last; a FIFO client would
        // hand its answer to the fast request.
        async let slow: Echo = client.call("fake.echo", params: Echo(tag: "slow", delayMs: 300))
        async let fast: Echo = client.call("fake.echo", params: Echo(tag: "fast", delayMs: nil))
        let (slowResult, fastResult) = try await (slow, fast)
        XCTAssertEqual(slowResult.tag, "slow")
        XCTAssertEqual(fastResult.tag, "fast")
        await client.shutdown()
    }

    func testUnknownMethodSurfacesAsRpcError() async throws {
        let client = makeClient()
        do {
            let _: JSONValue = try await client.call("engine.list", params: EmptyParams())
            XCTFail("expected an error")
        } catch CoreClientError.rpc(let error) {
            XCTAssertEqual(error.errorCode, .notImplemented)
            XCTAssertEqual(error.data, .object(["method": .string("engine.list")]))
        }
        await client.shutdown()
    }

    func testCrashRestartCapSurfacesRestartLimit() async throws {
        // Each process answers hello plus one request, then dies on the next.
        let client = makeClient(environment: ["FAKE_CORE_CRASH_AFTER": "2"])
        _ = try await client.hostDoctor()

        for restart in 1...3 {
            await assertCoreExited(client)
            _ = try await client.hostDoctor()
            let restarts = await client.restarts
            XCTAssertEqual(restarts, restart)
        }

        await assertCoreExited(client)
        do {
            _ = try await client.hostDoctor()
            XCTFail("the fourth restart must be refused")
        } catch CoreClientError.restartLimit {
        }
        let restarts = await client.restarts
        XCTAssertEqual(restarts, 3)
        await client.shutdown()
    }

    func testProtocolMismatchIsTyped() async {
        let client = makeClient(protocolVersion: 99)
        do {
            _ = try await client.hostDoctor()
            XCTFail("expected a protocol mismatch")
        } catch CoreClientError.protocolMismatch(let error) {
            XCTAssertEqual(error.code, RpcErrorCode.protocolMismatch.rawValue)
            XCTAssertEqual(error.data, .object(["expected": .integer(1), "received": .integer(99)]))
        } catch {
            XCTFail("unexpected error \(error)")
        }
        // The client stays dead rather than respawning into the same mismatch.
        do {
            _ = try await client.hostDoctor()
            XCTFail("expected the mismatch to persist")
        } catch CoreClientError.protocolMismatch {
        } catch {
            XCTFail("unexpected error \(error)")
        }
        await client.shutdown()
    }

    func testCancellationAbandonsTheRequest() async throws {
        let client = makeClient()
        _ = try await client.hello()
        let task = Task {
            let _: Echo = try await client.call("fake.echo", params: Echo(tag: "late", delayMs: 500))
        }
        task.cancel()
        do {
            try await task.value
            XCTFail("expected cancellation")
        } catch is CancellationError {
        }
        // The late answer is dropped and the client keeps working.
        let facts = try await client.hostDoctor()
        XCTAssertEqual(facts.arch, "arm64")
        await client.shutdown()
    }

    func testConcurrentHellosShareOneHandshake() async throws {
        // The split fake answers slowly, so both callers arrive mid-handshake.
        let client = makeClient(environment: ["FAKE_CORE_SPLIT": "1"])
        async let first = client.hello()
        async let second = client.hello()
        let (firstResult, secondResult) = try await (first, second)
        XCTAssertEqual(firstResult, HelloResult(protocolVersion: 1, coreVersion: "fake"))
        XCTAssertEqual(firstResult, secondResult)
        let restarts = await client.restarts
        XCTAssertEqual(restarts, 0)
        await client.shutdown()
    }

    func testCancelledCallDuringHandshakeDoesNotPoisonLaterCalls() async throws {
        let client = makeClient(environment: ["FAKE_CORE_SPLIT": "1"])
        let task = Task { try await client.hostDoctor() }
        // The split hello takes 200 ms; cancel while it is in flight.
        try await Task.sleep(for: .milliseconds(50))
        task.cancel()
        do {
            _ = try await task.value
            XCTFail("expected cancellation")
        } catch is CancellationError {
        }
        let hello = try await client.hello()
        XCTAssertEqual(hello.coreVersion, "fake")
        let facts = try await client.hostDoctor()
        XCTAssertEqual(facts.arch, "arm64")
        let restarts = await client.restarts
        XCTAssertEqual(restarts, 0, "the cancelled handshake must not have torn the core down")
        await client.shutdown()
    }

    func testExitIsDetectedWhileAGrandchildHoldsStdout() async throws {
        let client = makeClient(environment: ["FAKE_CORE_ORPHAN": "1"])
        let start = ContinuousClock.now
        do {
            _ = try await client.hello()
            XCTFail("expected the core to exit")
        } catch CoreClientError.coreExited(let status) {
            XCTAssertEqual(status, 3)
        }
        let elapsed = ContinuousClock.now - start
        XCTAssertLessThan(elapsed, .seconds(2), "the exit must surface before the grandchild releases stdout (5 s)")
        await client.shutdown()
    }

    func testLaunchFailureDoesNotSpendARestart() async {
        let client = CoreClient(executable: URL(fileURLWithPath: "/nonexistent/applejack"), arguments: [])
        for _ in 1...2 {
            do {
                _ = try await client.hello()
                XCTFail("expected a launch failure")
            } catch CoreClientError.launchFailed {
            } catch {
                XCTFail("unexpected error \(error)")
            }
        }
        let restarts = await client.restarts
        XCTAssertEqual(restarts, 0)
    }

    func testShutdownTerminatesACoreThatIgnoresEof() async throws {
        let pidFile = makePidFile()
        defer { try? FileManager.default.removeItem(at: pidFile) }
        let client = makeClient(environment: ["FAKE_CORE_IGNORE_EOF": "1", "FAKE_CORE_PID_FILE": pidFile.path])
        _ = try await client.hello()
        let pid = try readPid(pidFile)
        XCTAssertNotNil(processState(pid))

        await client.shutdown()
        try await waitUntilGone(pid)
        // An empty `ps` state means terminated and reaped; a zombie would show `Z`.
        XCTAssertNil(processState(pid), "a core that ignores EOF must be terminated and reaped after the grace period")
    }

    func testHandshakeTimeoutTearsDownAMuteCoreAndReleasesTheClient() async throws {
        let pidFile = makePidFile()
        defer { try? FileManager.default.removeItem(at: pidFile) }
        var client: CoreClient? = makeClient(
            environment: ["FAKE_CORE_MUTE": "1", "FAKE_CORE_PID_FILE": pidFile.path],
            handshakeTimeout: .milliseconds(500)
        )
        weak var released = client
        let start = ContinuousClock.now
        do {
            _ = try await client?.hello()
            XCTFail("expected the handshake to time out")
        } catch CoreClientError.handshakeTimedOut {
        }
        XCTAssertLessThan(ContinuousClock.now - start, .seconds(3), "hello must fail at the deadline")
        let pid = try readPid(pidFile)
        try await waitUntilGone(pid)
        XCTAssertNil(processState(pid), "a mute core must be torn down")
        let helloResult = await client?.helloResult
        XCTAssertNil(helloResult)

        // Nothing but the caller holds the client: no handshake task, no
        // strong capture in the reader or the deadline.
        client = nil
        let releaseDeadline = ContinuousClock.now + .seconds(2)
        while released != nil, ContinuousClock.now < releaseDeadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertNil(released, "the client must deallocate once its last caller drops it")
    }

    func testCancelledCallerReturnsWhileTheCoreIsMute() async throws {
        let client = makeClient(environment: ["FAKE_CORE_MUTE": "1"], handshakeTimeout: .seconds(5))
        let task = Task { try await client.hostDoctor() }
        try await Task.sleep(for: .milliseconds(100))
        let start = ContinuousClock.now
        task.cancel()
        do {
            _ = try await task.value
            XCTFail("expected cancellation")
        } catch is CancellationError {
        }
        XCTAssertLessThan(ContinuousClock.now - start, .seconds(1), "a cancelled caller must not wait for the handshake")
        await client.shutdown()
    }

    func testBackgroundEchoDoesNotInterleaveWithSplitWrites() async throws {
        // The fast answer is written in two halves 200 ms apart; the slow one
        // lands in between and must wait for the writer.
        let client = makeClient(environment: ["FAKE_CORE_SPLIT": "1"])
        _ = try await client.hello()
        async let slow: Echo = client.call("fake.echo", params: Echo(tag: "slow", delayMs: 100))
        async let fast: Echo = client.call("fake.echo", params: Echo(tag: "fast", delayMs: nil))
        let (slowResult, fastResult) = try await (slow, fast)
        XCTAssertEqual(slowResult.tag, "slow")
        XCTAssertEqual(fastResult.tag, "fast")
        await client.shutdown()
    }

    func testNestedIdInParamsIsNotMistakenForTheRequestId() async throws {
        let client = makeClient()
        let params: JSONValue = .object(["inner": .object(["id": .integer(99)]), "tag": .string("nested")])
        let echoed: JSONValue = try await client.call("fake.echo", params: params)
        XCTAssertEqual(echoed, params)
        await client.shutdown()
    }

    func testJobNotificationsReachTheirHandleOnly() async throws {
        let client = makeClient(environment: ["FAKE_CORE_IMPLEMENTED": "1"])
        let events = await client.events()
        async let first = client.startJob("engine.install", params: EngineInstallParams(engineId: "rosetta-wine"))
        async let second = client.startJob("app.install", params: AppInstallParams(appid: 220))
        let (engine, app) = try await (first, second)
        XCTAssertEqual(engine.jobId, JobId(rawValue: 7))
        XCTAssertEqual(app.jobId, JobId(rawValue: 8))

        var engineEvents: [JobEvent] = []
        for try await event in engine.events {
            engineEvents.append(event)
        }
        XCTAssertEqual(engineEvents.count, 3)
        XCTAssertTrue(engineEvents.allSatisfy { $0.jobId == engine.jobId })
        XCTAssertEqual(engineEvents.last, .done(JobDone(jobId: engine.jobId)))
        var appEvents: [JobEvent] = []
        for try await event in app.events {
            appEvents.append(event)
        }
        XCTAssertEqual(appEvents.map(\.jobId), Array(repeating: app.jobId, count: 3))

        // The unfiltered stream saw all six, interleaved or not.
        var all: [CoreEvent] = []
        for await event in events {
            all.append(event)
            if all.count == 6 { break }
        }
        XCTAssertEqual(all.filter { if case .job = $0 { true } else { false } }.count, 6)
        await client.shutdown()
    }

    func testJobStreamThrowsWhenTheCoreExits() async throws {
        // One answer (hello), then the job start; the next request kills the fake.
        let client = makeClient(environment: ["FAKE_CORE_IMPLEMENTED": "1", "FAKE_CORE_CRASH_AFTER": "2", "FAKE_CORE_JOB_OUTCOME": "never"])
        let handle = try await client.startJob("engine.install", params: EngineInstallParams(engineId: "rosetta-wine"))
        await assertCoreExited(client)
        do {
            for try await _ in handle.events {}
            XCTFail("the job stream must end with the exit")
        } catch CoreClientError.coreExited(let status) {
            XCTAssertEqual(status, 1)
        }
        await client.shutdown()
    }

    func testEventsStreamEndsOnShutdown() async throws {
        let client = makeClient()
        _ = try await client.hello()
        let events = await client.events()
        await client.shutdown()
        var count = 0
        for await _ in events {
            count += 1
        }
        XCTAssertEqual(count, 0)
    }

    /// The process's `ps` state, or `nil` once it is gone. A zombie answers
    /// `kill(pid, 0)` and shows as `Z`, so the state is what the tests check.
    private func processState(_ pid: Int32) -> String? {
        let ps = Process()
        ps.executableURL = URL(fileURLWithPath: "/bin/ps")
        ps.arguments = ["-o", "stat=", "-p", "\(pid)"]
        let output = Pipe()
        ps.standardOutput = output
        guard (try? ps.run()) != nil else { return "ps failed" }
        let stat = String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        ps.waitUntilExit()
        return stat.isEmpty ? nil : stat
    }

    private func assertCoreExited(_ client: CoreClient, file: StaticString = #filePath, line: UInt = #line) async {
        do {
            _ = try await client.hostDoctor()
            XCTFail("expected the core to exit", file: file, line: line)
        } catch CoreClientError.coreExited(let status) {
            XCTAssertEqual(status, 1, file: file, line: line)
        } catch {
            XCTFail("unexpected error \(error)", file: file, line: line)
        }
    }
}
