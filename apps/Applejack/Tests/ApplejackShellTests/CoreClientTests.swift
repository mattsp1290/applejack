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

    func makeClient(environment: [String: String] = [:], protocolVersion: UInt32 = 1) -> CoreClient {
        CoreClient(
            executable: Self.fakeCore,
            arguments: [],
            environment: environment,
            protocolVersion: protocolVersion
        )
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
