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

    /// `applejack doctor --controllers` and the Swift mirror say the same
    /// words for the same inventory: the fake's four named shapes are fed to
    /// the real binary through `APPLEJACK_CONTROLLERS` and its text is
    /// compared with the lines the Controllers view builds from the fake's
    /// `host.doctor` answer; the `unavailable` knob pins the unavailable
    /// line's sentence, and the `dualsense-unavailable` shape, fed as the
    /// knob's inventory object, pins that a listed connected Sony pad
    /// decides in place of that sentence.
    func testDoctorControllersAgreesWithTheSwiftMirror() async throws {
        let binary = try coreBinary()
        let scratch = FileManager.default.temporaryDirectory.appendingPathComponent("applejack-u18d-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: scratch) }

        for shape in ["empty", "dualsense", "xbox", "dualsense-away"] {
            let fake = CoreClient(executable: CoreClientTests.fakeCore, arguments: [], environment: ["FAKE_CORE_CONTROLLERS": shape])
            let facts = try await fake.hostDoctor()
            await fake.shutdown()
            XCTAssertNil(facts.controllersUnavailable, shape)
            let knob: String
            if facts.controllers.isEmpty {
                knob = "none"
            } else {
                let file = scratch.appendingPathComponent("\(shape).json")
                try JSONEncoder().encode(facts.controllers).write(to: file)
                knob = file.path
            }
            let printed = try doctorControllers(binary, knob: knob, dataDir: scratch)
            XCTAssertEqual(printed, doctorText(facts.controllerInventory), shape)
        }

        let printed = try doctorControllers(binary, knob: "unavailable", dataDir: scratch)
        let unavailable = ControllerInventory(pads: [], unavailable: "APPLEJACK_CONTROLLERS=unavailable")
        XCTAssertEqual(printed, doctorText(unavailable))
        XCTAssertEqual(printed.count, 1, "no decision line and no `no controllers` when the read failed: \(printed)")

        let fake = CoreClient(executable: CoreClientTests.fakeCore, arguments: [], environment: ["FAKE_CORE_CONTROLLERS": "dualsense-unavailable"])
        let facts = try await fake.hostDoctor()
        await fake.shutdown()
        XCTAssertEqual(facts.controllersUnavailable, "SPUSBDataType timed out")
        XCTAssertEqual(facts.controllers.map(\.kind), [.dualsense])
        let file = scratch.appendingPathComponent("dualsense-unavailable.json")
        try JSONEncoder().encode(KnobInventory(controllers: facts.controllers, controllersUnavailable: facts.controllersUnavailable)).write(to: file)
        let decided = try doctorControllers(binary, knob: file.path, dataDir: scratch)
        XCTAssertEqual(decided, doctorText(facts.controllerInventory))
        XCTAssertEqual(decided.count, 2, "the pad's row and the decision line: \(decided)")
        XCTAssertEqual(decided.last, "hidraw disabled (DualSense connected; SPUSBDataType timed out)")
    }

    /// The `{"controllers": [...], "controllers_unavailable": ...}` shape
    /// `APPLEJACK_CONTROLLERS` accepts.
    private struct KnobInventory: Encodable {
        var controllers: [ControllerFact]
        var controllersUnavailable: String?

        enum CodingKeys: String, CodingKey {
            case controllers
            case controllersUnavailable = "controllers_unavailable"
        }
    }

    /// `render_controllers` rebuilt from the Swift words: one line per pad,
    /// `no controllers` for an empty inventory that was read, then the rule's
    /// line, or the unavailable line alone in their place.
    private func doctorText(_ inventory: ControllerInventory) -> [String] {
        var lines = inventory.pads.map { pad in
            "\(pad.name)  \(pad.transport.rawValue)  \(pad.vendorId):\(pad.productId)  \(pad.kind.rawValue)  \(pad.connected ? "connected" : "not connected")  \(pad.hidrawColumn)"
        }
        if let unavailable = inventory.unavailableLine {
            lines.append(unavailable)
        } else {
            if lines.isEmpty { lines.append("no controllers") }
            if let rule = HidrawDefault.decide(inventory) { lines.append(rule.summary) }
        }
        return lines
    }

    /// `doctor --controllers` with the inventory pinned by the knob, under
    /// the network-denied sandbox like every other run of the binary here.
    private func doctorControllers(_ binary: String, knob: String, dataDir: URL) throws -> [String] {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/sandbox-exec")
        process.arguments = ["-p", "(version 1)(allow default)(deny network*)", binary, "doctor", "--controllers"]
        process.environment = ["APPLEJACK_CONTROLLERS": knob, "APPLEJACK_DATA_DIR": dataDir.path, "PATH": "/usr/bin:/bin"]
        let stdout = Pipe()
        process.standardOutput = stdout
        try process.run()
        let output = String(decoding: stdout.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0, "doctor --controllers exited \(process.terminationStatus): \(output)")
        return output.split(separator: "\n", omittingEmptySubsequences: true).map(String.init)
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
