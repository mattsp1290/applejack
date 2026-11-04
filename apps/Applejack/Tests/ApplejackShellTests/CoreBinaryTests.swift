import Foundation
import XCTest

@testable import ApplejackShell

final class CoreBinaryTests: XCTestCase {
    func testShellVersion() {
        XCTAssertEqual(ApplejackShell.version, "0.1.0")
    }

    func coreBinary() throws -> String {
        guard let binary = ProcessInfo.processInfo.environment["APPLEJACK_CORE_BIN"] else {
            throw XCTSkip("APPLEJACK_CORE_BIN not set; run through cargo xtask check")
        }
        return binary
    }

    func testCoreBinaryReportsVersion() throws {
        let binary = try coreBinary()

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binary)
        process.arguments = ["--version"]
        let stdout = Pipe()
        process.standardOutput = stdout
        try process.run()
        // Drain the pipe before waiting so a full pipe buffer cannot deadlock the child.
        let output = String(decoding: stdout.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        process.waitUntilExit()

        XCTAssertEqual(process.terminationStatus, 0, "\(binary) --version exited \(process.terminationStatus)")
        XCTAssertEqual(
            output.trimmingCharacters(in: .whitespacesAndNewlines),
            "applejack 0.1.0",
            "unexpected --version output: \(output)"
        )
    }

    func testCoreBinaryServesHelloAndHostDoctor() async throws {
        let binary = try coreBinary()
        let client = CoreClient(executable: URL(fileURLWithPath: binary))

        let hello = try await client.hello()
        XCTAssertEqual(hello.protocolVersion, 1)
        XCTAssertEqual(hello.coreVersion, "0.1.0")

        let facts = try await client.hostDoctor()
        XCTAssertFalse(facts.macosVersion.isEmpty)
        XCTAssertEqual(facts.screenRecording, .unknown)

        await client.shutdown()
        let restarts = await client.restarts
        XCTAssertEqual(restarts, 0)
    }

    /// The real core on an empty data directory, with the network denied
    /// (standing order 13: no harness reaches Steam): the client runtime
    /// reads `absent` and there are no sessions. Nothing is installed.
    func testCoreBinaryAnswersRuntimeStatusAndSessionsOffline() async throws {
        let binary = try coreBinary()
        let dataDir = FileManager.default.temporaryDirectory.appendingPathComponent("applejack-u16-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dataDir) }
        let client = CoreClient(
            executable: URL(fileURLWithPath: "/usr/bin/sandbox-exec"),
            arguments: ["-p", "(version 1)(allow default)(deny network*)", binary, "serve", "--stdio"],
            environment: ["APPLEJACK_DATA_DIR": dataDir.path]
        )
        let outcome = try await withTimeout(.seconds(20)) {
            let hello = try await client.hello()
            let status: ClientRuntimeStatus = try await client.call("runtime.steam.status", params: EmptyParams())
            let sessions: SessionListResult = try await client.call("session.list", params: SessionListParams(appid: nil))
            return (hello, status, sessions)
        }
        XCTAssertEqual(outcome.0.protocolVersion, 1)
        XCTAssertEqual(outcome.1.state, .absent)
        XCTAssertFalse(outcome.1.running)
        XCTAssertNil(outcome.1.buildId)
        XCTAssertEqual(outcome.1.snapshots, 0)
        XCTAssertTrue(outcome.1.prefix.hasPrefix(dataDir.path) || outcome.1.prefix.hasPrefix("/private" + dataDir.path), outcome.1.prefix)
        XCTAssertEqual(outcome.2.sessions, [])
        await client.shutdown()
        let contents = try FileManager.default.contentsOfDirectory(atPath: dataDir.path)
        XCTAssertFalse(contents.contains("prefixes"), "a status read creates no prefix: \(contents)")
    }

    private struct TimedOut: Error {}

    private func withTimeout<T: Sendable>(_ limit: Duration, _ body: @escaping @Sendable () async throws -> T) async throws -> T {
        try await withThrowingTaskGroup(of: T.self) { group in
            group.addTask { try await body() }
            group.addTask {
                try await Task.sleep(for: limit)
                throw TimedOut()
            }
            defer { group.cancelAll() }
            return try await group.next()!
        }
    }
}
