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
}
