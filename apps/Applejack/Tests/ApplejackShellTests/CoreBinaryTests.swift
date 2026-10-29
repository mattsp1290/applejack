import Foundation
import XCTest

@testable import ApplejackShell

final class CoreBinaryTests: XCTestCase {
    func testShellVersion() {
        XCTAssertEqual(ApplejackShell.version, "0.1.0")
    }

    func testCoreBinaryReportsVersion() throws {
        guard let binary = ProcessInfo.processInfo.environment["APPLEJACK_CORE_BIN"] else {
            throw XCTSkip("APPLEJACK_CORE_BIN not set; run through cargo xtask check")
        }

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
}
