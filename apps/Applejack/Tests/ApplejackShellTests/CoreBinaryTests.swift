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
        process.waitUntilExit()

        let output = String(decoding: stdout.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        XCTAssertEqual(process.terminationStatus, 0, "\(binary) --version exited \(process.terminationStatus)")
        XCTAssertTrue(output.contains("applejack"), "unexpected --version output: \(output)")
    }
}
